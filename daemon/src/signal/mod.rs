mod crypto;

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rumqttc::{AsyncClient, Event, Incoming, MqttOptions, QoS, Transport};
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot, watch};
use tracing::{debug, error, info, warn};

use self::crypto::{SeenIvs, SignalCrypto};

const REPLAY_WINDOW_MS: i64 = 60_000;

#[derive(Debug)]
pub struct ViewerOffer {
    pub viewer_id: String,
    pub sdp: String,
    pub reply: oneshot::Sender<Result<String, String>>,
}

#[derive(Debug, Clone, Copy)]
pub enum SignalEvent {
    StreamStart,
    StreamStop,
}

#[derive(Debug, Deserialize)]
struct EncryptedOfferEnvelope {
    #[serde(rename = "viewerId")]
    viewer_id: String,
    iv: String,
    data: String,
}

#[derive(Debug, Deserialize)]
struct OfferPayload {
    ts: i64,
    sdp: String,
}

#[derive(Debug, Serialize)]
struct AnswerPayload<'a> {
    sdp: &'a str,
}

#[derive(Debug, Serialize)]
struct StreamEventPayload<'a> {
    #[serde(rename = "type")]
    event_type: &'a str,
}

fn parse_mqtt_options(mqtt_url: &str, client_id: &str) -> Result<MqttOptions, String> {
    let (is_wss, rest) = if let Some(r) = mqtt_url.strip_prefix("wss://") {
        (true, r)
    } else if let Some(r) = mqtt_url.strip_prefix("ws://") {
        (false, r)
    } else {
        return Err(format!(
            "Unsupported MQTT URL '{mqtt_url}' (expected ws:// or wss://)"
        ));
    };

    let authority = rest.split('/').next().unwrap_or(rest);
    let default_port = if is_wss { 443 } else { 80 };
    let port = authority
        .rsplit_once(':')
        .and_then(|(_, p)| p.parse::<u16>().ok())
        .unwrap_or(default_port);

    // For websocket transports, rumqttc expects the full URL as the host parameter.
    let mut opts = MqttOptions::new(client_id, mqtt_url, port);
    opts.set_keep_alive(Duration::from_secs(30));
    opts.set_max_packet_size(256 * 1024, 256 * 1024);
    opts.set_transport(if is_wss {
        Transport::wss_with_default_config()
    } else {
        Transport::ws()
    });
    Ok(opts)
}

fn now_epoch_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

