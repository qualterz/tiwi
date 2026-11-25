mod metrics;
mod net;

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use str0m::change::SdpOffer;
use str0m::format::Codec;
use str0m::media::{KeyframeRequestKind, MediaKind, Mid, Pt};
use str0m::net::{Protocol, Receive};
use str0m::rtp::{RtpPacket, RtpWrite, SeqNo, Ssrc};
use str0m::{Candidate, Event, IceConnectionState, Input, Output, Rtc, RtcConfig};
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, watch};
use tracing::{debug, error, info, trace, warn};

use self::metrics::RelayMetrics;
use self::net::{bind_shared_udp, gather_local_candidates};
use crate::config::Config;
use crate::signal::{SignalEvent, ViewerOffer};
use crate::whip::{WhipAccepted, WhipCommand, WhipError};

const VIEWER_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(20);
const PLI_COOLDOWN: Duration = Duration::from_millis(500);
const STATS_LOG_INTERVAL: Duration = Duration::from_secs(5);
const RTX_CACHE_PACKETS: usize = 2048;
const RTX_CACHE_AGE: Duration = Duration::from_secs(3);
const RTX_RATIO_CAP: f32 = 0.15;

struct IngestSession {
    id: String,
    rtc: Rtc,
    next_timeout: Instant,
    video_mid: Option<Mid>,
    audio_mid: Option<Mid>,
    video_codec: Option<Codec>,
    video_pt: Option<Pt>,
    audio_pt: Option<Pt>,
    video_ssrc: Option<Ssrc>,
    connected: bool,
    last_media_at: Option<Instant>,
    last_pli_at: Option<Instant>,
}

struct ViewerSession {
    id: String,
    rtc: Rtc,
    next_timeout: Instant,
    created_at: Instant,
    connected: bool,
    video_mid: Option<Mid>,
    audio_mid: Option<Mid>,
    video_pt: Option<Pt>,
    audio_pt: Option<Pt>,
    video_seq: u64,
    audio_seq: u64,
    rtx_configured: bool,
    last_rtt: Option<Duration>,
    last_loss: Option<f32>,
}

