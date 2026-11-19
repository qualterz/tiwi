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

const origCreateSocket = dgram.createSocket;
dgram.createSocket = ((...args: Parameters<typeof dgram.createSocket>) => {
  const sock = origCreateSocket(...args);
  sock.once("listening", () => {
    try {
      sock.setRecvBufferSize(UDP_BUFFER_BYTES);
      sock.setSendBufferSize(UDP_BUFFER_BYTES);
    } catch {}
  });
  return sock;
}) as typeof dgram.createSocket;

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

let isStable = true;
let activeVideoCodec = new RTCRtpCodecParameters({
  mimeType: "video/AV1",
  clockRate: 90000,
});
const sharedVideoTrack = new MediaStreamTrack({
  kind: "video",
  codec: activeVideoCodec,
});
const sharedAudioTrack = new MediaStreamTrack({
  kind: "audio",
  codec: audioCodec,
});

const peers = new Map<string, RTCPeerConnection>();
const peerMetricsMap = new Map<string, PeerMetrics>();
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
    isStable = msg.profile === "stable";
    videoRtpHistory = isStable
      ? new Array<RtpPacket | undefined>(RTP_CACHE_SIZE)
      : null;
    if (msg.videoCodec) {
      activeVideoCodec = toCodecParam(msg.videoCodec);
      sharedVideoTrack.codec = activeVideoCodec;
    }
    return;
  }

  if (msg.type === "codec") {
    activeVideoCodec = toCodecParam(msg.videoCodec);
    sharedVideoTrack.codec = activeVideoCodec;
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
      activeVideoCodec = toCodecParam(videoCodec);
      sharedVideoTrack.codec = activeVideoCodec;

      const pc = new RTCPeerConnection({
        bandwidthEstimator: "none",
        codecs: {
          video: isStable
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
          for (const seqNum of feedback.lost) {
            const inSmallCache = sender.rtpCache[seqNum % 128];
            if (inSmallCache && inSmallCache.header.sequenceNumber === seqNum) {
              continue;
            }
            const cached = history[seqNum % RTP_CACHE_SIZE];
            if (!cached || cached.header.sequenceNumber !== seqNum) continue;
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

      pc.onIceCandidate.subscribe((c) => {
        if (c)
          console.debug(
            `[Peer ${viewerId.slice(0, 6)}] ICE candidate:`,
            c.candidate,
          );
      });

      pc.connectionStateChange.subscribe((state) => {
        pm.connectionState = state;
        pm.iceConnectionState = pc.iceConnectionState;
        if (state === "connected") {
          self.postMessage({ type: "pli", force: true });
          setTimeout(() => self.postMessage({ type: "pli", force: true }), 500);
        }
        const closed =
          state === "closed" || state === "failed" || state === "disconnected";
        if (closed) {
          pm.disconnectedAt ??= new Date().toISOString();
          pc.close();
          peers.delete(viewerId);
          peerMetricsMap.delete(viewerId);
        }
        self.postMessage({
          type: "state",
          viewerId,
          state,
          closed,
        });
      });

      const iceGatherStartMs = performance.now();
      await pc.setRemoteDescription({ type: "offer", sdp: offerSdp });
      await pc.setLocalDescription(await pc.createAnswer());

      if (pc.iceGatheringState !== "complete") {
        await new Promise<void>((resolve) => {
          pc.iceGatheringStateChange.subscribe((state) => {
            if (state === "complete") resolve();
          });
        });
      }
      pm.iceGatherDurationMs = Math.round(performance.now() - iceGatherStartMs);

      self.postMessage({
        type: "answer",
        viewerId,
        answerSdp: pc.localDescription!.sdp,
      });
    } catch (err) {
      peers.delete(viewerId);
      peerMetricsMap.delete(viewerId);
      self.postMessage({
        type: "offerError",
        viewerId,
        error: err instanceof Error ? err.message : String(err),
      });
    }
    return;
  }

  if (msg.type === "getStats") {
    const entries = [...peers.entries()].filter(
      ([, pc]) => pc.connectionState !== "closed",
    );
    const sessions = await Promise.all(
      entries.map(async ([viewerId, pc]) => {
        const pm = peerMetricsMap.get(viewerId);
        if (!pm) return null;
        pm.connectionState = pc.connectionState;
        pm.iceConnectionState = pc.iceConnectionState;

        const report = await pc.getStats();
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
              pm.video.remotePacketsLost = r.packetsLost ?? 0;
              pm.video.remoteFractionLost = Number(
                (r.fractionLost ?? 0).toFixed(4),
              );
              pm.video.remoteRttMs = rttMs;
              pm.video.remoteAvgRttMs = avgRttMs;
            } else if (r.kind === "audio") {
              pm.audio.remotePacketsLost = r.packetsLost ?? 0;
              pm.audio.remoteRttMs = rttMs;
              pm.audio.remoteAvgRttMs = avgRttMs;
            }
          } else if (
            s.type === "candidate-pair" &&
            ((s as RTCIceCandidatePairStats).nominated ||
              (s as RTCIceCandidatePairStats).state === "succeeded")
          ) {
            const pair = s as RTCIceCandidatePairStats;
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
            pm.transport.oneWayPingMs =
              rttMs != null ? Math.round(rttMs / 2) : null;
            pm.transport.bytesSent = pair.bytesSent ?? 0;
            pm.transport.packetsSent = pair.packetsSent ?? 0;
          }
        }
        return pm;
      }),
    );

    self.postMessage({
      type: "stats",
      reqId: msg.reqId,
      sessions: sessions.filter((s): s is PeerMetrics => s !== null),
    });
  }
};
