import dgram from "dgram";
import { parseArgs } from "util";
import {
  RTCPeerConnection,
  RTCRtpCodecParameters,
  type RTCRtpReceiver,
  type RTCInboundRtpStreamStats,
} from "werift";
import mqtt from "mqtt";
import type { CodecConfig, PeerMetrics } from "./peer-worker.ts";

const METRICS_INTERVAL_MS = 5000;
const UDP_BUFFER_BYTES = 16 * 1024 * 1024;
const WORKER_COUNT = Math.max(
  1,
  Math.min(4, navigator.hardwareConcurrency || 2),
);
const VALID_PROFILES = ["low-latency", "stable"] as const;
type StreamProfile = (typeof VALID_PROFILES)[number];

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

// 1. Parse optional --token and --profile flags, then derive public streamId + AES-GCM key
const { values } = parseArgs({
  args: Bun.argv.slice(2),
  options: {
    token: { type: "string" },
    profile: { type: "string" },
  },
});

const rawProfile = values.profile ?? "stable";
if (!VALID_PROFILES.includes(rawProfile as StreamProfile)) {
  console.error(
    `Invalid --profile "${rawProfile}". Valid profiles: ${VALID_PROFILES.join(", ")}`,
  );
  process.exit(1);
}
const profile = rawProfile as StreamProfile;
const isStable = profile === "stable";
const PLI_COOLDOWN_MS = isStable ? 1000 : 500;

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

const serializeCodec = (c: RTCRtpCodecParameters): CodecConfig => ({
  mimeType: c.mimeType,
  clockRate: c.clockRate,
  rtcpFeedback: c.rtcpFeedback,
  parameters: c.parameters,
});

// 3. Peer worker pool for multi-core SRTP fanout + OBS state
let activeVideoCodec = supportedCodecs.video[0]!;
let activeObsPc: RTCPeerConnection | null = null;
let obsVideoReceiver: RTCRtpReceiver | null = null;
let obsVideoSsrc: number | undefined;
let lastPliSentAtMs = 0;
let uplinkCongested = false;

const requestObsKeyframe = (force = false) => {
  if (!obsVideoReceiver || obsVideoSsrc === undefined) return;
  const now = performance.now();
  if (!force && now - lastPliSentAtMs < PLI_COOLDOWN_MS) return;
  lastPliSentAtMs = now;
  obsVideoReceiver.sendRtcpPLI(obsVideoSsrc);
};

type WorkerSlot = {
  worker: Worker;
  peers: Set<string>;
};

const peerToWorker = new Map<string, WorkerSlot>();
const pendingStats = new Map<number, (sessions: PeerMetrics[]) => void>();
let statsReqSeq = 0;

const workerSlots: WorkerSlot[] = Array.from({ length: WORKER_COUNT }, () => {
  const worker = new Worker(new URL("./peer-worker.ts", import.meta.url).href);
  const slot: WorkerSlot = { worker, peers: new Set() };
  worker.postMessage({
    type: "init",
    profile,
    videoCodec: serializeCodec(activeVideoCodec),
  });
  worker.onmessage = async (event: MessageEvent) => {
    const msg = event.data;
    if (msg.type === "pli") {
      requestObsKeyframe(msg.force);
    } else if (msg.type === "state") {
      const { viewerId, state, closed } = msg as {
        viewerId: string;
        state: string;
        closed: boolean;
      };
      if (closed) {
        slot.peers.delete(viewerId);
        peerToWorker.delete(viewerId);
        metrics.peers.totalFailedOrDisconnected++;
        metrics.peers.activeCount = peerToWorker.size;
      }
      console.info(
        `[Peer ${viewerId.slice(0, 6)}] ${state} (peers=${peerToWorker.size})`,
      );
    } else if (msg.type === "answer") {
      const { viewerId, answerSdp } = msg as {
        viewerId: string;
        answerSdp: string;
      };
      const ansIv = crypto.getRandomValues(new Uint8Array(12));
      const ansCipher = await crypto.subtle.encrypt(
        { name: "AES-GCM", iv: ansIv },
        aesKey,
        new TextEncoder().encode(JSON.stringify({ sdp: answerSdp, profile })),
      );
      client.publish(
        `stream/${streamId}/answer/${viewerId}`,
        JSON.stringify({
          iv: Buffer.from(ansIv).toString("base64url"),
          data: Buffer.from(ansCipher).toString("base64url"),
        }),
      );
    } else if (msg.type === "offerError") {
      const { viewerId, error } = msg as { viewerId: string; error: string };
      slot.peers.delete(viewerId);
      peerToWorker.delete(viewerId);
      metrics.peers.activeCount = peerToWorker.size;
      console.warn(
        `[Peer ${viewerId.slice(0, 6)}] Offer handling failed:`,
        error,
      );
    } else if (msg.type === "stats") {
      const resolve = pendingStats.get(msg.reqId);
      if (resolve) {
        pendingStats.delete(msg.reqId);
        resolve(msg.sessions);
      }
    }
  };
  return slot;
});