pub async fn run(
    mqtt_url: String,
    token: String,
    offer_tx: mpsc::Sender<ViewerOffer>,
    mut event_rx: mpsc::Receiver<SignalEvent>,
    mut shutdown_rx: watch::Receiver<bool>,
) {
    let crypto = Arc::new(SignalCrypto::from_token(&token));
    let client_id = format!(
        "tiwi-{}",
        &crypto.stream_id()[..crypto.stream_id().len().min(12)]
    );

    let opts = match parse_mqtt_options(&mqtt_url, &client_id) {
        Ok(o) => o,
        Err(e) => {
            error!(err = %e, "Signaling disabled");
            return;
        }
    };

    let (client, mut eventloop) = AsyncClient::new(opts, 64);
    let offer_topic = crypto.offer_topic();
    let events_topic = crypto.events_topic();
    let mut seen_ivs = SeenIvs::new();

    debug!(
        broker = %mqtt_url,
        stream_id = %crypto.stream_id(),
        "Connecting to MQTT signaling broker"
    );

    loop {
        tokio::select! {
            biased;

            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    let _ = client.disconnect().await;
                    break;
                }
            }

            Some(ev) = event_rx.recv() => {
                let kind = match ev {
                    SignalEvent::StreamStart => "start",
                    SignalEvent::StreamStop => "stop",
                };
                match crypto.encrypt_json(&StreamEventPayload { event_type: kind }) {
                    Ok(payload) => {
                        if let Err(e) =
                            client.try_publish(&events_topic, QoS::AtMostOnce, false, payload)
                        {
                            debug!(event = kind, err = %e, "Failed to enqueue stream event");
                        }
                    }
                    Err(e) => warn!(event = kind, err = %e, "Failed to encrypt stream event"),
                }
            }

            poll = eventloop.poll() => {
                match poll {
                    Ok(Event::Incoming(Incoming::ConnAck(_))) => {
                        info!("MQTT signaling connected");
                        let sub_client = client.clone();
                        let sub_topic = offer_topic.clone();
                        tokio::spawn(async move {
                            if let Err(e) = sub_client.subscribe(&sub_topic, QoS::AtMostOnce).await
                            {
                                warn!(err = %e, "Failed to subscribe to offer topic");
                            }
                        });
                    }
                    Ok(Event::Incoming(Incoming::Publish(p))) if p.topic == offer_topic => {
                        handle_incoming_offer(
                            &p.payload,
                            &crypto,
                            &mut seen_ivs,
                            &offer_tx,
                            &client,
                        );
                    }
                    Ok(_) => {}
                    Err(e) => {
                        warn!(err = %e, retry_secs = 2, "MQTT signaling error");
                        tokio::select! {
                            _ = tokio::time::sleep(Duration::from_secs(2)) => {}
                            _ = shutdown_rx.changed() => {
                                if *shutdown_rx.borrow() {
                                    break;
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

fn handle_incoming_offer(
    raw: &[u8],
    crypto: &Arc<SignalCrypto>,
    seen_ivs: &mut SeenIvs,
    offer_tx: &mpsc::Sender<ViewerOffer>,
    client: &AsyncClient,
) {
    // Parse outer envelope and decrypt BEFORE mutating any relay state.
    let Ok(env) = serde_json::from_slice::<EncryptedOfferEnvelope>(raw) else {
        debug!("Ignored malformed offer envelope");
        return;
    };

    if env.viewer_id.is_empty() || env.viewer_id.len() > 128 {
        debug!("Ignored offer with invalid viewerId");
        return;
    }

    let Ok(plaintext) = crypto.decrypt_bytes(&env.iv, &env.data) else {
        debug!(viewer = %env.viewer_id, "Ignored offer with invalid ciphertext");
        return;
    };

    let Ok(payload) = serde_json::from_slice::<OfferPayload>(&plaintext) else {
        debug!(viewer = %env.viewer_id, "Ignored offer with invalid decrypted JSON");
        return;
    };

    // Replay window check (+-60s) and IV deduplication.
    let delta_ms = (now_epoch_ms() - payload.ts).abs();
    if delta_ms > REPLAY_WINDOW_MS {
        debug!(
            viewer = %env.viewer_id,
            delta_ms,
            "Rejected offer outside replay window"
        );
        return;
    }

    if !seen_ivs.insert(env.iv) {
        debug!(viewer = %env.viewer_id, "Rejected replayed offer IV");
        return;
    }

    let (reply_tx, reply_rx) = oneshot::channel();
    if offer_tx
        .try_send(ViewerOffer {
            viewer_id: env.viewer_id.clone(),
            sdp: payload.sdp,
            reply: reply_tx,
        })
        .is_err()
    {
        warn!(viewer = %env.viewer_id, "Relay offer queue full, dropping offer");
        return;
    }

    let client = client.clone();
    let crypto = Arc::clone(crypto);
    let viewer_id = env.viewer_id;
    tokio::spawn(async move {
        match reply_rx.await {
            Ok(Ok(answer_sdp)) => {
                let topic = crypto.answer_topic(&viewer_id);
                match crypto.encrypt_json(&AnswerPayload { sdp: &answer_sdp }) {
                    Ok(encrypted) => {
                        if let Err(e) = client
                            .publish(&topic, QoS::AtMostOnce, false, encrypted)
                            .await
                        {
                            warn!(viewer = %viewer_id, err = %e, "Failed to publish answer");
                        }
                    }
                    Err(e) => warn!(viewer = %viewer_id, err = %e, "Failed to encrypt answer"),
                }
            }
            Ok(Err(reason)) => {
                debug!(viewer = %viewer_id, reason = %reason, "Offer rejected by relay");
            }
            Err(_) => {
                debug!(viewer = %viewer_id, "Relay dropped answer channel");
            }
        }
    });
}
