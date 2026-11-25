mod metrics;
mod net;

use std::collections::HashMap;
use std::io::ErrorKind;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{self as std_mpsc, RecvTimeoutError, TryRecvError, TrySendError};
use std::time::{Duration, Instant};

use str0m::change::SdpOffer;
use str0m::config::DtlsCert;
use str0m::crypto as str0m_crypto;
use str0m::format::{Codec, CodecConfig, FormatParams, PayloadParams};
use str0m::media::{Frequency, KeyframeRequestKind, MediaKind, Mid, Pt};
use str0m::net::{Protocol, Receive};
use str0m::rtp::{RtpWrite, SeqNo, Ssrc};
use str0m::{Event, IceConnectionState, Input, Output, Rtc, RtcConfig};
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, oneshot, watch};
use tracing::{debug, error, info, trace, warn};

use self::metrics::{MetricsRollup, MetricsWriter, RelayMetrics, ShardCounters};
use self::net::{
    BoundCandidates, bind_ingest_udp, bind_shard_udp, from_socket_addr, gather_all_candidates,
    to_socket_addr,
};
use crate::config::Config;
use crate::signal::{SignalEvent, ViewerOffer};
use crate::whip::{WhipAccepted, WhipCommand, WhipError};

const VIEWER_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(20);
const PLI_COOLDOWN: Duration = Duration::from_secs(2);
const METRICS_INTERVAL: Duration = Duration::from_secs(1);
const STDOUT_ROLLUP_TICKS: usize = 5;
const RTX_CACHE_PACKETS: usize = 2048;
const RTX_CACHE_AGE: Duration = Duration::from_secs(3);
const RTX_RATIO_CAP: f32 = 0.15;
const RTX_CONGESTED_RATIO_CAP: f32 = 0.05;
const SHARD_CHANNEL_CAP: usize = 2048;
const MAX_UDP_BATCH: usize = 64;
const MAX_SHARD_BATCH: usize = 64;
const SHARD_UDP_POLL_CAP: Duration = Duration::from_millis(2);

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
    metrics_rollup: MetricsRollup,
}

struct ViewerSession {
    id: String,
    rtc: Rtc,
    next_timeout: Instant,
    created_at: Instant,
    connected: bool,
    remote_addr: Option<SocketAddr>,
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

struct SharedRtp {
    is_video: bool,
    timestamp: u32,
    wall_time: Instant,
    marker: bool,
    payload: Arc<[u8]>,
}

enum ShardMsg {
    Rtp(Arc<SharedRtp>),
    AddViewer {
        viewer_id: String,
        sdp: String,
        video_codec: Option<Codec>,
        reply: oneshot::Sender<Result<String, String>>,
    },
    SetCongested(bool),
    ClearViewers,
    Shutdown,
}

struct ShardHandle {
    tx: std_mpsc::SyncSender<ShardMsg>,
    counters: Arc<ShardCounters>,
    handle: Option<std::thread::JoinHandle<()>>,
}

pub async fn run(
    config: Config,
    mut whip_rx: mpsc::Receiver<WhipCommand>,
    mut offer_rx: mpsc::Receiver<ViewerOffer>,
    signal_tx: mpsc::Sender<SignalEvent>,
    mut shutdown_rx: watch::Receiver<bool>,
) {
    let mut shard_count = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(2);
    if config.max_peers > 0 {
        shard_count = shard_count.min(config.max_peers);
    }
    let shard_count = shard_count.clamp(1, 32);

    let ingest_socket = match bind_ingest_udp(config.media_port) {
        Ok(s) => s,
        Err(e) => {
            error!(err = %e, "Failed to bind ingest UDP socket");
            return;
        }
    };

    let mut all_sockets = Vec::with_capacity(1 + shard_count);
    all_sockets.push(ingest_socket);

    for shard_id in 0..shard_count {
        let port = if config.media_port == 0 {
            0
        } else {
            match config.media_port.checked_add(1 + shard_id as u16) {
                Some(p) => p,
                None => {
                    error!(
                        media_port = config.media_port,
                        shard = shard_id,
                        "Media port range overflow"
                    );
                    return;
                }
            }
        };
        match bind_shard_udp(port) {
            Ok(s) => all_sockets.push(s),
            Err(e) => {
                error!(port, shard = shard_id, err = %e, "Failed to bind shard UDP socket");
                return;
            }
        }
    }

    let mut gathered = gather_all_candidates(all_sockets, &config).await.into_iter();
    let (socket, ingest_bound) = gathered.next().expect("ingest socket");

    let mut metrics_writer = config.resolved_metrics_dir().and_then(|dir| {
        match MetricsWriter::open(&dir) {
            Ok(w) => Some(w),
            Err(e) => {
                warn!(dir = %dir.display(), err = %e, "Could not open metrics file");
                None
            }
        }
    });

    let dtls_cert = str0m_crypto::from_feature_flags()
        .dtls_provider
        .generate_certificate();

    let (pli_tx, mut pli_rx) = mpsc::channel::<()>(64);
    let mut shards: Vec<ShardHandle> = Vec::with_capacity(shard_count);

    for (shard_id, (shard_tokio_sock, shard_bound)) in gathered.enumerate() {
        let shard_socket = match shard_tokio_sock.into_std() {
            Ok(s) => s,
            Err(e) => {
                error!(shard = shard_id, err = %e, "Failed to convert shard UDP socket");
                return;
            }
        };
        let (tx, rx) = std_mpsc::sync_channel::<ShardMsg>(SHARD_CHANNEL_CAP);
        let counters = Arc::new(ShardCounters::new());
        let shard_counters = Arc::clone(&counters);
        let shard_pli_tx = pli_tx.clone();
        let shard_cert = dtls_cert.clone();
        let handle = std::thread::Builder::new()
            .name(format!("tiwi-egress-{shard_id}"))
            .spawn(move || {
                run_egress_shard(
                    shard_id,
                    rx,
                    shard_socket,
                    shard_bound,
                    shard_counters,
                    shard_pli_tx,
                    shard_cert,
                );
            })
            .expect("failed to spawn egress shard thread");
        shards.push(ShardHandle {
            tx,
            counters,
            handle: Some(handle),
        });
    }

    info!(
        media_port = ingest_bound.local_port,
        candidates = ingest_bound.candidates.len(),
        shards = shard_count,
        "Media relay ready"
    );

    let mut ingest: Option<IngestSession> = None;
    let mut metrics = RelayMetrics::new(Instant::now());
    let mut udp_buf = vec![0u8; 2048];
    let mut next_session_seq: u64 = 1;

    loop {
        let now = Instant::now();
        let next_deadline = compute_ingest_deadline(now, ingest.as_ref(), metrics.last_tick);
        let sleep_dur = next_deadline
            .saturating_duration_since(now)
            .min(Duration::from_secs(1));

        tokio::select! {
            biased;

            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    debug!("Media relay shutting down");
                    if ingest.is_some() {
                        teardown_ingest(
                            &mut ingest,
                            &shards,
                            &socket,
                            ingest_bound.is_dual_stack,
                            &mut metrics,
                            &signal_tx,
                        );
                    }
                    for shard in &mut shards {
                        let _ = shard.tx.send(ShardMsg::Shutdown);
                    }
                    for shard in &mut shards {
                        if let Some(h) = shard.handle.take() {
                            let _ = h.join();
                        }
                    }
                    if let Some(writer) = metrics_writer.take() {
                        writer.finish();
                    }
                    break;
                }
            }

            recv = socket.recv_from(&mut udp_buf) => {
                match recv {
                    Ok((n, source)) => {
                        handle_ingest_udp(
                            &udp_buf[..n],
                            source,
                            &ingest_bound,
                            &mut ingest,
                            &shards,
                            &socket,
                            &mut metrics,
                            &signal_tx,
                        );
                        // Drain queued ingest UDP packets in a tight batch to amortize select! overhead.
                        for _ in 1..MAX_UDP_BATCH {
                            match socket.try_recv_from(&mut udp_buf) {
                                Ok((n, source)) => {
                                    handle_ingest_udp(
                                        &udp_buf[..n],
                                        source,
                                        &ingest_bound,
                                        &mut ingest,
                                        &shards,
                                        &socket,
                                        &mut metrics,
                                        &signal_tx,
                                    );
                                }
                                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                                Err(e) => {
                                    metrics.record_udp_drop();
                                    trace!(err = %e, "Ingest UDP try_recv_from error");
                                    break;
                                }
                            }
                        }
                    }
                    Err(e) => {
                        metrics.record_udp_drop();
                        trace!(err = %e, "Ingest UDP recv_from error");
                    }
                }
            }

            Some(()) = pli_rx.recv() => {
                while pli_rx.try_recv().is_ok() {}
                if let Some(ing) = ingest.as_mut() {
                    request_ingest_keyframe(
                        ing,
                        &shards,
                        &socket,
                        ingest_bound.is_dual_stack,
                        &mut metrics,
                        &signal_tx,
                    );
                }
            }

            Some(cmd) = whip_rx.recv() => {
                match cmd {
                    WhipCommand::Offer { sdp, reply } => {
                        let res = handle_whip_offer(
                            sdp,
                            &ingest_bound,
                            dtls_cert.as_ref(),
                            &mut next_session_seq,
                            &mut ingest,
                            &shards,
                            &mut metrics,
                            &socket,
                            &signal_tx,
                        );
                        let _ = reply.send(res);
                    }
                    WhipCommand::Delete { session_id, reply } => {
                        let matched = ingest.as_ref().is_some_and(|i| i.id == session_id);
                        if matched {
                            info!(session = %session_id, "WHIP ingest deleted");
                            teardown_ingest(
                                &mut ingest,
                                &shards,
                                &socket,
                                ingest_bound.is_dual_stack,
                                &mut metrics,
                                &signal_tx,
                            );
                        }
                        let _ = reply.send(matched);
                    }
                }
            }

            Some(offer) = offer_rx.recv() => {
                dispatch_viewer_offer(
                    offer,
                    &config,
                    ingest.as_ref(),
                    &shards,
                    &mut metrics,
                );
            }

            _ = tokio::time::sleep(sleep_dur) => {}
        }