const broadcastCodecToWorkers = (codec: RTCRtpCodecParameters) => {
  const serialized = serializeCodec(codec);
  for (const slot of workerSlots) {
    slot.worker.postMessage({ type: "codec", videoCodec: serialized });
  }
};

const requestWorkerStats = (slot: WorkerSlot) => {
  if (slot.peers.size === 0) return Promise.resolve<PeerMetrics[]>([]);
  const reqId = ++statsReqSeq;
  return new Promise<PeerMetrics[]>((resolve) => {
    pendingStats.set(reqId, resolve);
    slot.worker.postMessage({ type: "getStats", reqId });
  });
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
      metrics.peers.activeCount = peerToWorker.size;

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

      const activeSessions = (
        await Promise.all(workerSlots.map(requestWorkerStats))
      ).flat();

      let connectedPeers = 0;
      let congestedPeers = 0;
      for (const pm of activeSessions) {
        metrics.peers.sessions[pm.viewerId] = pm;
        if (pm.connectionState !== "connected") continue;
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

      if (activeObsPc || peerToWorker.size > 0) {
        const o = metrics.obs;
        const lines = [
          `[Metrics up=${metrics.uptimeSec}s profile=${profile} workers=${WORKER_COUNT} peers=${metrics.peers.activeCount}${uplinkCongested ? " UPLINK_CONGESTED" : ""} (accepted=${metrics.peers.totalAccepted} rejected=${metrics.peers.totalRejected} dropped=${metrics.peers.totalFailedOrDisconnected})]`,
          `  OBS [${o.connectionState}] ${o.codec} (conns=${o.connectedCount})`,
          `    video ssrc=${o.video.ssrc ?? "-"} rate=${o.video.kbps}kbps (avg=${o.video.avgKbps} peak=${o.video.peakKbps}) ${o.video.pps}pps | rx=${o.video.packetsReceived}pkts (${(o.video.bytesReceived / 1048576).toFixed(2)}MB) lost=${o.video.packetsLost} jitter=${o.video.jitterMs}ms nack=${o.video.nackCount} pli=${o.video.pliCount}`,
          `    audio ssrc=${o.audio.ssrc ?? "-"} rate=${o.audio.kbps}kbps (avg=${o.audio.avgKbps} peak=${o.audio.peakKbps}) ${o.audio.pps}pps | rx=${o.audio.packetsReceived}pkts (${(o.audio.bytesReceived / 1048576).toFixed(2)}MB) lost=${o.audio.packetsLost} jitter=${o.audio.jitterMs}ms`,
        ];
        for (const pm of activeSessions) {
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
                broadcastCodecToWorkers(matched);
                console.info(
                  `[OBS] Dynamically resolved video codec: ${matched.mimeType} (PT=${matched.payloadType})`,
                );
              }
            }
            if (peerToWorker.size === 0) return;
            const data = rtp.serialize();
            for (const slot of workerSlots) {
              if (slot.peers.size > 0) {
                slot.worker.postMessage({ type: "rtp", kind: "video", data });
              }
            }
          });
        }

        if (track.kind === "audio") {
          track.onReceiveRtp.subscribe((rtp) => {
            if (peerToWorker.size === 0) return;
            const data = rtp.serialize();
            for (const slot of workerSlots) {
              if (slot.peers.size > 0) {
                slot.worker.postMessage({ type: "rtp", kind: "audio", data });
              }
            }
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
        broadcastCodecToWorkers(activeVideoCodec);
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
console.info(`Profile: ${profile} (workers=${WORKER_COUNT})`);
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
    if (peerToWorker.has(viewerId) || uplinkCongested) {
      metrics.peers.totalRejected++;
      console.warn(
        `[Peer ${viewerId.slice(0, 6)}] Rejected (${peerToWorker.has(viewerId) ? "duplicate" : "host uplink congested"})`,
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

    const slot = workerSlots.reduce((min, cur) =>
      cur.peers.size < min.peers.size ? cur : min,
    );
    slot.peers.add(viewerId);
    peerToWorker.set(viewerId, slot);
    metrics.peers.totalAccepted++;
    metrics.peers.activeCount = peerToWorker.size;

    slot.worker.postMessage({
      type: "offer",
      viewerId,
      offerSdp,
      skewMs,
      videoCodec: serializeCodec(activeVideoCodec),
    });
  } catch (err) {
    console.warn("[MQTT] Ignored invalid or unauthorized offer:", err);
  }
});
