import dgram from "dgram";
import {
  RTCPeerConnection,
  MediaStreamTrack,
  RTCRtpCodecParameters,
  RtpPacket,
  type RTCOutboundRtpStreamStats,
  type RTCRemoteInboundRtpStreamStats,
  type RTCIceCandidatePairStats,
  wrapRtx,
  uint16Add,
} from "werift";

const UDP_BUFFER_BYTES = 16 * 1024 * 1024;
const RTP_CACHE_SIZE = 4096;
const RTX_MIN_COOLDOWN_MS = 20;
const RTX_MAX_COOLDOWN_MS = 200;
const RTX_MAX_FRACTION = 0.15;
const RTX_MIN_PPS = 150;

const origBind = dgram.Socket.prototype.bind;
dgram.Socket.prototype.bind = function (
  this: dgram.Socket,
  ...args: Parameters<dgram.Socket["bind"]>
) {
  this.once("listening", () => {
    try {
      this.setRecvBufferSize(UDP_BUFFER_BYTES);
      this.setSendBufferSize(UDP_BUFFER_BYTES);
    } catch {}
  });
  return origBind.apply(this, args);
} as dgram.Socket["bind"];

export type CodecConfig = {
  mimeType: string;
  clockRate: number;
  rtcpFeedback: { type: string; parameter?: string }[];
  parameters?: string;
};

export type PeerMetrics = {
  viewerId: string;
  connectedAt: string;
  disconnectedAt: string | null;
  connectionState: string;
  iceConnectionState: string;
  wallClockSkewMs: number;
  iceGatherDurationMs: number;
  transport: {
    localCandidate: string | null;
    remoteCandidate: string | null;
    candidateType: string | null;
    protocol: string | null;
    rttMs: number | null;
    avgRttMs: number | null;
    minRttMs: number | null;
    oneWayPingMs: number | null;
    bytesSent: number;
    packetsSent: number;
  };
  video: {
    ssrc: number | null;
    kbps: number;
    peakKbps: number;
    avgKbps: number;
    pps: number;
    packetsSent: number;
    bytesSent: number;
    retransmittedPacketsSent: number;
    nackCount: number;
    pliCount: number;
    firCount: number;
    remotePacketsLost: number;
    remoteFractionLost: number;
    remoteRttMs: number | null;
    remoteAvgRttMs: number | null;
    remoteMinRttMs: number | null;
  };
  audio: {
    ssrc: number | null;
    kbps: number;
    peakKbps: number;
    avgKbps: number;
    pps: number;
    packetsSent: number;
    bytesSent: number;
    remotePacketsLost: number;
    remoteRttMs: number | null;
    remoteAvgRttMs: number | null;
  };
};

const audioCodec = new RTCRtpCodecParameters({
  mimeType: "audio/opus",
  clockRate: 48000,
  channels: 2,
});

const toCodecParam = (c: CodecConfig) =>
  new RTCRtpCodecParameters({
    mimeType: c.mimeType,
    clockRate: c.clockRate,
    rtcpFeedback: c.rtcpFeedback,
    parameters: c.parameters,
  });

let rtxEnabled = false;
let activeVideoCodec = new RTCRtpCodecParameters({
  mimeType: "video/AV1",
  clockRate: 90000,
});
let sharedVideoTrack = new MediaStreamTrack({
  kind: "video",
  codec: activeVideoCodec,
});
const sharedAudioTrack = new MediaStreamTrack({
  kind: "audio",
  codec: audioCodec,
});

const setVideoCodec = (codec: CodecConfig) => {
  const next = toCodecParam(codec);
  const changed =
    next.mimeType !== activeVideoCodec.mimeType ||
    next.parameters !== activeVideoCodec.parameters;
  activeVideoCodec = next;
  if (!changed) return;
  sharedVideoTrack = new MediaStreamTrack({
    kind: "video",
    codec: activeVideoCodec,
  });
  videoRtpHistory?.fill(undefined);
  for (const viewerId of [...peers.keys()]) {
    cleanupPeer(viewerId, "codec-change");
  }
};

