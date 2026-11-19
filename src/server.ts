import dgram from "dgram";
import { parseArgs } from "util";
import {
  RTCPeerConnection,
  MediaStreamTrack,
  RTCRtpCodecParameters,
  type RTCRtpReceiver,
  type RTCInboundRtpStreamStats,
  type RTCOutboundRtpStreamStats,
  type RTCRemoteInboundRtpStreamStats,
  type RTCIceCandidatePairStats,
} from "werift";
import mqtt from "mqtt";

const METRICS_INTERVAL_MS = 5000;
const UDP_BUFFER_BYTES = 16 * 1024 * 1024;
const PLI_COOLDOWN_MS = 500;

// Enlarge OS UDP socket buffers (default 64KB on Windows overflows during 30-55Mbps keyframe bursts)
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

// 1. Parse optional --token flag or generate a random secret token, then derive public streamId + AES-GCM key
const { values } = parseArgs({
  args: Bun.argv.slice(2),
  options: {
    token: { type: "string" },
  },
});

const token =
  values.token ||
  Buffer.from(crypto.getRandomValues(new Uint8Array(32))).toString("base64url");
const tokenBytes = new TextEncoder().encode(token);

const hmacKey = await crypto.subtle.importKey(
  "raw",
  tokenBytes,
  { name: "HMAC", hash: "SHA-256" },
  false,
  ["sign"],
);
const streamId = Buffer.from(
  await crypto.subtle.sign(
    "HMAC",
    hmacKey,
    new TextEncoder().encode("stream-id"),
  ),
).toString("base64url");

const aesKey = await crypto.subtle.importKey(
  "raw",
  await crypto.subtle.digest("SHA-256", tokenBytes),
  { name: "AES-GCM" },
  false,
  ["encrypt", "decrypt"],
);

// 2. Candidate codecs so OBS can connect with AV1, H.264, HEVC, VP9, or VP8
const rtcpFeedback = [
  { type: "nack" },
  { type: "nack", parameter: "pli" },
  { type: "ccm", parameter: "fir" },
];

const supportedCodecs = {
  video: [
    new RTCRtpCodecParameters({
      mimeType: "video/AV1",
      clockRate: 90000,
      rtcpFeedback,
    }),
    new RTCRtpCodecParameters({
      mimeType: "video/H264",
      clockRate: 90000,
      rtcpFeedback,
      parameters:
        "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f",
    }),
    new RTCRtpCodecParameters({
      mimeType: "video/H265",
      clockRate: 90000,
      rtcpFeedback,
    }),
    new RTCRtpCodecParameters({
      mimeType: "video/VP9",
      clockRate: 90000,
      rtcpFeedback,
    }),
    new RTCRtpCodecParameters({
      mimeType: "video/VP8",
      clockRate: 90000,
      rtcpFeedback,
    }),
  ],
  audio: [
    new RTCRtpCodecParameters({
      mimeType: "audio/opus",
      clockRate: 48000,
      channels: 2,
    }),
  ],
};

// 3. Shared outbound media tracks fed by OBS + dynamically resolved video codec
let activeVideoCodec = supportedCodecs.video[0]!;
const sharedVideoTrack = new MediaStreamTrack({
  kind: "video",
  codec: activeVideoCodec,
});
const sharedAudioTrack = new MediaStreamTrack({
  kind: "audio",
  codec: supportedCodecs.audio[0],
});
const peers = new Map<string, RTCPeerConnection>();
let uplinkCongested = false;

let activeObsPc: RTCPeerConnection | null = null;
let obsVideoReceiver: RTCRtpReceiver | null = null;
let obsVideoSsrc: number | undefined;
let lastPliSentAtMs = 0;

const requestObsKeyframe = (force = false) => {
  if (!obsVideoReceiver || obsVideoSsrc === undefined) return;
  const now = performance.now();
  if (!force && now - lastPliSentAtMs < PLI_COOLDOWN_MS) return;
  lastPliSentAtMs = now;
  obsVideoReceiver.sendRtcpPLI(obsVideoSsrc);
};