        // Reap a closed ingest session immediately rather than waiting for the next timer tick.
        if ingest.as_ref().is_some_and(|i| !i.rtc.is_alive()) {
            teardown_ingest(
                &mut ingest,
                &shards,
                &socket,
                ingest_bound.is_dual_stack,
                &mut metrics,
                &signal_tx,
            );
        }

        // Run ingest timers and 1s metrics tick outside `select!` so continuous UDP traffic
        // under `biased` cannot starve timeouts or metrics snapshots.
        let now = Instant::now();
        if now >= next_deadline {
            let mut ingest_dead = false;
            if let Some(ing) = ingest.as_mut() {
                if now >= ing.next_timeout {
                    if let Err(e) = ing.rtc.handle_input(Input::Timeout(now)) {
                        debug!(session = %ing.id, err = %e, "Ingest timeout error");
                    }
                    drain_ingest(
                        ing,
                        &shards,
                        &socket,
                        ingest_bound.is_dual_stack,
                        &mut metrics,
                        &signal_tx,
                    );
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
                        "WHIP ingest media silence timeout"
                    );
                    ingest_dead = true;
                }
            }
            if ingest_dead {
                teardown_ingest(
                    &mut ingest,
                    &shards,
                    &socket,
                    ingest_bound.is_dual_stack,
                    &mut metrics,
                    &signal_tx,
                );
            }

            // 1s derivative metrics snapshot to JSONL file, folded into 5s stdout debug rollup.
            if let Some(ing) = ingest.as_mut()
                && now.duration_since(metrics.last_tick) >= METRICS_INTERVAL
            {
                let elapsed = now.duration_since(metrics.last_tick);
                metrics.last_tick = now;

                let mut total_viewers = 0usize;
                let mut total_out_bytes = 0u64;
                let mut total_out_pkts = 0u64;
                let mut total_shard_drops = 0u64;
                let mut max_rtt: Option<Duration> = None;
                let mut max_loss: Option<f32> = None;

                for shard in &shards {
                    total_viewers += shard.counters.viewers.load(Ordering::Relaxed);
                    let (bytes, pkts, drops, rtt, loss) = shard.counters.take_interval();
                    total_out_bytes += bytes;
                    total_out_pkts += pkts;
                    total_shard_drops += drops;
                    max_rtt = match (max_rtt, rtt) {
                        (Some(a), Some(b)) => Some(a.max(b)),
                        (a, b) => a.or(b),
                    };
                    max_loss = match (max_loss, loss) {
                        (Some(a), Some(b)) => Some(a.max(b)),
                        (a, b) => a.or(b),
                    };
                }

                if let Some(congested) = metrics.update_congestion(now, max_rtt, max_loss) {
                    warn!(congested, "Uplink congestion state changed");
                    for shard in &shards {
                        let _ = shard.tx.try_send(ShardMsg::SetCongested(congested));
                    }
                }

                let codec = ing
                    .video_codec
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "pending".to_string());
                let snap = metrics.take_snapshot(
                    now,
                    elapsed,
                    codec,
                    total_viewers,
                    total_out_bytes,
                    total_out_pkts,
                    total_shard_drops,
                    max_rtt,
                    max_loss,
                );

                if let Some(writer) = metrics_writer.as_mut() {
                    writer.write(&snap);
                }