const peers = new Map<string, RTCPeerConnection>();
const peerMetricsMap = new Map<string, PeerMetrics>();
const handshakeTimers = new Map<string, ReturnType<typeof setTimeout>>();
const pliTimers = new Map<string, ReturnType<typeof setTimeout>>();
const prevVideoLoss = new Map<string, { lost: number; sent: number }>();

const HANDSHAKE_TIMEOUT_MS = 15_000;
const ICE_GATHER_TIMEOUT_MS = 5_000;

const cleanupPeer = (viewerId: string, reason: string, notify = true) => {
  const timer = handshakeTimers.get(viewerId);
  if (timer) {
    clearTimeout(timer);
    handshakeTimers.delete(viewerId);
  }
  const pliTimer = pliTimers.get(viewerId);
  if (pliTimer) {
    clearTimeout(pliTimer);
    pliTimers.delete(viewerId);
  }
  const pc = peers.get(viewerId);
  if (!pc) return;
  const detail =
    reason === "failed" || reason === "timeout"
      ? `${reason} (ice=${pc.iceConnectionState}, dtls=${pc.dtlsTransports.map((d) => d.state).join(",") || "-"})`
      : reason;
  peers.delete(viewerId);
  prevVideoLoss.delete(viewerId);
  const prefix = `${viewerId}:`;
  for (const key of prevRtp.keys()) {
    if (key.startsWith(prefix)) prevRtp.delete(key);
  }
  for (const key of startRtp.keys()) {
    if (key.startsWith(prefix)) startRtp.delete(key);
  }
  const pm = peerMetricsMap.get(viewerId);
  if (pm) {
    pm.disconnectedAt ??= new Date().toISOString();
    peerMetricsMap.delete(viewerId);
  }
  pc.close();
  if (notify) {
    self.postMessage({ type: "state", viewerId, state: detail, closed: true });
  }
};
let videoRtpHistory: (RtpPacket | undefined)[] | null = null;

const prevRtp = new Map<
  string,
  { ts: number; bytes: number; packets: number }
>();
const startRtp = new Map<string, { ts: number; bytes: number }>();

const computeRates = (
  key: string,
  ts: number,
  bytes: number,
  packets: number,
) => {
  const prev = prevRtp.get(key);
  const first = startRtp.get(key) ?? { ts, bytes };
  if (!startRtp.has(key)) startRtp.set(key, first);

  const dtSec = prev ? (ts - prev.ts) / 1000 : 0;
  const totalSec = (ts - first.ts) / 1000;
  const kbps =
    dtSec > 0 ? Math.round(((bytes - prev!.bytes) * 8) / dtSec / 1000) : 0;
  const pps = dtSec > 0 ? Math.round((packets - prev!.packets) / dtSec) : 0;
  const avgKbps =
    totalSec > 0
      ? Math.round(((bytes - first.bytes) * 8) / totalSec / 1000)
      : kbps;
  prevRtp.set(key, { ts, bytes, packets });
  return { kbps, pps, avgKbps };
};