type PeerMetrics = {
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

const serverStartedAtMs = performance.now();
const metrics = {
  streamId,
  startedAt: new Date().toISOString(),
  uptimeSec: 0,
  obs: {
    connectionState: "closed",
    connectedCount: 0,
    lastConnectedAt: null as string | null,
    codec: activeVideoCodec.mimeType,
    video: {
      ssrc: null as number | null,
      kbps: 0,
      peakKbps: 0,
      avgKbps: 0,
      pps: 0,
      packetsReceived: 0,
      bytesReceived: 0,
      packetsLost: 0,
      jitterMs: 0,
      nackCount: 0,
      pliCount: 0,
    },
    audio: {
      ssrc: null as number | null,
      kbps: 0,
      peakKbps: 0,
      avgKbps: 0,
      pps: 0,
      packetsReceived: 0,
      bytesReceived: 0,
      packetsLost: 0,
      jitterMs: 0,
    },
  },
  peers: {
    activeCount: 0,
    totalAccepted: 0,
    totalRejected: 0,
    totalFailedOrDisconnected: 0,
    sessions: {} as Record<string, PeerMetrics>,
  },
};

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

const scheduleStats = () => {
  setTimeout(
    async () => {
      metrics.uptimeSec = Math.round(
        (performance.now() - serverStartedAtMs) / 1000,
      );
      metrics.peers.activeCount = peers.size;

      if (activeObsPc && activeObsPc.connectionState !== "closed") {
        metrics.obs.connectionState = activeObsPc.connectionState;
        metrics.obs.codec = activeVideoCodec.mimeType;
        const report = await activeObsPc.getStats();
        for (const s of report.values()) {
          if (s.type === "inbound-rtp") {
            const r = s as RTCInboundRtpStreamStats;
            const bytes = r.bytesReceived ?? 0;
            const packets = r.packetsReceived ?? 0;
            const { kbps, pps, avgKbps } = computeRates(
              `obs:${r.id}`,
              r.timestamp,
              bytes,
              packets,
            );
            const target =
              r.kind === "video" ? metrics.obs.video : metrics.obs.audio;
            target.ssrc = r.ssrc;
            target.kbps = kbps;
            target.peakKbps = Math.max(target.peakKbps, kbps);
            target.avgKbps = avgKbps;
            target.pps = pps;
            target.packetsReceived = packets;
            target.bytesReceived = bytes;
            target.packetsLost = r.packetsLost ?? 0;
            target.jitterMs = Math.round((r.jitter ?? 0) * 1000);
            if (r.kind === "video") {
              metrics.obs.video.nackCount = r.nackCount ?? 0;
              metrics.obs.video.pliCount = r.pliCount ?? 0;
            }
          }
        }
      }

      for (const [viewerId, pc] of peers) {
        if (pc.connectionState === "closed") continue;
        const pm = metrics.peers.sessions[viewerId];
        if (!pm) continue;
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
      }

      let connectedPeers = 0;
      let congestedPeers = 0;
      for (const [viewerId, pc] of peers) {
        if (pc.connectionState !== "connected") continue;
        const pm = metrics.peers.sessions[viewerId];
        if (!pm) continue;
        connectedPeers++;
        const rtt = pm.transport.rttMs ?? pm.video.remoteRttMs;
        const avgRtt = pm.transport.avgRttMs ?? pm.video.remoteAvgRttMs;
        const rttSpiked =
          rtt != null &&
          avgRtt != null &&
          rtt >= avgRtt * 1.5 &&
          rtt - avgRtt >= 30;
        if (pm.video.remoteFractionLost >= 0.05 || rttSpiked) {
          congestedPeers++;
        }
      }
      const nextCongested =
        connectedPeers >= 2 && congestedPeers >= Math.ceil(connectedPeers / 2);
      if (nextCongested !== uplinkCongested) {
        uplinkCongested = nextCongested;
        console.warn(
          uplinkCongested
            ? `[Uplink] Saturated (${congestedPeers}/${connectedPeers} peers congested) — pausing new peer joins`
            : `[Uplink] Recovered — accepting new peers`,
        );
      }

      if (activeObsPc || peers.size > 0) {
        const o = metrics.obs;
        const lines = [
          `[Metrics up=${metrics.uptimeSec}s peers=${metrics.peers.activeCount}${uplinkCongested ? " UPLINK_CONGESTED" : ""} (accepted=${metrics.peers.totalAccepted} rejected=${metrics.peers.totalRejected} dropped=${metrics.peers.totalFailedOrDisconnected})]`,
          `  OBS [${o.connectionState}] ${o.codec} (conns=${o.connectedCount})`,
          `    video ssrc=${o.video.ssrc ?? "-"} rate=${o.video.kbps}kbps (avg=${o.video.avgKbps} peak=${o.video.peakKbps}) ${o.video.pps}pps | rx=${o.video.packetsReceived}pkts (${(o.video.bytesReceived / 1048576).toFixed(2)}MB) lost=${o.video.packetsLost} jitter=${o.video.jitterMs}ms nack=${o.video.nackCount} pli=${o.video.pliCount}`,
          `    audio ssrc=${o.audio.ssrc ?? "-"} rate=${o.audio.kbps}kbps (avg=${o.audio.avgKbps} peak=${o.audio.peakKbps}) ${o.audio.pps}pps | rx=${o.audio.packetsReceived}pkts (${(o.audio.bytesReceived / 1048576).toFixed(2)}MB) lost=${o.audio.packetsLost} jitter=${o.audio.jitterMs}ms`,
        ];
        for (const pm of Object.values(metrics.peers.sessions)) {
          if (!peers.has(pm.viewerId)) continue;
          const rtt = pm.transport.rttMs ?? pm.video.remoteRttMs;
          const avgRtt = pm.transport.avgRttMs ?? pm.video.remoteAvgRttMs;
          const ping =
            pm.transport.oneWayPingMs ??
            (rtt != null ? Math.round(rtt / 2) : null);
          lines.push(
            `  Peer ${pm.viewerId.slice(0, 6)} [${pm.connectionState}/${pm.iceConnectionState}] rtt=${rtt ?? "-"}ms (avg=${avgRtt ?? "-"}ms ping=${ping ?? "-"}ms) rtcpRtt=${pm.video.remoteRttMs ?? "-"}ms skew=${pm.wallClockSkewMs}ms gather=${pm.iceGatherDurationMs}ms ${pm.transport.protocol ?? "-"} ${pm.transport.localCandidate ?? "-"} -> ${pm.transport.remoteCandidate ?? "-"}`,
            `    video ssrc=${pm.video.ssrc ?? "-"} rate=${pm.video.kbps}kbps (avg=${pm.video.avgKbps} peak=${pm.video.peakKbps}) ${pm.video.pps}pps | tx=${pm.video.packetsSent}pkts (${(pm.video.bytesSent / 1048576).toFixed(2)}MB) rtx=${pm.video.retransmittedPacketsSent} remLost=${pm.video.remotePacketsLost} (${(pm.video.remoteFractionLost * 100).toFixed(1)}%) nack=${pm.video.nackCount} pli=${pm.video.pliCount} fir=${pm.video.firCount}`,
            `    audio ssrc=${pm.audio.ssrc ?? "-"} rate=${pm.audio.kbps}kbps (avg=${pm.audio.avgKbps} peak=${pm.audio.peakKbps}) ${pm.audio.pps}pps | tx=${pm.audio.packetsSent}pkts (${(pm.audio.bytesSent / 1048576).toFixed(2)}MB) remLost=${pm.audio.remotePacketsLost}`,
          );
        }
        console.info(lines.join("\n"));
      }

      scheduleStats();
    },
    METRICS_INTERVAL_MS - (Date.now() % METRICS_INTERVAL_MS),
  );
};
scheduleStats();

// 4. Start local WHIP server for OBS (http://127.0.0.1:8080/whip)
Bun.serve({
  port: 8080,
  hostname: "127.0.0.1",
  async fetch(req) {
    const url = new URL(req.url);

    if (req.method === "POST" && url.pathname === "/whip") {
      if (activeObsPc) {
        await activeObsPc.close();
      }
      const obsPc = new RTCPeerConnection({ codecs: supportedCodecs });
      activeObsPc = obsPc;
      metrics.obs.connectedCount++;
      metrics.obs.lastConnectedAt = new Date().toISOString();
      metrics.obs.connectionState = obsPc.connectionState;

      obsPc.connectionStateChange.subscribe((state) => {
        metrics.obs.connectionState = state;
        console.info(`[OBS] Connection state: ${state}`);
        if (
          (state === "closed" ||
            state === "failed" ||
            state === "disconnected") &&
          activeObsPc === obsPc
        ) {
          activeObsPc = null;
        }
      });

      obsPc.onTrack.subscribe((track) => {
        if (track.kind === "video") {
          const videoTransceiver = obsPc
            .getTransceivers()
            .find((t) => t.kind === "video");

          obsVideoReceiver = videoTransceiver?.receiver ?? null;

          track.onReceiveRtp.subscribe((rtp) => {
            if (obsVideoSsrc !== rtp.header.ssrc) {
              obsVideoSsrc = rtp.header.ssrc;
              const matched = videoTransceiver?.codecs.find(
                (c) => c.payloadType === rtp.header.payloadType,
              );
              if (matched) {
                activeVideoCodec = matched;
                sharedVideoTrack.codec = matched;
                console.info(
                  `[OBS] Dynamically resolved video codec: ${matched.mimeType} (PT=${matched.payloadType})`,
                );
              }
            }
            sharedVideoTrack.writeRtp(rtp);
          });
        }

        if (track.kind === "audio") {
          track.onReceiveRtp.subscribe((rtp) => {
            sharedAudioTrack.writeRtp(rtp);
          });
        }
      });

      await obsPc.setRemoteDescription({
        type: "offer",
        sdp: await req.text(),
      });
      await obsPc.setLocalDescription(await obsPc.createAnswer());

      const videoTransceiver = obsPc
        .getTransceivers()
        .find((t) => t.kind === "video");

      if (videoTransceiver?.codecs[0]) {
        activeVideoCodec = videoTransceiver.codecs[0];
        sharedVideoTrack.codec = activeVideoCodec;
        console.info(
          `[OBS] Connected via WHIP (${activeVideoCodec.mimeType})! Streaming live...`,
        );
      }

      return new Response(obsPc.localDescription!.sdp, {
        status: 201,
        headers: {
          "Content-Type": "application/sdp",
          Location: "/whip/session",
        },
      });
    }

    return new Response("Not Found", { status: 404 });
  },
});

console.info("OBS WHIP Server: http://127.0.0.1:8080/whip");
console.info(`Stream Public ID (HMAC): ${streamId}`);
console.info(
  `\nShare this 1-click private link with peers:\nhttp://localhost:3000/#${token}\n`,
);

// 5. Listen for encrypted peer Offers over public MQTT
const client = mqtt.connect("wss://broker.emqx.io:8084/mqtt");
const offerTopic = `stream/${streamId}/offer`;

client.on("connect", () => {
  client.subscribe(offerTopic, (err) => {
    if (err) console.error("[MQTT] Subscribe error:", err);
  });
  console.info("[MQTT] Connected and listening for peers...");
});

client.on("error", (err) => console.error("[MQTT] Error:", err));

client.on("message", async (_topic, rawPayload) => {
  try {
    const { viewerId, iv, data } = JSON.parse(rawPayload.toString());
    if (!viewerId) return;
    if (peers.has(viewerId) || uplinkCongested) {
      metrics.peers.totalRejected++;
      console.warn(
        `[Peer ${viewerId.slice(0, 6)}] Rejected (${peers.has(viewerId) ? "duplicate" : "host uplink congested"})`,
      );
      return;
    }

    const decrypted = await crypto.subtle.decrypt(
      { name: "AES-GCM", iv: Buffer.from(iv, "base64url") },
      aesKey,
      Buffer.from(data, "base64url"),
    );
    const { ts, sdp: offerSdp } = JSON.parse(
      new TextDecoder().decode(decrypted),
    );
    const skewMs = Date.now() - ts;
    console.info(
      `[Peer ${viewerId.slice(0, 6)}] Offer authenticated (wall-clock skew: ${skewMs}ms)`,
    );
    if (Math.abs(skewMs) > 30_000) {
      console.warn(
        `[Peer ${viewerId.slice(0, 6)}] Large wall-clock skew (${skewMs}ms) — rely on RTP packetsSent/packetsReceived & SSRC to correlate logs`,
      );
    }

    const pc = new RTCPeerConnection({
      codecs: {
        video: [activeVideoCodec],
        audio: supportedCodecs.audio,
      },
      iceServers: [{ urls: "stun:stun.l.google.com:19302" }],
    });
    peers.set(viewerId, pc);
    metrics.peers.totalAccepted++;
    metrics.peers.activeCount = peers.size;
    const peerMetrics: PeerMetrics = {
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
    metrics.peers.sessions[viewerId] = peerMetrics;

    const videoSender = pc.addTrack(sharedVideoTrack);
    pc.addTrack(sharedAudioTrack);

    videoSender.onKeyFrameRequest.subscribe(() => {
      requestObsKeyframe();
    });

    pc.onIceCandidate.subscribe((c) => {
      if (c)
        console.debug(
          `[Peer ${viewerId.slice(0, 6)}] ICE candidate:`,
          c.candidate,
        );
    });

    pc.connectionStateChange.subscribe((state) => {
      peerMetrics.connectionState = state;
      peerMetrics.iceConnectionState = pc.iceConnectionState;
      console.info(
        `[Peer ${viewerId.slice(0, 6)}] ${state} (peers=${peers.size})`,
      );
      if (state === "connected") {
        requestObsKeyframe(true);
        setTimeout(() => requestObsKeyframe(true), 500);
      }
      if (
        state === "closed" ||
        state === "failed" ||
        state === "disconnected"
      ) {
        if (peers.delete(viewerId)) {
          peerMetrics.disconnectedAt ??= new Date().toISOString();
          metrics.peers.totalFailedOrDisconnected++;
          metrics.peers.activeCount = peers.size;
          pc.close();
        }
      }
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
    peerMetrics.iceGatherDurationMs = Math.round(
      performance.now() - iceGatherStartMs,
    );

    const ansIv = crypto.getRandomValues(new Uint8Array(12));
    const ansCipher = await crypto.subtle.encrypt(
      { name: "AES-GCM", iv: ansIv },
      aesKey,
      new TextEncoder().encode(
        JSON.stringify({ sdp: pc.localDescription!.sdp }),
      ),
    );

    client.publish(
      `stream/${streamId}/answer/${viewerId}`,
      JSON.stringify({
        iv: Buffer.from(ansIv).toString("base64url"),
        data: Buffer.from(ansCipher).toString("base64url"),
      }),
    );
  } catch (err) {
    console.warn("[MQTT] Ignored invalid or unauthorized offer:", err);
  }
});