pub async fn run(
    config: Config,
    mut whip_rx: mpsc::Receiver<WhipCommand>,
    mut offer_rx: mpsc::Receiver<ViewerOffer>,
    signal_tx: mpsc::Sender<SignalEvent>,
    mut shutdown_rx: watch::Receiver<bool>,
) {
    let socket = match bind_shared_udp(config.media_port) {
        Ok(s) => s,
        Err(e) => {
            error!(err = %e, "Failed to bind media UDP socket");
            return;
        }
    };
    let local_port = socket.local_addr().expect("bound UDP socket").port();
    let candidates = gather_local_candidates(&socket, local_port, &config).await;

    info!(
        media_port = local_port,
        candidates = candidates.len(),
        "Media relay ready"
    );

    let mut ingest: Option<IngestSession> = None;
    let mut viewers: HashMap<String, ViewerSession> = HashMap::new();
    let mut metrics = RelayMetrics::new(Instant::now());
    let mut udp_buf = vec![0u8; 2048];
    let mut next_session_seq: u64 = 1;

    loop {
        let now = Instant::now();
        let next_deadline = compute_next_deadline(now, ingest.as_ref(), &viewers);
        let sleep_dur = next_deadline
            .saturating_duration_since(now)
            .min(Duration::from_secs(1));

        tokio::select! {
            biased;

            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    info!("Media relay shutting down");
                    if ingest.is_some() {
                        let _ = signal_tx.try_send(SignalEvent::StreamStop);
                    }
                    break;
                }
            }

            recv = socket.recv_from(&mut udp_buf) => {
                match recv {
                    Ok((n, source)) => {
                        let recv_time = Instant::now();
                        let dest = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), local_port);
                        if let Ok(receive) = Receive::new(Protocol::Udp, source, dest, &udp_buf[..n]) {
                            let input = Input::Receive(recv_time, receive);
                            let mut handled = false;

                            if let Some(ing) = ingest.as_mut()
                                && ing.rtc.accepts(&input)
                            {
                                handled = true;
                                if let Err(e) = ing.rtc.handle_input(input) {
                                    debug!("Ingest handle_input error: {e}");
                                }
                                drain_ingest(
                                    ing,
                                    &mut viewers,
                                    &socket,
                                    &mut metrics,
                                    &signal_tx,
                                ).await;
                            } else {
                                let target_id = viewers
                                    .iter()
                                    .find(|(_, v)| v.rtc.accepts(&input))
                                    .map(|(id, _)| id.clone());
                                if let Some(vid) = target_id
                                    && let Some(v) = viewers.get_mut(&vid)
                                {
                                    handled = true;
                                    if let Err(e) = v.rtc.handle_input(input) {
                                        debug!(viewer = %vid, "Viewer handle_input error: {e}");
                                    }
                                    let want_pli = drain_viewer(v, &socket, &mut metrics).await;
                                    if want_pli
                                        && let Some(ing) = ingest.as_mut()
                                    {
                                        request_ingest_keyframe(ing, &socket, &mut metrics).await;
                                    }
                                }
                            }

                            if !handled {
                                metrics.record_udp_drop();
                                trace!(addr = %source, bytes = n, "Dropped unmatched UDP packet");
                            }
                        }
                    }
                    Err(e) => {
                        // Windows returns WSAECONNRESET (10054) on UDP sockets after ICMP port unreachable
                        metrics.record_udp_drop();
                        trace!(err = %e, "UDP recv_from error");
                    }
                }
            }

            Some(cmd) = whip_rx.recv() => {
                match cmd {
                    WhipCommand::Offer { sdp, reply } => {
                        let res = handle_whip_offer(
                            sdp,
                            &candidates,
                            &mut next_session_seq,
                            &mut ingest,
                            &mut viewers,
                            &mut metrics,
                            &socket,
                            &signal_tx,
                        )
                        .await;
                        let _ = reply.send(res);
                    }
                    WhipCommand::Delete { session_id, reply } => {
                        let matched = ingest.as_ref().is_some_and(|i| i.id == session_id);
                        if matched {
                            info!(session = %session_id, "WHIP session deleted");
                            teardown_ingest(&mut ingest, &mut viewers, &mut metrics, &signal_tx);
                        }
                        let _ = reply.send(matched);
                    }
                }
            }

            Some(offer) = offer_rx.recv() => {
                let res = handle_viewer_offer(
                    offer.viewer_id.clone(),
                    offer.sdp,
                    &config,
                    &candidates,
                    ingest.as_ref(),
                    &mut viewers,
                    &mut metrics,
                    &socket,
                )
                .await;
                let _ = offer.reply.send(res);
            }

            _ = tokio::time::sleep(sleep_dur) => {
                let now = Instant::now();

                // Drive ingest timeout + check ingest silence timeout
                let mut ingest_dead = false;
                if let Some(ing) = ingest.as_mut() {
                    if now >= ing.next_timeout {
                        if let Err(e) = ing.rtc.handle_input(Input::Timeout(now)) {
                            debug!("Ingest timeout error: {e}");
                        }
                        drain_ingest(ing, &mut viewers, &socket, &mut metrics, &signal_tx).await;
                    }
                    if !ing.rtc.is_alive() {
                        ingest_dead = true;
                    } else if config.ingest_timeout_secs > 0
                        && let Some(last) = ing.last_media_at
                        && now.duration_since(last) >= Duration::from_secs(config.ingest_timeout_secs)
                    {
                        warn!(
                            session = %ing.id,
                            timeout_secs = config.ingest_timeout_secs,
                            "Ingest media silence timeout"
                        );
                        ingest_dead = true;
                    }
                }
                if ingest_dead {
                    teardown_ingest(&mut ingest, &mut viewers, &mut metrics, &signal_tx);
                }

                // Drive viewer timeouts & prune dead/stalled viewers
                let mut dead_viewers = Vec::new();
                let mut want_pli = false;
                let mut congestion_changed = None;

                for (id, v) in viewers.iter_mut() {
                    if now >= v.next_timeout {
                        if let Err(e) = v.rtc.handle_input(Input::Timeout(now)) {
                            debug!(viewer = %id, "Viewer timeout error: {e}");
                        }
                        if drain_viewer(v, &socket, &mut metrics).await {
                            want_pli = true;
                        }
                    }

                    if !v.rtc.is_alive()
                        || (!v.connected && now.duration_since(v.created_at) >= VIEWER_HANDSHAKE_TIMEOUT)
                    {
                        dead_viewers.push(id.clone());
                    } else if v.connected
                        && let Some(changed) = metrics.update_congestion(now, v.last_rtt, v.last_loss)
                    {
                        congestion_changed = Some(changed);
                    }
                }

                for id in dead_viewers {
                    viewers.remove(&id);
                    info!(viewer = %id, viewers = viewers.len(), "Viewer disconnected");
                }

                if let Some(congested) = congestion_changed {
                    warn!(congested, "Uplink congestion state changed");
                    let ratio_cap = if congested { Some(0.0) } else { Some(RTX_RATIO_CAP) };
                    for v in viewers.values_mut() {
                        if let Some(mid) = v.video_mid
                            && let Some(tx) = v.rtc.direct_api().stream_tx_by_mid(mid, None)
                        {
                            tx.set_rtx_cache(RTX_CACHE_PACKETS, RTX_CACHE_AGE, ratio_cap);
                        }
                        let _ = drain_viewer(v, &socket, &mut metrics).await;
                    }
                }

                if want_pli
                    && let Some(ing) = ingest.as_mut()
                {
                    request_ingest_keyframe(ing, &socket, &mut metrics).await;
                }

                // Periodic derivative metrics snapshot while ingest is active (debug-only).
                if let Some(ing) = ingest.as_ref()
                    && now.duration_since(metrics.last_log) >= STATS_LOG_INTERVAL
                {
                    metrics.last_log = now;
                    let max_rtt = viewers.values().filter_map(|v| v.last_rtt).max();
                    let max_loss = viewers
                        .values()
                        .filter_map(|v| v.last_loss)
                        .reduce(f32::max);
                    let snap = metrics.take_snapshot(now, viewers.len(), max_rtt, max_loss);
                    let codec = ing
                        .video_codec
                        .map(|c| format!("{c:?}"))
                        .unwrap_or_else(|| "pending".to_string());
                    debug!(
                        codec = %codec,
                        viewers = snap.viewers,
                        in_kbps = format_args!("{:.0}", snap.in_kbps),
                        out_kbps = format_args!("{:.0}", snap.out_kbps),
                        in_pps = snap.in_pps,
                        out_pps = snap.out_pps,
                        rtt_ms = snap.rtt_ms,
                        rtt_delta_ms = snap.rtt_delta_ms,
                        loss_pct = format_args!("{:.1}", snap.loss_pct),
                        pli = snap.pli,
                        udp_drops = snap.udp_drops,
                        offers_rejected = snap.offers_rejected,
                        congested = snap.congested,
                        "Stream metrics"
                    );
                }
            }
        }
    }
}

