pub mod relay;
pub mod signal;
#[cfg(feature = "whip")]
pub mod whip;

use std::net::IpAddr;
use std::path::PathBuf;
use std::time::SystemTime;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rand::RngCore;
use tokio::sync::{mpsc, watch};
use tracing::{info, warn};

pub use relay::DirectCommand;
pub use relay::metrics::{MetricsSnapshot, MetricsWriter, format_utc_filename_ts, prune_old_files};
pub use relay::packetize::{DirectVideoConfig, EncodedFrame, VideoCodec};

pub const DEFAULT_VIEWER_URL: &str = "https://qualterz.github.io/tiwi/watch/";
pub const DEFAULT_MQTT_URL: &str = "wss://broker.emqx.io:8084/mqtt";
pub const DEFAULT_STUN_SERVER: &str = "stun.l.google.com:19302";

/// Core configuration for running the Tiwi media relay and signaling engine.
#[derive(Debug, Clone)]
pub struct RelayConfig {
    pub token: String,
    pub viewer_url: String,
    pub mqtt_url: String,
    pub stun_server: String,
    pub public_ip: Option<IpAddr>,
    pub media_port: u16,
    pub max_peers: usize,
    pub max_uplink_mbps: f64,
    pub logs_dir: Option<PathBuf>,
}

impl Default for RelayConfig {
    fn default() -> Self {
        Self {
            token: generate_token(),
            viewer_url: DEFAULT_VIEWER_URL.to_string(),
            mqtt_url: DEFAULT_MQTT_URL.to_string(),
            stun_server: DEFAULT_STUN_SERVER.to_string(),
            public_ip: None,
            media_port: 0,
            max_peers: 0,
            max_uplink_mbps: 0.0,
            logs_dir: None,
        }
    }
}

impl RelayConfig {
    pub fn watch_url(&self) -> String {
        let base = self.viewer_url.trim_end_matches('#');
        format!("{base}#{}", self.token)
    }
}

pub fn generate_token() -> String {
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

/// Embedded Tiwi relay engine accepting encoded video/audio frames directly
/// (without WHIP HTTP/DTLS ingest), intended for native integrations like the OBS plugin.
pub struct DirectEngine {
    watch_url: String,
    direct_tx: mpsc::Sender<DirectCommand>,
    status_rx: watch::Receiver<Option<MetricsSnapshot>>,
    shutdown_tx: watch::Sender<bool>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl DirectEngine {
    pub fn start(mut config: RelayConfig) -> Result<Self, String> {
        if config.token.is_empty() {
            config.token = generate_token();
        }
        let watch_url = config.watch_url();

        let (direct_tx, direct_rx) = mpsc::channel(512);
        let (offer_tx, offer_rx) = mpsc::channel(64);
        let (signal_tx, signal_rx) = mpsc::channel(32);
        let (status_tx, status_rx) = watch::channel(None);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);

        let worker = std::thread::Builder::new()
            .name("tiwi-direct-engine".to_string())
            .spawn(move || {
                let rt = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(e) => {
                        tracing::error!(err = %e, "Failed to build direct engine runtime");
                        return;
                    }
                };

                rt.block_on(async move {
                    let run_ts = format_utc_filename_ts(SystemTime::now());
                    let metrics_writer = config.logs_dir.as_deref().and_then(|dir| {
                        match MetricsWriter::open(dir, &run_ts) {
                            Ok(w) => Some(w),
                            Err(e) => {
                                warn!(dir = %dir.display(), err = %e, "Could not open metrics file");
                                None
                            }
                        }
                    });

                    let signal_task = tokio::spawn(signal::run(
                        config.mqtt_url.clone(),
                        config.token.clone(),
                        offer_tx,
                        signal_rx,
                    ));

                    relay::run_direct(
                        config,
                        metrics_writer,
                        status_tx,
                        direct_rx,
                        offer_rx,
                        signal_tx,
                        shutdown_rx,
                    )
                    .await;

                    let _ = signal_task.await;
                    info!("Direct engine stopped cleanly");
                });
            })
            .map_err(|e| format!("Failed to spawn direct engine thread: {e}"))?;

        Ok(Self {
            watch_url,
            direct_tx,
            status_rx,
            shutdown_tx,
            worker: Some(worker),
        })
    }

    pub fn watch_url(&self) -> &str {
        &self.watch_url
    }

    /// Announces stream start and initializes the RTP packetizer for the given video codec.
    pub fn start_stream(&self, video: DirectVideoConfig) -> bool {
        self.direct_tx
            .try_send(DirectCommand::Start { video })
            .is_ok()
    }

    /// Pushes an encoded video or audio frame without blocking the caller thread.
    pub fn push_frame(&self, frame: EncodedFrame) -> bool {
        self.direct_tx.try_send(DirectCommand::Frame(frame)).is_ok()
    }

    /// Tears down all viewer sessions and announces stream stop over signaling.
    pub fn stop_stream(&self) -> bool {
        self.direct_tx.try_send(DirectCommand::Stop).is_ok()
    }

    /// Returns the most recent 1-second metrics snapshot, or `None` when idle.
    pub fn metrics(&self) -> Option<MetricsSnapshot> {
        self.status_rx.borrow().clone()
    }

    /// Gracefully shuts down the relay, flushes `StreamStop` over MQTT, and joins the worker thread.
    pub fn shutdown(mut self) {
        self.shutdown_inner();
    }

    fn shutdown_inner(&mut self) {
        let _ = self.shutdown_tx.send(true);
        if let Some(handle) = self.worker.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for DirectEngine {
    fn drop(&mut self) {
        self.shutdown_inner();
    }
}