                if let Some(rollup) = ing.metrics_rollup.push(&snap) {
                    debug!(
                        codec = %rollup.codec,
                        viewers = rollup.viewers,
                        in_kbps = format_args!("{:.0}", rollup.in_kbps),
                        out_kbps = format_args!("{:.0}", rollup.out_kbps),
                        in_pps = rollup.in_pps,
                        out_pps = rollup.out_pps,
                        rtt_ms = rollup.rtt_ms,
                        rtt_delta_ms = rollup.rtt_delta_ms,
                        loss_pct = format_args!("{:.1}", rollup.loss_pct),
                        pli = rollup.pli,
                        udp_drops = rollup.udp_drops,
                        offers_rejected = rollup.offers_rejected,
                        congested = rollup.congested,
                        "Stream metrics"
                    );
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_ingest_udp(
    buf: &[u8],
    raw_source: SocketAddr,
    bound: &BoundCandidates,
    ingest: &mut Option<IngestSession>,
    shards: &[ShardHandle],
    socket: &UdpSocket,
    metrics: &mut RelayMetrics,
    signal_tx: &mpsc::Sender<SignalEvent>,
) {
    let Some(ing) = ingest.as_mut() else {
        return;
    };
    let source = from_socket_addr(raw_source);
    let dest = local_dest_for(source, bound);
    let Ok(receive) = Receive::new(Protocol::Udp, source, dest, buf) else {
        return;
    };
    let input = Input::Receive(Instant::now(), receive);
    if ing.rtc.accepts(&input) {
        if let Err(e) = ing.rtc.handle_input(input) {
            debug!(session = %ing.id, err = %e, "Ingest handle_input error");
        }
        drain_ingest(ing, shards, socket, bound.is_dual_stack, metrics, signal_tx);
    } else {
        trace!(addr = %source, bytes = buf.len(), "Ignored unmatched ingest UDP packet");
    }
}

#[inline]
fn local_dest_for(source: SocketAddr, bound: &BoundCandidates) -> SocketAddr {
    if source.is_ipv6() {
        bound.v6_host_addr.unwrap_or(bound.host_addr)
    } else if source.ip().is_loopback() {
        bound.loopback_addr
    } else {
        bound.host_addr
    }
}

fn compute_ingest_deadline(
    now: Instant,
    ingest: Option<&IngestSession>,
    last_metrics_tick: Instant,
) -> Instant {
    let mut earliest = now + Duration::from_millis(250);
    if let Some(ing) = ingest {
        earliest = earliest.min(ing.next_timeout);
        earliest = earliest.min(last_metrics_tick + METRICS_INTERVAL);
    }
    earliest
}

#[allow(clippy::too_many_arguments)]
fn handle_whip_offer(
    sdp: String,
    bound: &BoundCandidates,
    dtls_cert: Option<&DtlsCert>,
    next_session_seq: &mut u64,
    ingest: &mut Option<IngestSession>,
    shards: &[ShardHandle],
    metrics: &mut RelayMetrics,
    socket: &UdpSocket,
    signal_tx: &mpsc::Sender<SignalEvent>,
) -> Result<WhipAccepted, WhipError> {
    let offer = SdpOffer::from_sdp_string(&sdp)
        .map_err(|e| WhipError::BadRequest(format!("Invalid SDP offer: {e}")))?;

    let now = Instant::now();
    let mut rtc_cfg = RtcConfig::new()
        .set_rtp_mode(true)
        .set_stats_interval(Some(Duration::from_secs(1)));
    if let Some(cert) = dtls_cert {
        rtc_cfg = rtc_cfg.set_dtls_cert(cert.clone());
    }
    register_offer_av1_formats(rtc_cfg.codec_config(), &sdp);
    let mut rtc = rtc_cfg.build(now);

    for c in &bound.candidates {
        rtc.add_local_candidate(c.clone());
    }

    let answer = rtc
        .sdp_api()
        .accept_offer(offer)
        .map_err(|e| WhipError::BadRequest(format!("Failed to accept SDP offer: {e}")))?;

    let session_id = format!("s{}", *next_session_seq);
    *next_session_seq += 1;

    if let Some(prev) = ingest.as_ref() {
        info!(session = %prev.id, next = %session_id, "Replacing existing WHIP ingest");
        teardown_ingest(
            ingest,
            shards,
            socket,
            bound.is_dual_stack,
            metrics,
            signal_tx,
        );
    }

    metrics.last_tick = now;

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
        metrics_rollup: MetricsRollup::new(STDOUT_ROLLUP_TICKS),
    };

    let _ = session.rtc.handle_input(Input::Timeout(now));
    drain_ingest(
        &mut session,
        shards,
        socket,
        bound.is_dual_stack,
        metrics,
        signal_tx,
    );

    debug!(session = %session_id, "WHIP ingest offer accepted");
    *ingest = Some(session);

    Ok(WhipAccepted {
        session_id,
        answer_sdp: answer.to_sdp_string(),
    })
}

fn dispatch_viewer_offer(
    offer: ViewerOffer,
    config: &Config,
    ingest: Option<&IngestSession>,
    shards: &[ShardHandle],
    metrics: &mut RelayMetrics,
) {
    let Some(ing) = ingest else {
        metrics.record_offer_rejected();
        let _ = offer.reply.send(Err("No active stream".to_string()));
        return;
    };

    let total_viewers: usize = shards
        .iter()
        .map(|s| s.counters.viewers.load(Ordering::Relaxed))
        .sum();

    if config.max_peers > 0 && total_viewers >= config.max_peers {
        metrics.record_offer_rejected();
        let _ = offer
            .reply
            .send(Err(format!("Max peers reached ({})", config.max_peers)));
        return;
    }

    let now = Instant::now();
    if config.max_uplink_mbps > 0.0 {
        let in_kbps = metrics.in_kbps(now);
        let projected_mbps = (in_kbps * (total_viewers + 1) as f64) / 1000.0;
        if projected_mbps > config.max_uplink_mbps {
            metrics.record_offer_rejected();
            let _ = offer.reply.send(Err(format!(
                "Uplink cap reached ({projected_mbps:.1} > {:.1} Mbps)",
                config.max_uplink_mbps
            )));
            return;
        }
    }

    // Pick the least-loaded egress shard and pre-increment its viewer count so bursts
    // of simultaneous offers still respect max_peers accurately.
    let target = shards
        .iter()
        .min_by_key(|s| s.counters.viewers.load(Ordering::Relaxed))
        .expect("at least one egress shard");

    target.counters.viewers.fetch_add(1, Ordering::Relaxed);
    let msg = ShardMsg::AddViewer {
        viewer_id: offer.viewer_id,
        sdp: offer.sdp,
        video_codec: ing.video_codec,
        reply: offer.reply,
    };

    if let Err(err) = target.tx.try_send(msg) {
        target.counters.viewers.fetch_sub(1, Ordering::Relaxed);
        metrics.record_offer_rejected();
        let reply = match err {
            TrySendError::Full(ShardMsg::AddViewer { reply, .. })
            | TrySendError::Disconnected(ShardMsg::AddViewer { reply, .. }) => Some(reply),
            _ => None,
        };
        if let Some(r) = reply {
            let _ = r.send(Err("Relay shard busy".to_string()));
        }
    }
}

fn drain_ingest(
    ing: &mut IngestSession,
    shards: &[ShardHandle],
    socket: &UdpSocket,
    is_dual_stack: bool,
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
                if let Err(e) =
                    socket.try_send_to(&t.contents, to_socket_addr(t.destination, is_dual_stack))
                {
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
                    let param = negotiated_param(&ing.rtc, m.mid);
                    match m.kind {
                        MediaKind::Video => {
                            if let Some(p) = param {
                                ing.video_mid = Some(m.mid);
                                ing.video_pt = Some(p.pt());
                                ing.video_codec = Some(p.spec().codec);
                                info!(
                                    session = %ing.id,
                                    codec = %p.spec().codec,
                                    pt = *p.pt(),
                                    "Ingest video track negotiated"
                                );
                            }
                        }
                        MediaKind::Audio => {
                            if let Some(p) = param {
                                ing.audio_mid = Some(m.mid);
                                ing.audio_pt = Some(p.pt());
                                debug!(
                                    session = %ing.id,
                                    codec = %p.spec().codec,
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
                    metrics.record_in(pkt.payload.len());

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
                                let prev_codec = ing.video_codec;
                                ing.video_pt = Some(p.pt());
                                ing.video_codec = Some(p.spec().codec);
                                info!(
                                    session = %ing.id,
                                    codec = %p.spec().codec,
                                    pt = *p.pt(),
                                    "Active ingest video codec detected"
                                );
                                if prev_codec.is_some() {
                                    for shard in shards {
                                        let _ = shard.tx.send(ShardMsg::ClearViewers);
                                    }
                                    let _ = signal_tx.try_send(SignalEvent::StreamStart);
                                }
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

                    // Zero-copy fanout to all active egress shards; non-blocking try_send
                    // ensures an overloaded shard never backpressures OBS ingest.
                    let mut shared: Option<Arc<SharedRtp>> = None;
                    for shard in shards {
                        if shard.counters.connected_viewers.load(Ordering::Relaxed) == 0 {
                            continue;
                        }
                        let pkt_arc = shared.get_or_insert_with(|| {
                            Arc::new(SharedRtp {
                                is_video,
                                timestamp: pkt.header.timestamp,
                                wall_time: pkt.timestamp,
                                marker: pkt.header.marker,
                                payload: Arc::clone(&pkt.payload),
                            })
                        });
                        if let Err(TrySendError::Full(_)) =
                            shard.tx.try_send(ShardMsg::Rtp(Arc::clone(pkt_arc)))
                        {
                            shard.counters.record_udp_drop();
                        }
                    }
                }
                _ => {}
            },
            Err(e) => {
                warn!(session = %ing.id, err = %e, "Ingest poll_output error");
                break;
            }
        }
    }
}

fn request_ingest_keyframe(
    ing: &mut IngestSession,
    shards: &[ShardHandle],
    socket: &UdpSocket,
    is_dual_stack: bool,
    metrics: &mut RelayMetrics,
    signal_tx: &mpsc::Sender<SignalEvent>,
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
        trace!(session = %ing.id, "PLI sent");
        // Drain PLI RTCP packet out to OBS immediately without dropping queued events.
        drain_ingest(ing, shards, socket, is_dual_stack, metrics, signal_tx);
    }
}

fn teardown_ingest(
    ingest: &mut Option<IngestSession>,
    shards: &[ShardHandle],
    socket: &UdpSocket,
    is_dual_stack: bool,
    metrics: &mut RelayMetrics,
    signal_tx: &mpsc::Sender<SignalEvent>,
) {
    if let Some(mut ing) = ingest.take() {
        let _ = ing.rtc.close();
        while let Ok(output) = ing.rtc.poll_output() {
            match output {
                Output::Timeout(_) => break,
                Output::Transmit(t) => {
                    let _ = socket
                        .try_send_to(&t.contents, to_socket_addr(t.destination, is_dual_stack));
                }
                Output::Event(_) => {}
            }
        }
    }
    for shard in shards {
        let _ = shard.tx.send(ShardMsg::ClearViewers);
    }
    metrics.reset_ingest();
    let _ = signal_tx.try_send(SignalEvent::StreamStop);
}

// ============================================================================
// Dedicated OS-thread egress shard with its own UDP socket (zero Tokio overhead)
// ============================================================================

fn run_egress_shard(
    shard_id: usize,
    rx: std_mpsc::Receiver<ShardMsg>,
    socket: std::net::UdpSocket,
    bound: BoundCandidates,
    counters: Arc<ShardCounters>,
    pli_tx: mpsc::Sender<()>,
    dtls_cert: Option<DtlsCert>,
) {
    let mut viewers: HashMap<String, ViewerSession> = HashMap::new();
    let mut addr_to_viewer: HashMap<SocketAddr, String> = HashMap::new();
    let mut udp_buf = vec![0u8; 2048];
    let mut congested = false;

    loop {
        if viewers.is_empty() {
            // 0% CPU when idle: block until a viewer or control message arrives.
            match rx.recv() {
                Ok(msg) => {
                    if !handle_shard_msg(
                        shard_id,
                        msg,
                        &mut viewers,
                        &mut addr_to_viewer,
                        &mut congested,
                        &socket,
                        &bound,
                        &counters,
                        &pli_tx,
                        dtls_cert.as_ref(),
                    ) {
                        break;
                    }
                    for _ in 1..MAX_SHARD_BATCH {
                        match rx.try_recv() {
                            Ok(m) => {
                                if !handle_shard_msg(
                                    shard_id,
                                    m,
                                    &mut viewers,
                                    &mut addr_to_viewer,
                                    &mut congested,
                                    &socket,
                                    &bound,
                                    &counters,
                                    &pli_tx,
                                    dtls_cert.as_ref(),
                                ) {
                                    return;
                                }
                            }
                            Err(TryRecvError::Empty) => break,
                            Err(TryRecvError::Disconnected) => return,
                        }
                    }
                }
                Err(_) => break,
            }
        } else {
            let now = Instant::now();
            let deadline = compute_shard_deadline(now, &viewers);
            let wait = deadline
                .saturating_duration_since(now)
                .min(SHARD_UDP_POLL_CAP);

            match rx.recv_timeout(wait) {
                Ok(msg) => {
                    if !handle_shard_msg(
                        shard_id,
                        msg,
                        &mut viewers,
                        &mut addr_to_viewer,
                        &mut congested,
                        &socket,
                        &bound,
                        &counters,
                        &pli_tx,
                        dtls_cert.as_ref(),
                    ) {
                        break;
                    }
                    for _ in 1..MAX_SHARD_BATCH {
                        match rx.try_recv() {
                            Ok(m) => {
                                if !handle_shard_msg(
                                    shard_id,
                                    m,
                                    &mut viewers,
                                    &mut addr_to_viewer,
                                    &mut congested,
                                    &socket,
                                    &bound,
                                    &counters,
                                    &pli_tx,
                                    dtls_cert.as_ref(),
                                ) {
                                    return;
                                }
                            }
                            Err(TryRecvError::Empty) => break,
                            Err(TryRecvError::Disconnected) => return,
                        }
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }

        if viewers.is_empty() {
            continue;
        }

        // Drain incoming viewer STUN/DTLS/RTCP/NACK packets directly on this shard's socket.
        drain_shard_udp(
            &socket,
            &mut udp_buf,
            &bound,
            &mut viewers,
            &mut addr_to_viewer,
            &counters,
            &pli_tx,
        );

        // Drive due viewer timeouts and reap closed/timed-out sessions on every pass so
        // continuous RTP traffic can never starve timeouts or leak closed sessions.
        tick_and_reap_viewers(
            Instant::now(),
            &mut viewers,
            &mut addr_to_viewer,
            &socket,
            bound.is_dual_stack,
            &counters,
            &pli_tx,
        );
    }
}

fn tick_and_reap_viewers(
    now: Instant,
    viewers: &mut HashMap<String, ViewerSession>,
    addr_to_viewer: &mut HashMap<SocketAddr, String>,
    socket: &std::net::UdpSocket,
    is_dual_stack: bool,
    counters: &ShardCounters,
    pli_tx: &mpsc::Sender<()>,
) {
    let mut dead_viewers: Vec<(String, bool)> = Vec::new();
    let mut want_pli = false;
    let mut stats_dirty = false;

    for (id, v) in viewers.iter_mut() {
        if v.rtc.is_alive() && now >= v.next_timeout {
            if let Err(e) = v.rtc.handle_input(Input::Timeout(now)) {
                debug!(viewer = %id, err = %e, "Viewer timeout error");
            }
            let (pli, dirty) = drain_viewer(v, socket, is_dual_stack, counters);
            want_pli |= pli;
            stats_dirty |= dirty;
        }

        if !v.rtc.is_alive() {
            dead_viewers.push((id.clone(), v.connected));
        } else if !v.connected && now.duration_since(v.created_at) >= VIEWER_HANDSHAKE_TIMEOUT {
            dead_viewers.push((id.clone(), false));
        }
    }

    if !dead_viewers.is_empty() {
        let removed = dead_viewers.len();
        for (id, was_connected) in dead_viewers {
            if let Some(mut v) = viewers.remove(&id) {
                if let Some(addr) = v.remote_addr {
                    remove_addr_mapping(addr_to_viewer, addr, &id);
                }
                if v.rtc.is_alive() {
                    close_viewer(&mut v, socket, is_dual_stack);
                }
            }
            if was_connected {
                info!(viewer = %id, "Viewer disconnected");
            } else {
                debug!(viewer = %id, "Viewer handshake timed out");
            }
        }
        counters.viewers.fetch_sub(removed, Ordering::Relaxed);
        sync_shard_stats(viewers, counters);
    } else if stats_dirty {
        sync_shard_stats(viewers, counters);
    }

    if want_pli {
        let _ = pli_tx.try_send(());
    }
}

fn remove_addr_mapping(
    addr_to_viewer: &mut HashMap<SocketAddr, String>,
    addr: SocketAddr,
    viewer_id: &str,
) {
    if addr_to_viewer
        .get(&addr)
        .is_some_and(|mapped| mapped == viewer_id)
    {
        addr_to_viewer.remove(&addr);
    }
}

#[allow(clippy::too_many_arguments)]
fn drain_shard_udp(
    socket: &std::net::UdpSocket,
    udp_buf: &mut [u8],
    bound: &BoundCandidates,
    viewers: &mut HashMap<String, ViewerSession>,
    addr_to_viewer: &mut HashMap<SocketAddr, String>,
    counters: &ShardCounters,
    pli_tx: &mpsc::Sender<()>,
) {
    let mut want_pli = false;
    let mut stats_dirty = false;

    for _ in 0..MAX_UDP_BATCH {
        let (n, raw_source) = match socket.recv_from(udp_buf) {
            Ok(pair) => pair,
            Err(e) if e.kind() == ErrorKind::WouldBlock => break,
            Err(e) => {
                counters.record_udp_drop();
                trace!(err = %e, "Shard UDP recv_from error");
                break;
            }
        };

        let source = from_socket_addr(raw_source);
        let dest = local_dest_for(source, bound);
        let Ok(receive) = Receive::new(Protocol::Udp, source, dest, &udp_buf[..n]) else {
            continue;
        };
        let input = Input::Receive(Instant::now(), receive);

        // Fast O(1) dispatch for known viewer SocketAddrs.
        if let Some(vid) = addr_to_viewer.get(&source)
            && let Some(v) = viewers.get_mut(vid)
            && v.rtc.accepts(&input)
        {
            if let Err(e) = v.rtc.handle_input(input) {
                debug!(viewer = %vid, err = %e, "Viewer handle_input error");
            }
            let (pli, dirty) = drain_viewer(v, socket, bound.is_dual_stack, counters);
            want_pli |= pli;
            stats_dirty |= dirty;
            continue;
        }

        // Initial STUN/ICE binding or NAT rebind: scan this shard's viewers and cache the mapping.
        let target_id = viewers
            .iter()
            .find(|(_, v)| v.rtc.accepts(&input))
            .map(|(id, _)| id.clone());
        if let Some(vid) = target_id
            && let Some(v) = viewers.get_mut(&vid)
        {
            if let Some(old) = v.remote_addr.replace(source) {
                remove_addr_mapping(addr_to_viewer, old, &vid);
            }
            addr_to_viewer.insert(source, vid.clone());
            if let Err(e) = v.rtc.handle_input(input) {
                debug!(viewer = %vid, err = %e, "Viewer handle_input error");
            }
            let (pli, dirty) = drain_viewer(v, socket, bound.is_dual_stack, counters);
            want_pli |= pli;
            stats_dirty |= dirty;
        }
    }

    if stats_dirty {
        sync_shard_stats(viewers, counters);
    }
    if want_pli {
        let _ = pli_tx.try_send(());
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_shard_msg(
    shard_id: usize,
    msg: ShardMsg,
    viewers: &mut HashMap<String, ViewerSession>,
    addr_to_viewer: &mut HashMap<SocketAddr, String>,
    congested: &mut bool,
    socket: &std::net::UdpSocket,
    bound: &BoundCandidates,
    counters: &ShardCounters,
    pli_tx: &mpsc::Sender<()>,
    dtls_cert: Option<&DtlsCert>,
) -> bool {
    match msg {
        ShardMsg::Rtp(pkt) => {
            fanout_rtp_shard(
                &pkt,
                viewers,
                *congested,
                socket,
                bound.is_dual_stack,
                counters,
                pli_tx,
            );
            true
        }
        ShardMsg::AddViewer {
            viewer_id,
            sdp,
            video_codec,
            reply,
        } => {
            let res = build_viewer_session(
                viewer_id.clone(),
                sdp,
                video_codec,
                bound,
                dtls_cert,
                socket,
                counters,
            );
            match res {
                Ok((viewer, answer_sdp)) => {
                    if let Some(mut old) = viewers.insert(viewer_id.clone(), viewer) {
                        if let Some(addr) = old.remote_addr {
                            remove_addr_mapping(addr_to_viewer, addr, &old.id);
                        }
                        close_viewer(&mut old, socket, bound.is_dual_stack);
                        // Undo extra pre-increment when replacing an existing viewer_id.
                        counters.viewers.fetch_sub(1, Ordering::Relaxed);
                        sync_shard_stats(viewers, counters);
                    }
                    debug!(
                        viewer = %viewer_id,
                        shard = shard_id,
                        shard_viewers = viewers.len(),
                        "Viewer offer accepted"
                    );
                    let _ = reply.send(Ok(answer_sdp));
                }
                Err(e) => {
                    counters.viewers.fetch_sub(1, Ordering::Relaxed);
                    let _ = reply.send(Err(e));
                }
            }
            true
        }
        ShardMsg::SetCongested(new_congested) => {
            *congested = new_congested;
            let ratio_cap = if new_congested {
                Some(RTX_CONGESTED_RATIO_CAP)
            } else {
                Some(RTX_RATIO_CAP)
            };
            let mut want_pli = false;
            let mut stats_dirty = false;
            for v in viewers.values_mut() {
                if let Some(mid) = v.video_mid
                    && let Some(tx) = v.rtc.direct_api().stream_tx_by_mid(mid, None)
                {
                    tx.set_rtx_cache(RTX_CACHE_PACKETS, RTX_CACHE_AGE, ratio_cap);
                }
                let (pli, dirty) = drain_viewer(v, socket, bound.is_dual_stack, counters);
                want_pli |= pli;
                stats_dirty |= dirty;
            }
            if stats_dirty {
                sync_shard_stats(viewers, counters);
            }
            if want_pli {
                let _ = pli_tx.try_send(());
            }
            true
        }
        ShardMsg::ClearViewers => {
            let removed = viewers.len();
            for (_, mut v) in viewers.drain() {
                close_viewer(&mut v, socket, bound.is_dual_stack);
            }
            addr_to_viewer.clear();
            if removed > 0 {
                counters.viewers.fetch_sub(removed, Ordering::Relaxed);
            }
            counters.connected_viewers.store(0, Ordering::Relaxed);
            counters.set_peer_stats(None, None);
            *congested = false;
            true
        }
        ShardMsg::Shutdown => {
            for (_, mut v) in viewers.drain() {
                close_viewer(&mut v, socket, bound.is_dual_stack);
            }
            addr_to_viewer.clear();
            counters.reset();
            false
        }
    }
}

fn build_viewer_session(
    viewer_id: String,
    sdp: String,
    video_codec: Option<Codec>,
    bound: &BoundCandidates,
    dtls_cert: Option<&DtlsCert>,
    socket: &std::net::UdpSocket,
    counters: &ShardCounters,
) -> Result<(ViewerSession, String), String> {
    let offer = SdpOffer::from_sdp_string(&sdp).map_err(|e| format!("Invalid viewer SDP: {e}"))?;
    let now = Instant::now();

    let mut rtc_cfg = RtcConfig::new()
        .set_rtp_mode(true)
        .set_stats_interval(Some(Duration::from_secs(1)));

    if let Some(cert) = dtls_cert {
        rtc_cfg = rtc_cfg.set_dtls_cert(cert.clone());
    }

    if let Some(codec) = video_codec {
        rtc_cfg = rtc_cfg.clear_codecs().enable_opus(true, false);
        rtc_cfg = match codec {
            Codec::Av1 => {
                rtc_cfg = rtc_cfg.enable_av1(true);
                register_offer_av1_formats(rtc_cfg.codec_config(), &sdp);
                rtc_cfg
            }
            Codec::H264 => rtc_cfg.enable_h264(true),
            Codec::H265 => rtc_cfg.enable_h265(true),
            Codec::Vp8 => rtc_cfg.enable_vp8(true),
            Codec::Vp9 => rtc_cfg.enable_vp9(true),
            _ => {
                rtc_cfg = rtc_cfg
                    .enable_av1(true)
                    .enable_h264(true)
                    .enable_h265(true)
                    .enable_vp8(true)
                    .enable_vp9(true);
                register_offer_av1_formats(rtc_cfg.codec_config(), &sdp);
                rtc_cfg
            }
        };
    } else {
        register_offer_av1_formats(rtc_cfg.codec_config(), &sdp);
    }

    let mut rtc = rtc_cfg.build(now);
    for c in &bound.candidates {
        rtc.add_local_candidate(c.clone());
    }

    let answer = rtc
        .sdp_api()
        .accept_offer(offer)
        .map_err(|e| format!("Failed to accept viewer offer: {e}"))?;
    let answer_sdp = answer.to_sdp_string();

    if let Some(codec) = video_codec
        && !answer_has_active_video(&answer_sdp)
    {
        warn!(
            viewer = %viewer_id,
            codec = %codec,
            "Viewer does not support stream video codec"
        );
        return Err(format!("Unsupported video codec ({codec})"));
    }

    let mut viewer = ViewerSession {
        id: viewer_id,
        rtc,
        next_timeout: now,
        created_at: now,
        connected: false,
        remote_addr: None,
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

    let _ = viewer.rtc.handle_input(Input::Timeout(now));
    let _ = drain_viewer(&mut viewer, socket, bound.is_dual_stack, counters);

    Ok((viewer, answer_sdp))
}

fn fanout_rtp_shard(
    pkt: &SharedRtp,
    viewers: &mut HashMap<String, ViewerSession>,
    congested: bool,
    socket: &std::net::UdpSocket,
    is_dual_stack: bool,
    counters: &ShardCounters,
    pli_tx: &mpsc::Sender<()>,
) {
    let nackable = pkt.is_video;
    let payload_len = pkt.payload.len();
    let mut want_pli = false;
    let mut stats_dirty = false;

    for v in viewers.values_mut() {
        if !v.connected || !v.rtc.is_alive() {
            continue;
        }
        let (mid_opt, pt_opt, seq_counter) = if pkt.is_video {
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
            if pkt.is_video && !v.rtx_configured {
                let ratio_cap = if congested {
                    Some(RTX_CONGESTED_RATIO_CAP)
                } else {
                    Some(RTX_RATIO_CAP)
                };
                stream_tx.set_rtx_cache(RTX_CACHE_PACKETS, RTX_CACHE_AGE, ratio_cap);
                v.rtx_configured = true;
            }

            // Zero-copy Arc<[u8]> clone across all viewers in this shard.
            let write = RtpWrite::new(
                pt,
                seq,
                pkt.timestamp,
                pkt.wall_time,
                Arc::clone(&pkt.payload),
            )
            .marker(pkt.marker)
            .nackable(nackable);

            stream_tx.write_rtp(write);
            counters.record_out(payload_len);
        }

        // Single-mutation invariant: drain immediately after write_rtp.
        let (pli, dirty) = drain_viewer(v, socket, is_dual_stack, counters);
        want_pli |= pli;
        stats_dirty |= dirty;
    }

    if stats_dirty {
        sync_shard_stats(viewers, counters);
    }
    if want_pli {
        let _ = pli_tx.try_send(());
    }
}

/// Drains a viewer's `Rtc` output queue.
/// Returns `(want_pli, stats_or_conn_changed)`.
fn drain_viewer(
    v: &mut ViewerSession,
    socket: &std::net::UdpSocket,
    is_dual_stack: bool,
    counters: &ShardCounters,
) -> (bool, bool) {
    let mut want_pli = false;
    let mut dirty = false;

    loop {
        match v.rtc.poll_output() {
            Ok(Output::Timeout(t)) => {
                v.next_timeout = t;
                break;
            }
            Ok(Output::Transmit(t)) => {
                if let Err(e) =
                    socket.send_to(&t.contents, to_socket_addr(t.destination, is_dual_stack))
                {
                    counters.record_udp_drop();
                    trace!(viewer = %v.id, err = %e, "Viewer UDP send_to error");
                }
            }
            Ok(Output::Event(ev)) => match ev {
                Event::Connected => {
                    v.connected = true;
                    want_pli = true;
                    dirty = true;
                    info!(viewer = %v.id, "Viewer connected");
                }
                Event::IceConnectionStateChange(IceConnectionState::Disconnected) => {
                    let _ = v.rtc.close();
                    dirty = true;
                }
                Event::Closed => {
                    dirty = true;
                }
                Event::MediaAdded(m) => {
                    if let Some(p) = negotiated_param(&v.rtc, m.mid) {
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
                    dirty = true;
                }
                _ => {}
            },
            Err(e) => {
                debug!(viewer = %v.id, err = %e, "Viewer poll_output error");
                break;
            }
        }
    }

    (want_pli, dirty)
}

fn close_viewer(v: &mut ViewerSession, socket: &std::net::UdpSocket, is_dual_stack: bool) {
    let _ = v.rtc.close();
    while let Ok(output) = v.rtc.poll_output() {
        match output {
            Output::Timeout(_) => break,
            Output::Transmit(t) => {
                let _ = socket.send_to(&t.contents, to_socket_addr(t.destination, is_dual_stack));
            }
            Output::Event(_) => {}
        }
    }
}

fn compute_shard_deadline(now: Instant, viewers: &HashMap<String, ViewerSession>) -> Instant {
    let mut earliest = now + Duration::from_millis(50);
    for v in viewers.values() {
        earliest = earliest.min(v.next_timeout);
        if !v.connected {
            earliest = earliest.min(v.created_at + VIEWER_HANDSHAKE_TIMEOUT);
        }
    }
    earliest
}

fn sync_shard_stats(viewers: &HashMap<String, ViewerSession>, counters: &ShardCounters) {
    let connected = viewers
        .values()
        .filter(|v| v.connected && v.rtc.is_alive())
        .count();
    let max_rtt = viewers
        .values()
        .filter(|v| v.rtc.is_alive())
        .filter_map(|v| v.last_rtt)
        .max();
    let max_loss = viewers
        .values()
        .filter(|v| v.rtc.is_alive())
        .filter_map(|v| v.last_loss)
        .reduce(f32::max);
    counters.connected_viewers.store(connected, Ordering::Relaxed);
    counters.set_peer_stats(max_rtt, max_loss);
}

fn negotiated_param(rtc: &Rtc, mid: Mid) -> Option<PayloadParams> {
    let pt = rtc.media(mid)?.remote_pts().first().copied()?;
    rtc.codec_config()
        .params()
        .iter()
        .find(|p| p.pt() == pt)
        .copied()
}

fn answer_has_active_video(answer_sdp: &str) -> bool {
    answer_sdp.lines().any(|line| {
        let Some(rest) = line.trim().strip_prefix("m=video ") else {
            return false;
        };
        rest.split_whitespace()
            .next()
            .is_some_and(|port| port != "0")
    })
}

/// Workaround for str0m's exact-equality AV1 `level-idx`/`profile`/`tier` matching:
/// registers any AV1 `a=fmtp` parameters advertised by the remote SDP offer so
/// str0m negotiates AV1 regardless of the peer's advertised level-idx.
fn register_offer_av1_formats(cfg: &mut CodecConfig, sdp: &str) {
    let mut av1_pts: Vec<u8> = Vec::new();
    for line in sdp.lines().map(str::trim) {
        let Some(rest) = line.strip_prefix("a=rtpmap:") else {
            continue;
        };
        let Some((pt_str, codec)) = rest.split_once(' ') else {
            continue;
        };
        if codec.to_ascii_lowercase().starts_with("av1/90000")
            && let Ok(pt) = pt_str.parse::<u8>()
            && !av1_pts.contains(&pt)
        {
            av1_pts.push(pt);
        }
    }
    if av1_pts.is_empty() {
        return;
    }

    let mut fmtps: Vec<(Option<u8>, Option<u8>, Option<u8>)> = Vec::new();
    for line in sdp.lines().map(str::trim) {
        let Some(rest) = line.strip_prefix("a=fmtp:") else {
            continue;
        };
        let Some((pt_str, params)) = rest.split_once(' ') else {
            continue;
        };
        let Ok(pt) = pt_str.parse::<u8>() else {
            continue;
        };
        if !av1_pts.contains(&pt) {
            continue;
        }
        let (mut profile, mut level_idx, mut tier) = (None, None, None);
        for kv in params.split(';') {
            let Some((k, v)) = kv.trim().split_once('=') else {
                continue;
            };
            match k.trim().to_ascii_lowercase().as_str() {
                "profile" => profile = v.trim().parse::<u8>().ok(),
                "level-idx" => level_idx = v.trim().parse::<u8>().ok(),
                "tier" => tier = v.trim().parse::<u8>().ok(),
                _ => {}
            }
        }
        fmtps.push((profile, level_idx, tier));
    }

    for (profile, level_idx, tier) in fmtps {
        let norm = (profile.unwrap_or(0), level_idx.unwrap_or(5), tier.unwrap_or(0));
        let already_exists = cfg.params().iter().any(|p| {
            let s = p.spec();
            s.codec == Codec::Av1
                && (
                    s.format.profile.unwrap_or(0),
                    s.format.level_idx.unwrap_or(5),
                    s.format.tier.unwrap_or(0),
                ) == norm
        });
        if already_exists {
            continue;
        }

        let Some(pt) = next_free_pt(cfg) else {
            break;
        };
        let rtx_pt = next_free_pt_excluding(cfg, pt);
        cfg.add_config(
            pt,
            rtx_pt,
            Codec::Av1,
            Frequency::NINETY_KHZ,
            None,
            FormatParams {
                profile,
                level_idx,
                tier,
                ..Default::default()
            },
        );
    }
}

fn is_pt_used(cfg: &CodecConfig, candidate: u8) -> bool {
    let pt = Pt::new_with_value(candidate);
    cfg.params()
        .iter()
        .any(|p| p.pt() == pt || p.resend() == Some(pt) || p.red() == Some(pt))
}

fn next_free_pt(cfg: &CodecConfig) -> Option<Pt> {
    (35..=63)
        .chain(80..=127)
        .find(|&n| !is_pt_used(cfg, n))
        .map(Pt::new_with_value)
}

fn next_free_pt_excluding(cfg: &CodecConfig, exclude: Pt) -> Option<Pt> {
    (35..=63)
        .chain(80..=127)
        .find(|&n| n != *exclude && !is_pt_used(cfg, n))
        .map(Pt::new_with_value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use str0m::Candidate;

    fn test_bound_and_socket() -> (std::net::UdpSocket, BoundCandidates) {
        let sock = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        sock.set_nonblocking(true).unwrap();
        let addr = sock.local_addr().unwrap();
        let bound = BoundCandidates {
            local_port: addr.port(),
            loopback_addr: addr,
            host_addr: addr,
            v6_host_addr: None,
            is_dual_stack: false,
            candidates: vec![Candidate::host(addr, "udp").unwrap()],
        };
        (sock, bound)
    }

    fn sample_firefox_offer(video_sdp: &str) -> String {
        format!(
            "v=0\r\n\
             o=mozilla...THIS_IS_SDPARTA-99.0 12345 0 IN IP4 0.0.0.0\r\n\
             s=-\r\n\
             t=0 0\r\n\
             a=fingerprint:sha-256 00:11:22:33:44:55:66:77:88:99:AA:BB:CC:DD:EE:FF:00:11:22:33:44:55:66:77:88:99:AA:BB:CC:DD:EE:FF\r\n\
             a=group:BUNDLE 0 1\r\n\
             a=ice-options:trickle\r\n\
             a=msid-semantic:WMS *\r\n\
             {video_sdp}\
             m=audio 9 UDP/TLS/RTP/SAVPF 111\r\n\
             c=IN IP4 0.0.0.0\r\n\
             a=recvonly\r\n\
             a=ice-ufrag:testufrag\r\n\
             a=ice-pwd:testpassword123456789012\r\n\
             a=mid:1\r\n\
             a=rtcp-mux\r\n\
             a=setup:actpass\r\n\
             a=rtpmap:111 opus/48000/2\r\n"
        )
    }

    #[test]
    fn av1_non_default_level_idx_negotiates_video() {
        let (sock, bound) = test_bound_and_socket();
        let counters = ShardCounters::new();
        let offer = sample_firefox_offer(
            "m=video 9 UDP/TLS/RTP/SAVPF 41 42\r\n\
             c=IN IP4 0.0.0.0\r\n\
             a=recvonly\r\n\
             a=ice-ufrag:testufrag\r\n\
             a=ice-pwd:testpassword123456789012\r\n\
             a=mid:0\r\n\
             a=rtcp-mux\r\n\
             a=setup:actpass\r\n\
             a=rtpmap:41 AV1/90000\r\n\
             a=fmtp:41 level-idx=31;profile=0;tier=0\r\n\
             a=rtpmap:42 rtx/90000\r\n\
             a=fmtp:42 apt=41\r\n",
        );

        let (viewer, answer) = build_viewer_session(
            "v-av1".to_string(),
            offer,
            Some(Codec::Av1),
            &bound,
            None,
            &sock,
            &counters,
        )
        .expect("AV1 with level-idx=31 should negotiate");

        assert!(
            negotiated_param(&viewer.rtc, Mid::from("0"))
                .is_some_and(|p| p.spec().codec == Codec::Av1)
        );
        assert!(answer.contains("a=candidate:"));
        assert!(answer_has_active_video(&answer));
    }

    #[test]
    fn unsupported_h265_viewer_offer_is_rejected() {
        let (sock, bound) = test_bound_and_socket();
        let counters = ShardCounters::new();
        let offer = sample_firefox_offer(
            "m=video 9 UDP/TLS/RTP/SAVPF 120\r\n\
             c=IN IP4 0.0.0.0\r\n\
             a=recvonly\r\n\
             a=ice-ufrag:testufrag\r\n\
             a=ice-pwd:testpassword123456789012\r\n\
             a=mid:0\r\n\
             a=rtcp-mux\r\n\
             a=setup:actpass\r\n\
             a=rtpmap:120 VP8/90000\r\n",
        );

        let err = build_viewer_session(
            "v-h265".to_string(),
            offer,
            Some(Codec::H265),
            &bound,
            None,
            &sock,
            &counters,
        )
        .err()
        .expect("viewer without H265 should be rejected");

        assert!(err.contains("Unsupported video codec"));
    }

    #[test]
    fn viewer_session_arms_ice_timers_immediately() {
        let (sock, bound) = test_bound_and_socket();
        let counters = ShardCounters::new();
        let offer = sample_firefox_offer(
            "m=video 9 UDP/TLS/RTP/SAVPF 120\r\n\
             c=IN IP4 0.0.0.0\r\n\
             a=recvonly\r\n\
             a=ice-ufrag:testufrag\r\n\
             a=ice-pwd:testpassword123456789012\r\n\
             a=mid:0\r\n\
             a=rtcp-mux\r\n\
             a=setup:actpass\r\n\
             a=rtpmap:120 VP8/90000\r\n\
             a=candidate:1 1 UDP 1685987327 198.51.100.10 50000 typ srflx raddr 0.0.0.0 rport 0\r\n",
        );

        let before = Instant::now();
        let (viewer, _) = build_viewer_session(
            "v-ice".to_string(),
            offer,
            Some(Codec::Vp8),
            &bound,
            None,
            &sock,
            &counters,
        )
        .expect("viewer offer should be accepted");

        // str0m's IceAgent returns None (not_happening) until the first Input::Timeout(now).
        // Verify next_timeout is armed for active ICE pacing, not years in the future.
        assert!(
            viewer.next_timeout.saturating_duration_since(before) < Duration::from_secs(5),
            "ICE timer was not armed: next_timeout = {:?}",
            viewer.next_timeout.saturating_duration_since(before)
        );
    }

    #[test]
    fn closed_viewer_is_skipped_on_fanout_and_reaped_immediately() {
        let (sock, bound) = test_bound_and_socket();
        let counters = ShardCounters::new();
        let offer = sample_firefox_offer(
            "m=video 9 UDP/TLS/RTP/SAVPF 120\r\n\
             c=IN IP4 0.0.0.0\r\n\
             a=recvonly\r\n\
             a=ice-ufrag:testufrag\r\n\
             a=ice-pwd:testpassword123456789012\r\n\
             a=mid:0\r\n\
             a=rtcp-mux\r\n\
             a=setup:actpass\r\n\
             a=rtpmap:120 VP8/90000\r\n",
        );

        let (mut viewer, _) = build_viewer_session(
            "v-dead".to_string(),
            offer,
            Some(Codec::Vp8),
            &bound,
            None,
            &sock,
            &counters,
        )
        .expect("viewer offer should be accepted");

        let shared_addr: SocketAddr = "198.51.100.42:54321".parse().unwrap();
        viewer.connected = true;
        viewer.remote_addr = Some(shared_addr);
        viewer.rtc.disconnect();
        assert!(!viewer.rtc.is_alive());

        counters.viewers.store(1, Ordering::Relaxed);
        counters.connected_viewers.store(1, Ordering::Relaxed);

        let mut viewers = HashMap::new();
        viewers.insert("v-dead".to_string(), viewer);
        let mut addr_to_viewer = HashMap::new();
        // Simulate a newer viewer already owning the same SocketAddr.
        addr_to_viewer.insert(shared_addr, "v-newer".to_string());

        let (pli_tx, _pli_rx) = mpsc::channel(4);
        let pkt = SharedRtp {
            is_video: true,
            timestamp: 90_000,
            wall_time: Instant::now(),
            marker: true,
            payload: Arc::from([1u8, 2, 3, 4]),
        };

        fanout_rtp_shard(&pkt, &mut viewers, false, &sock, false, &counters, &pli_tx);
        let (out_bytes, out_pkts, _, _, _) = counters.take_interval();
        assert_eq!(out_bytes, 0, "closed viewer must not receive RTP fanout");
        assert_eq!(out_pkts, 0);

        tick_and_reap_viewers(
            Instant::now(),
            &mut viewers,
            &mut addr_to_viewer,
            &sock,
            false,
            &counters,
            &pli_tx,
        );

        assert!(viewers.is_empty(), "closed viewer must be reaped immediately");
        assert_eq!(counters.viewers.load(Ordering::Relaxed), 0);
        assert_eq!(counters.connected_viewers.load(Ordering::Relaxed), 0);
        assert_eq!(
            addr_to_viewer.get(&shared_addr).map(String::as_str),
            Some("v-newer"),
            "reaping old viewer must not clobber newer viewer's addr mapping"
        );
    }
}