fn compute_next_deadline(
    now: Instant,
    ingest: Option<&IngestSession>,
    viewers: &HashMap<String, ViewerSession>,
) -> Instant {
    let mut earliest = now + Duration::from_millis(250);
    if let Some(ing) = ingest {
        earliest = earliest.min(ing.next_timeout);
    }
    for v in viewers.values() {
        earliest = earliest.min(v.next_timeout);
    }
    earliest
}

#[allow(clippy::too_many_arguments)]
async fn handle_whip_offer(
    sdp: String,
    candidates: &[Candidate],
    next_session_seq: &mut u64,
    ingest: &mut Option<IngestSession>,
    viewers: &mut HashMap<String, ViewerSession>,
    metrics: &mut RelayMetrics,
    socket: &UdpSocket,
    signal_tx: &mpsc::Sender<SignalEvent>,
) -> Result<WhipAccepted, WhipError> {
    let offer = SdpOffer::from_sdp_string(&sdp)
        .map_err(|e| WhipError::BadRequest(format!("Invalid SDP offer: {e}")))?;

    let now = Instant::now();
    let mut rtc = RtcConfig::new()
        .set_rtp_mode(true)
        .set_stats_interval(Some(Duration::from_secs(1)))
        .build(now);

    for c in candidates {
        rtc.add_local_candidate(c.clone());
    }

    let answer = rtc
        .sdp_api()
        .accept_offer(offer)
        .map_err(|e| WhipError::BadRequest(format!("Failed to accept SDP offer: {e}")))?;

    let session_id = format!("s{}", *next_session_seq);
    *next_session_seq += 1;

    if ingest.is_some() {
        info!("Replacing existing WHIP ingest session");
        teardown_ingest(ingest, viewers, metrics, signal_tx);
    }

    let mut session = IngestSession {
        id: session_id.clone(),
        rtc,
        next_timeout: now,
        video_mid: None,
        audio_mid: None,
        video_codec: None,
        video_pt: None,
        audio_pt: None,
        video_ssrc: None,
        connected: false,
        last_media_at: None,
        last_pli_at: None,
    };

    drain_ingest(&mut session, viewers, socket, metrics, signal_tx).await;

    debug!(session = %session_id, "WHIP ingest offer accepted");
    *ingest = Some(session);

    Ok(WhipAccepted {
        session_id,
        answer_sdp: answer.to_sdp_string(),
    })
}