self.onmessage = async (event: MessageEvent) => {
  const msg = event.data;

  if (msg.type === "init") {
    rtxEnabled = Boolean(msg.rtx);
    videoRtpHistory = rtxEnabled
      ? new Array<RtpPacket | undefined>(RTP_CACHE_SIZE)
      : null;
    if (msg.videoCodec) {
      activeVideoCodec = toCodecParam(msg.videoCodec);
      sharedVideoTrack = new MediaStreamTrack({
        kind: "video",
        codec: activeVideoCodec,
      });
    }
    return;
  }

  if (msg.type === "codec") {
    videoRtpHistory?.fill(undefined);
    setVideoCodec(msg.videoCodec);
    return;
  }

  if (msg.type === "rtp") {
    if (peers.size === 0) return;
    const rtp = RtpPacket.deSerialize(Buffer.from(msg.data));
    if (msg.kind === "video") {
      if (videoRtpHistory) {
        videoRtpHistory[rtp.header.sequenceNumber % RTP_CACHE_SIZE] = rtp;
      }
      sharedVideoTrack.writeRtp(rtp);
    } else {
      sharedAudioTrack.writeRtp(rtp);
    }
    return;
  }

  if (msg.type === "offer") {
    const { viewerId, offerSdp, skewMs, videoCodec } = msg as {
      viewerId: string;
      offerSdp: string;
      skewMs: number;
      videoCodec: CodecConfig;
    };
    try {
      setVideoCodec(videoCodec);

      const pc = new RTCPeerConnection({
        bandwidthEstimator: "none",
        codecs: {
          video: rtxEnabled
            ? [
                toCodecParam(videoCodec),
                new RTCRtpCodecParameters({
                  mimeType: "video/rtx",
                  clockRate: 90000,
                }),
              ]
            : [toCodecParam(videoCodec)],
          audio: [audioCodec],
        },
        iceServers: [{ urls: "stun:stun.l.google.com:19302" }],
      });

      peers.set(viewerId, pc);
      const pm: PeerMetrics = {
        viewerId,
        connectedAt: new Date().toISOString(),
        disconnectedAt: null,
        connectionState: pc.connectionState,
        iceConnectionState: pc.iceConnectionState,
        wallClockSkewMs: skewMs,
        iceGatherDurationMs: 0,
        transport: {
          localCandidate: null,
          remoteCandidate: null,
          candidateType: null,
          protocol: null,
          rttMs: null,
          avgRttMs: null,
          minRttMs: null,
          oneWayPingMs: null,
          bytesSent: 0,
          packetsSent: 0,
        },
        video: {
          ssrc: null,
          kbps: 0,
          peakKbps: 0,
          avgKbps: 0,
          pps: 0,
          packetsSent: 0,
          bytesSent: 0,
          retransmittedPacketsSent: 0,
          nackCount: 0,
          pliCount: 0,
          firCount: 0,
          remotePacketsLost: 0,
          remoteFractionLost: 0,
          remoteRttMs: null,
          remoteAvgRttMs: null,
          remoteMinRttMs: null,
        },
        audio: {
          ssrc: null,
          kbps: 0,
          peakKbps: 0,
          avgKbps: 0,
          pps: 0,
          packetsSent: 0,
          bytesSent: 0,
          remotePacketsLost: 0,
          remoteRttMs: null,
          remoteAvgRttMs: null,
        },
      };
      peerMetricsMap.set(viewerId, pm);

      const videoSender = pc.addTrack(sharedVideoTrack);
      pc.addTrack(sharedAudioTrack);

      if (videoRtpHistory) {
        const history = videoRtpHistory;
        const rtxSentAt = new Float64Array(RTP_CACHE_SIZE);
        const rtxSentSeq = new Int32Array(RTP_CACHE_SIZE).fill(-1);
        let rtxWindowStartMs = 0;
        let rtxWindowSent = 0;

        videoSender.onGenericNack.subscribe(async (feedback) => {
          const sender = videoSender as unknown as {
            rtpCache: (RtpPacket | undefined)[];
            rtxPayloadType?: number;
            rtxSequenceNumber: number;
            rtxSsrc: number;
            ssrc: number;
            codec?: { payloadType: number };
            dispatchOutgoing: (
              fn: () => Promise<void>,
              meta: { kind: string; bytes: number },
            ) => Promise<void>;
            sendRtpInternal: (
              pkt: RtpPacket,
              opts: { injectProbePadding: boolean; isRetransmission: boolean },
            ) => Promise<void>;
          };
          const now = performance.now();
          if (now - rtxWindowStartMs >= 1000) {
            rtxWindowStartMs = now;
            rtxWindowSent = 0;
          }
          const maxRtxPerSec = Math.max(
            RTX_MIN_PPS,
            Math.round(pm.video.pps * RTX_MAX_FRACTION),
          );
          const rttMs = pm.transport.rttMs ?? pm.video.remoteRttMs ?? 25;
          const cooldownMs = Math.max(
            RTX_MIN_COOLDOWN_MS,
            Math.min(RTX_MAX_COOLDOWN_MS, rttMs),
          );

          for (const seqNum of feedback.lost) {
            if (rtxWindowSent >= maxRtxPerSec) break;
            const inSmallCache = sender.rtpCache[seqNum % 128];
            if (inSmallCache && inSmallCache.header.sequenceNumber === seqNum) {
              continue;
            }
            const slot = seqNum % RTP_CACHE_SIZE;
            if (
              rtxSentSeq[slot] === seqNum &&
              now - rtxSentAt[slot]! < cooldownMs
            ) {
              continue;
            }
            const cached = history[slot];
            if (!cached || cached.header.sequenceNumber !== seqNum) continue;
            rtxSentSeq[slot] = seqNum;
            rtxSentAt[slot] = now;
            rtxWindowSent++;
            let pkt = cached.clone();
            pkt.header.ssrc = sender.ssrc;
            if (sender.codec) pkt.header.payloadType = sender.codec.payloadType;
            if (sender.rtxPayloadType != null) {
              pkt = wrapRtx(
                pkt,
                sender.rtxPayloadType,
                sender.rtxSequenceNumber,
                sender.rtxSsrc,
              );
              sender.rtxSequenceNumber = uint16Add(sender.rtxSequenceNumber, 1);
            }
            await sender.dispatchOutgoing(
              () =>
                sender.sendRtpInternal(pkt, {
                  injectProbePadding: false,
                  isRetransmission: true,
                }),
              { kind: "rtx", bytes: pkt.payload.length + 24 },
            );
          }
        });
      }

      videoSender.onKeyFrameRequest.subscribe(() => {
        self.postMessage({ type: "pli", force: false });
      });

      pc.connectionStateChange.subscribe((state) => {
        pm.connectionState = state;
        pm.iceConnectionState = pc.iceConnectionState;
        if (state === "connected") {
          const timer = handshakeTimers.get(viewerId);
          if (timer) {
            clearTimeout(timer);
            handshakeTimers.delete(viewerId);
          }
          self.postMessage({ type: "pli", force: true });
          const existingPli = pliTimers.get(viewerId);
          if (existingPli) clearTimeout(existingPli);
          pliTimers.set(
            viewerId,
            setTimeout(() => {
              pliTimers.delete(viewerId);
              self.postMessage({ type: "pli", force: true });
            }, 500),
          );
        }
        if (state === "closed" || state === "failed") {
          cleanupPeer(viewerId, state);
          return;
        }
        self.postMessage({
          type: "state",
          viewerId,
          state,
          closed: false,
        });
      });

      pc.iceConnectionStateChange.subscribe((state) => {
        pm.iceConnectionState = state;
        if (state === "failed" || state === "closed") {
          cleanupPeer(viewerId, `ice-${state}`);
        }
      });

      const iceGatherStartMs = performance.now();
      await pc.setRemoteDescription({ type: "offer", sdp: offerSdp });
      await pc.setLocalDescription(await pc.createAnswer());

      for (const dtls of pc.dtlsTransports) {
        dtls.onStateChange.subscribe((state) => {
          if (state === "closed" || state === "failed") {
            cleanupPeer(viewerId, `dtls-${state}`);
          }
        });
      }

      if (pc.iceGatheringState !== "complete") {
        await new Promise<void>((resolve) => {
          const gatherTimer = setTimeout(resolve, ICE_GATHER_TIMEOUT_MS);
          pc.iceGatheringStateChange.subscribe((state) => {
            if (state === "complete") {
              clearTimeout(gatherTimer);
              resolve();
            }
          });
        });
      }
      pm.iceGatherDurationMs = Math.round(performance.now() - iceGatherStartMs);

      if (!peers.has(viewerId)) return;
      if (pc.connectionState !== "connected") {
        handshakeTimers.set(
          viewerId,
          setTimeout(
            () => cleanupPeer(viewerId, "timeout"),
            HANDSHAKE_TIMEOUT_MS,
          ),
        );
      }

      self.postMessage({
        type: "answer",
        viewerId,
        answerSdp: pc.localDescription!.sdp,
      });
    } catch (err) {
      cleanupPeer(viewerId, "offerError", false);
      self.postMessage({
        type: "offerError",
        viewerId,
        error: err instanceof Error ? err.message : String(err),
      });
    }
    return;
  }

  if (msg.type === "closeAll") {
    const reason = (msg.reason as string | undefined) ?? "shutdown";
    const notify = Boolean(msg.notify);
    for (const viewerId of [...peers.keys()]) {
      cleanupPeer(viewerId, reason, notify);
    }
    return;
  }

  if (msg.type === "getStats") {
    for (const [viewerId, pc] of peers.entries()) {
      const s = pc.connectionState;
      if (s === "closed" || s === "failed") {
        cleanupPeer(viewerId, s);
      } else {
        const closedDtls = pc.dtlsTransports.find(
          (d) => d.state === "closed" || d.state === "failed",
        );
        if (closedDtls) cleanupPeer(viewerId, `dtls-${closedDtls.state}`);
      }
    }
    const entries = [...peers.entries()];
    const sessions = await Promise.all(
      entries.map(async ([viewerId, pc]) => {
        try {
          const pm = peerMetricsMap.get(viewerId);
          if (!pm) return null;
          pm.connectionState = pc.connectionState;
          pm.iceConnectionState = pc.iceConnectionState;

          const report = await pc.getStats();
          let selectedPairId: string | undefined;
          let fallbackPair: RTCIceCandidatePairStats | undefined;
          let rawFractionLost: number | null = null;
          let hasRemoteVideo = false;
          for (const s of report.values()) {
            if (s.type === "outbound-rtp") {
              const r = s as RTCOutboundRtpStreamStats;
              const bytes = r.bytesSent ?? 0;
              const packets = r.packetsSent ?? 0;
              const { kbps, pps, avgKbps } = computeRates(
                `${viewerId}:${r.id}`,
                r.timestamp,
                bytes,
                packets,
              );
              if (r.kind === "video") {
                pm.video.ssrc = r.ssrc;
                pm.video.kbps = kbps;
                pm.video.peakKbps = Math.max(pm.video.peakKbps, kbps);
                pm.video.avgKbps = avgKbps;
                pm.video.pps = pps;
                pm.video.packetsSent = packets;
                pm.video.bytesSent = bytes;
                pm.video.retransmittedPacketsSent =
                  r.retransmittedPacketsSent ?? 0;
                pm.video.nackCount = r.nackCount ?? 0;
                pm.video.pliCount = r.pliCount ?? 0;
                pm.video.firCount = r.firCount ?? 0;
              } else if (r.kind === "audio") {
                pm.audio.ssrc = r.ssrc;
                pm.audio.kbps = kbps;
                pm.audio.peakKbps = Math.max(pm.audio.peakKbps, kbps);
                pm.audio.avgKbps = avgKbps;
                pm.audio.pps = pps;
                pm.audio.packetsSent = packets;
                pm.audio.bytesSent = bytes;
              }
            } else if (s.type === "remote-inbound-rtp") {
              const r = s as RTCRemoteInboundRtpStreamStats;
              const rawLost = r.packetsLost ?? 0;
              const packetsLost =
                rawLost >= 0x800000 ? rawLost - 0x1000000 : rawLost;
              const rttMs =
                r.roundTripTime != null
                  ? Math.round(r.roundTripTime * 1000)
                  : null;
              const measurements = r.roundTripTimeMeasurements ?? 0;
              const avgRttMs =
                measurements > 0 && r.totalRoundTripTime != null
                  ? Math.round((r.totalRoundTripTime / measurements) * 1000)
                  : rttMs;
              if (r.kind === "video") {
                pm.video.remotePacketsLost = packetsLost;
                rawFractionLost = r.fractionLost ?? 0;
                hasRemoteVideo = true;
                pm.video.remoteRttMs = rttMs;
                pm.video.remoteAvgRttMs = avgRttMs;
                if (rttMs != null) {
                  pm.video.remoteMinRttMs =
                    pm.video.remoteMinRttMs == null
                      ? rttMs
                      : Math.min(pm.video.remoteMinRttMs, rttMs);
                }
              } else if (r.kind === "audio") {
                pm.audio.remotePacketsLost = packetsLost;
                pm.audio.remoteRttMs = rttMs;
                pm.audio.remoteAvgRttMs = avgRttMs;
              }
            } else if (s.type === "transport") {
              const id = (s as { selectedCandidatePairId?: string })
                .selectedCandidatePairId;
              if (id) selectedPairId = id;
            } else if (s.type === "candidate-pair") {
              const pair = s as RTCIceCandidatePairStats;
              if (
                !fallbackPair?.nominated &&
                (pair.nominated ||
                  (!fallbackPair && pair.state === "succeeded"))
              ) {
                fallbackPair = pair;
              }
            }
          }

          if (hasRemoteVideo) {
            const prev = prevVideoLoss.get(viewerId);
            const curLost = pm.video.remotePacketsLost;
            const curSent = pm.video.packetsSent;
            let frac = rawFractionLost ?? 0;
            if (prev) {
              const dLost = Math.max(0, curLost - prev.lost);
              const dSent = curSent - prev.sent;
              frac = dSent > 0 ? Math.min(1, dLost / dSent) : 0;
            }
            prevVideoLoss.set(viewerId, { lost: curLost, sent: curSent });
            pm.video.remoteFractionLost = Number(frac.toFixed(4));
          }

          const pair =
            (selectedPairId
              ? (report.get(selectedPairId) as
                  RTCIceCandidatePairStats | undefined)
              : undefined) ?? fallbackPair;
          if (pair) {
            const local = report.get(pair.localCandidateId) as
              Record<string, unknown> | undefined;
            const remote = report.get(pair.remoteCandidateId) as
              Record<string, unknown> | undefined;
            const rttMs =
              pair.currentRoundTripTime != null
                ? Math.round(pair.currentRoundTripTime * 1000)
                : null;
            const measurements = pair.roundTripTimeMeasurements ?? 0;
            const avgRttMs =
              measurements > 0 && pair.totalRoundTripTime != null
                ? Math.round((pair.totalRoundTripTime / measurements) * 1000)
                : rttMs;
            pm.transport.localCandidate = local
              ? `${local.address}:${local.port} (${local.candidateType})`
              : null;
            pm.transport.remoteCandidate = remote
              ? `${remote.address}:${remote.port} (${remote.candidateType})`
              : null;
            pm.transport.candidateType =
              (local?.candidateType as string | undefined) ?? null;
            pm.transport.protocol =
              (local?.protocol as string | undefined) ?? null;
            pm.transport.rttMs = rttMs;
            pm.transport.avgRttMs = avgRttMs;
            if (rttMs != null) {
              pm.transport.minRttMs =
                pm.transport.minRttMs == null
                  ? rttMs
                  : Math.min(pm.transport.minRttMs, rttMs);
            }
            pm.transport.oneWayPingMs =
              rttMs != null ? Math.round(rttMs / 2) : null;
            pm.transport.bytesSent = pair.bytesSent ?? 0;
            pm.transport.packetsSent = pair.packetsSent ?? 0;
          }
          return pm;
        } catch {
          return null;
        }
      }),
    );

    self.postMessage({
      type: "stats",
      reqId: msg.reqId,
      sessions: sessions.filter((s): s is PeerMetrics => s !== null),
    });
  }
};