#[allow(clippy::too_many_arguments)]
async fn handle_viewer_offer(
    viewer_id: String,
    sdp: String,
    config: &Config,
    candidates: &[Candidate],
    ingest: Option<&IngestSession>,
    viewers: &mut HashMap<String, ViewerSession>,
    metrics: &mut RelayMetrics,
    socket: &UdpSocket,
) -> Result<String, String> {
    let Some(ing) = ingest else {
        metrics.record_offer_rejected();
        return Err("No active stream".to_string());
    };

    if viewers.len() >= config.max_peers && !viewers.contains_key(&viewer_id) {
        metrics.record_offer_rejected();
        return Err(format!("Max peers reached ({})", config.max_peers));
    }

    let now = Instant::now();
    if config.max_uplink_mbps > 0.0 {
        let in_kbps = metrics.in_kbps(now);
        let projected_mbps = (in_kbps * (viewers.len() + 1) as f64) / 1000.0;
        if projected_mbps > config.max_uplink_mbps {
            metrics.record_offer_rejected();
            return Err(format!(
                "Uplink cap reached ({projected_mbps:.1} > {:.1} Mbps)",
                config.max_uplink_mbps
            ));
        }
    }

    let offer = SdpOffer::from_sdp_string(&sdp).map_err(|e| {
        metrics.record_offer_rejected();
        format!("Invalid viewer SDP: {e}")
    })?;

    // Match the ingest's negotiated video codec so the browser negotiates the exact same codec.
    let mut rtc_cfg = RtcConfig::new()
        .set_rtp_mode(true)
        .set_stats_interval(Some(Duration::from_secs(1)));

    if let Some(codec) = ing.video_codec {
        rtc_cfg = rtc_cfg.clear_codecs().enable_opus(true, false);
        rtc_cfg = match codec {
            Codec::Av1 => rtc_cfg.enable_av1(true),
            Codec::H264 => rtc_cfg.enable_h264(true),
            Codec::H265 => rtc_cfg.enable_h265(true),
            Codec::Vp8 => rtc_cfg.enable_vp8(true),
            Codec::Vp9 => rtc_cfg.enable_vp9(true),
            _ => rtc_cfg
                .enable_av1(true)
                .enable_h264(true)
                .enable_h265(true)
                .enable_vp8(true)
                .enable_vp9(true),
        };
    }

    let mut rtc = rtc_cfg.build(now);
    for c in candidates {
        rtc.add_local_candidate(c.clone());
    }

    let answer = rtc
        .sdp_api()
        .accept_offer(offer)
        .map_err(|e| format!("Failed to accept viewer offer: {e}"))?;

    let mut viewer = ViewerSession {
        id: viewer_id.clone(),
        rtc,
        next_timeout: now,
        created_at: now,
        connected: false,
        video_mid: None,
        audio_mid: None,
        video_pt: None,
        audio_pt: None,
        video_seq: 1,
        audio_seq: 1,
        rtx_configured: false,
        last_rtt: None,
        last_loss: None,
    };

    let _ = drain_viewer(&mut viewer, socket, metrics).await;

    debug!(viewer = %viewer_id, viewers = viewers.len() + 1, "Viewer offer accepted");
    viewers.insert(viewer_id, viewer);

    Ok(answer.to_sdp_string())
}

async fn drain_ingest(
    ing: &mut IngestSession,
    viewers: &mut HashMap<String, ViewerSession>,
    socket: &UdpSocket,
    metrics: &mut RelayMetrics,
    signal_tx: &mpsc::Sender<SignalEvent>,
) {
    loop {
        match ing.rtc.poll_output() {
            Ok(Output::Timeout(t)) => {
                ing.next_timeout = t;
                break;
            }
            Ok(Output::Transmit(t)) => {
                if let Err(e) = socket.send_to(&t.contents, t.destination).await {
                    metrics.record_udp_drop();
                    trace!(session = %ing.id, err = %e, "Ingest UDP send_to error");
                }
            }
            Ok(Output::Event(ev)) => match ev {
                Event::Connected => {
                    ing.connected = true;
                    info!(session = %ing.id, "WHIP ingest connected");
                    let _ = signal_tx.try_send(SignalEvent::StreamStart);
                }
                Event::IceConnectionStateChange(IceConnectionState::Disconnected) => {
                    info!(session = %ing.id, "WHIP ingest ICE disconnected");
                    let _ = ing.rtc.close();
                }
                Event::MediaAdded(m) => {
                    let first_param = ing
                        .rtc
                        .codec_config()
                        .params()
                        .iter()
                        .find(|p| p.spec().codec.kind() == m.kind)
                        .copied();
                    match m.kind {
                        MediaKind::Video => {
                            ing.video_mid = Some(m.mid);
                            if let Some(p) = first_param {
                                ing.video_pt = Some(p.pt());
                                ing.video_codec = Some(p.spec().codec);
                                debug!(
                                    session = %ing.id,
                                    codec = ?p.spec().codec,
                                    pt = *p.pt(),
                                    "Ingest video track negotiated"
                                );
                            }
                        }
                        MediaKind::Audio => {
                            ing.audio_mid = Some(m.mid);
                            if let Some(p) = first_param {
                                ing.audio_pt = Some(p.pt());
                                debug!(
                                    session = %ing.id,
                                    codec = ?p.spec().codec,
                                    pt = *p.pt(),
                                    "Ingest audio track negotiated"
                                );
                            }
                        }
                    }
                }
                Event::RtpPacket(pkt) => {
                    let now = Instant::now();
                    ing.last_media_at = Some(now);
                    metrics.record_in(now, pkt.payload.len());

                    // Identify track kind and active codec from the actual incoming PT.
                    let matched_param = ing
                        .rtc
                        .codec_config()
                        .params()
                        .iter()
                        .find(|p| p.pt() == pkt.header.payload_type)
                        .copied();
                    let is_video = match matched_param {
                        Some(p) => {
                            let v = p.spec().codec.kind() == MediaKind::Video;
                            if v && ing.video_codec != Some(p.spec().codec) {
                                ing.video_pt = Some(p.pt());
                                ing.video_codec = Some(p.spec().codec);
                                info!(
                                    session = %ing.id,
                                    codec = ?p.spec().codec,
                                    pt = *p.pt(),
                                    "Active ingest video codec detected"
                                );
                            }
                            v
                        }
                        None => {
                            Some(pkt.header.payload_type) == ing.video_pt
                                || (ing.video_pt.is_none()
                                    && Some(pkt.header.payload_type) != ing.audio_pt)
                        }
                    };

                    if is_video && ing.video_ssrc.is_none() {
                        ing.video_ssrc = Some(pkt.header.ssrc);
                    }

                    fanout_rtp(&pkt, is_video, viewers, socket, metrics).await;
                }
                _ => {}
            },
            Err(e) => {
                warn!(session = %ing.id, "Ingest poll_output error: {e}");
                break;
            }
        }
    }
}

async fn fanout_rtp(
    pkt: &RtpPacket,
    is_video: bool,
    viewers: &mut HashMap<String, ViewerSession>,
    socket: &UdpSocket,
    metrics: &mut RelayMetrics,
) {
    let nackable = is_video && !metrics.congested;
    let payload_len = pkt.payload.len();
    let now = Instant::now();

    for v in viewers.values_mut() {
        if !v.connected {
            continue;
        }
        let (mid_opt, pt_opt, seq_counter) = if is_video {
            (v.video_mid, v.video_pt, &mut v.video_seq)
        } else {
            (v.audio_mid, v.audio_pt, &mut v.audio_seq)
        };
        let (Some(mid), Some(pt)) = (mid_opt, pt_opt) else {
            continue;
        };

        let seq: SeqNo = (*seq_counter).into();
        *seq_counter += 1;

        if let Some(stream_tx) = v.rtc.direct_api().stream_tx_by_mid(mid, None) {
            if is_video && !v.rtx_configured {
                let ratio_cap = if metrics.congested {
                    Some(0.0)
                } else {
                    Some(RTX_RATIO_CAP)
                };
                stream_tx.set_rtx_cache(RTX_CACHE_PACKETS, RTX_CACHE_AGE, ratio_cap);
                v.rtx_configured = true;
            }

            // Zero-copy Arc<[u8]> clone across all viewers.
            let write = RtpWrite::new(
                pt,
                seq,
                pkt.header.timestamp,
                pkt.timestamp,
                Arc::clone(&pkt.payload),
            )
            .marker(pkt.header.marker)
            .nackable(nackable);

            stream_tx.write_rtp(write);
            metrics.record_out(now, payload_len);
        }

        // Single-mutation invariant: drain immediately after write_rtp.
        let _ = drain_viewer(v, socket, metrics).await;
    }
}

/// Drains a viewer's `Rtc` output queue. Returns `true` if the viewer requested a keyframe (PLI/FIR).
async fn drain_viewer(
    v: &mut ViewerSession,
    socket: &UdpSocket,
    metrics: &mut RelayMetrics,
) -> bool {
    let mut want_pli = false;

    loop {
        match v.rtc.poll_output() {
            Ok(Output::Timeout(t)) => {
                v.next_timeout = t;
                break;
            }
            Ok(Output::Transmit(t)) => {
                if let Err(e) = socket.send_to(&t.contents, t.destination).await {
                    metrics.record_udp_drop();
                    trace!(viewer = %v.id, err = %e, "Viewer UDP send_to error");
                }
            }
            Ok(Output::Event(ev)) => match ev {
                Event::Connected => {
                    v.connected = true;
                    want_pli = true;
                    info!(viewer = %v.id, "Viewer connected");
                }
                Event::IceConnectionStateChange(IceConnectionState::Disconnected) => {
                    let _ = v.rtc.close();
                }
                Event::MediaAdded(m) => {
                    if let Some(p) = v
                        .rtc
                        .codec_config()
                        .params()
                        .iter()
                        .find(|p| p.spec().codec.kind() == m.kind)
                        .copied()
                    {
                        match m.kind {
                            MediaKind::Video => {
                                v.video_mid = Some(m.mid);
                                v.video_pt = Some(p.pt());
                            }
                            MediaKind::Audio => {
                                v.audio_mid = Some(m.mid);
                                v.audio_pt = Some(p.pt());
                            }
                        }
                    }
                }
                Event::KeyframeRequest(_) => {
                    want_pli = true;
                }
                Event::PeerStats(stats) => {
                    v.last_rtt = stats
                        .rtt
                        .or_else(|| stats.selected_candidate_pair.and_then(|p| p.current_round_trip_time));
                    v.last_loss = stats.egress_loss_fraction;
                }
                _ => {}
            },
            Err(e) => {
                debug!(viewer = %v.id, "Viewer poll_output error: {e}");
                break;
            }
        }
    }

    want_pli
}

async fn request_ingest_keyframe(
    ing: &mut IngestSession,
    socket: &UdpSocket,
    metrics: &mut RelayMetrics,
) {
    let now = Instant::now();
    if let Some(last) = ing.last_pli_at
        && now.duration_since(last) < PLI_COOLDOWN
    {
        return;
    }

    let requested = if let Some(mid) = ing.video_mid
        && let Some(rx) = ing.rtc.direct_api().stream_rx_by_mid(mid, None)
    {
        rx.request_keyframe(KeyframeRequestKind::Pli);
        true
    } else if let Some(ssrc) = ing.video_ssrc
        && let Some(rx) = ing.rtc.direct_api().stream_rx(&ssrc)
    {
        rx.request_keyframe(KeyframeRequestKind::Pli);
        true
    } else {
        false
    };

    if requested {
        ing.last_pli_at = Some(now);
        metrics.record_pli();
        debug!(session = %ing.id, "PLI sent");
        // Drain PLI RTCP packet out to OBS immediately.
        loop {
            match ing.rtc.poll_output() {
                Ok(Output::Timeout(t)) => {
                    ing.next_timeout = t;
                    break;
                }
                Ok(Output::Transmit(t)) => {
                    let _ = socket.send_to(&t.contents, t.destination).await;
                }
                Ok(Output::Event(_)) => {}
                Err(_) => break,
            }
        }
    }
}

fn teardown_ingest(
    ingest: &mut Option<IngestSession>,
    viewers: &mut HashMap<String, ViewerSession>,
    metrics: &mut RelayMetrics,
    signal_tx: &mpsc::Sender<SignalEvent>,
) {
    if let Some(mut ing) = ingest.take() {
        let _ = ing.rtc.close();
    }
    viewers.clear();
    metrics.reset_ingest();
    let _ = signal_tx.try_send(SignalEvent::StreamStop);
}
