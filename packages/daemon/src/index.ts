import dgram from "dgram";
import fs from "fs";
import os from "os";
import path from "path";
import { parseArgs } from "util";
import {
  RTCPeerConnection,
  RTCRtpCodecParameters,
  type RTCRtpReceiver,
  type RTCInboundRtpStreamStats,
} from "werift";
import mqtt from "mqtt";
import type { CodecConfig, PeerMetrics } from "./worker.ts";

if (typeof Bun === "undefined") {
  console.error(
    "tiwi requires Bun (https://bun.sh). Run with: bunx @qualterz/tiwi",
  );
  process.exit(1);
}

declare const __TIWI_VERSION__: string | undefined;
declare const __TIWI_COMMIT__: string | undefined;
declare const __TIWI_BUILD__: "dev" | "npm" | "exe" | "msi" | undefined;

const VERSION =
  typeof __TIWI_VERSION__ !== "undefined" ? __TIWI_VERSION__ : "0.0.0-dev";
const COMMIT = typeof __TIWI_COMMIT__ !== "undefined" ? __TIWI_COMMIT__ : "dev";
const BUILD_CHANNEL =
  typeof __TIWI_BUILD__ !== "undefined" ? __TIWI_BUILD__ : "dev";
const REPO = "qualterz/tiwi";

const DEFAULT_PORT = 2000;
const METRICS_INTERVAL_MS = 5000;
const UDP_BUFFER_BYTES = 16 * 1024 * 1024;
const WORKER_COUNT = Math.max(
  1,
  Math.min(4, navigator.hardwareConcurrency || 2),
);

// Clean up leftover binary from a previous portable self-update on Windows
if (BUILD_CHANNEL === "exe" && process.platform === "win32") {
  try {
    fs.rmSync(`${process.execPath}.old`, { force: true });
  } catch {}
}

// Enlarge OS UDP socket buffers (default 64KB on Windows overflows during 30-55Mbps keyframe bursts)
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

// 1. Parse CLI flags and handle subcommands/meta flags
const { values, positionals } = parseArgs({
  args: Bun.argv.slice(2),
  options: {
    "help": { type: "boolean", short: "h" },
    "version": { type: "boolean", short: "v" },
    "config": { type: "string" },
    "token": { type: "string" },
    "rtx": { type: "boolean" },
    "port": { type: "string" },
    "max-peers": { type: "string" },
    "max-uplink": { type: "string" },
    "verbose": { type: "boolean" },
    "update-check": { type: "boolean" },
  },
  allowNegative: true,
  allowPositionals: true,
});

if (values.help) {
  console.log(`Usage: tiwi [command] [options]

Commands:
  update              Update tiwi to the latest release

Options:
  --config <path>     Path to config.json
  --token <token>     Override stream token for this run
  --port <port>       Local WHIP/WS server port (default: ${DEFAULT_PORT})
  --rtx / --no-rtx    Enable or disable RTP retransmission (default: on)
  --max-peers <n>     Max concurrent viewers (default: 0, unlimited)
  --max-uplink <mbps> Max total upload bitrate in Mbps (default: 0, unlimited)
  --no-update-check   Skip checking for updates on startup
  --verbose           Log full output instead of status line
  -v, --version       Show version
  -h, --help          Show this help message`);
  process.exit(0);
}

if (values.version) {
  console.log(`tiwi ${VERSION} (${COMMIT})`);
  process.exit(0);
}

const isNewerVersion = (latest: string, current: string) => {
  const a = latest.split(".").map(Number);
  const b = current.split(".").map(Number);
  for (let i = 0; i < Math.max(a.length, b.length); i++) {
    const diff = (a[i] ?? 0) - (b[i] ?? 0);
    if (diff !== 0) return diff > 0;
  }
  return false;
};

const fetchLatestRelease = async (timeoutMs = 3000) => {
  const res = await fetch(`https://github.com/${REPO}/releases/latest`, {
    redirect: "manual",
    signal: AbortSignal.timeout(timeoutMs),
  });
  const loc = res.headers.get("location");
  const tag = loc?.match(/\/releases\/tag\/([^/?#]+)$/)?.[1];
  if (!tag) return null;
  return { tag, version: tag.replace(/^v/, "") };
};

const verifySha256 = (data: Uint8Array, sumsText: string, filename: string) => {
  const line = sumsText
    .split(/\r?\n/)
    .find(
      (l) =>
        l.trim().endsWith(` ${filename}`) || l.trim().endsWith(`*${filename}`),
    );
  if (!line) throw new Error(`Checksum entry not found for ${filename}`);
  const expected = line.trim().split(/\s+/)[0]!.toLowerCase();
  const actual = new Bun.CryptoHasher("sha256")
    .update(data)
    .digest("hex")
    .toLowerCase();
  if (actual !== expected) {
    throw new Error(
      `Checksum mismatch for ${filename} (expected ${expected}, got ${actual})`,
    );
  }
};

if (positionals[0] === "update") {
  if (BUILD_CHANNEL === "dev") {
    console.log("Running from source (dev) — update via git pull.");
    process.exit(0);
  }

  let latest: { tag: string; version: string } | null = null;
  try {
    latest = await fetchLatestRelease(10_000);
  } catch (err) {
    console.error(
      `Failed to check for updates: ${err instanceof Error ? err.message : String(err)}`,
    );
    process.exit(1);
  }

  if (!latest) {
    console.error("No published releases found.");
    process.exit(1);
  }

  if (!isNewerVersion(latest.version, VERSION)) {
    console.log(`tiwi is already up to date (${VERSION}).`);
    process.exit(0);
  }

  console.log(`Updating tiwi ${VERSION} -> ${latest.version}...`);

  if (BUILD_CHANNEL === "npm") {
    const proc = Bun.spawnSync(
      [process.execPath, "add", "-g", "@qualterz/tiwi@latest"],
      {
        stdio: ["inherit", "inherit", "inherit"],
      },
    );
    process.exit(proc.exitCode);
  }

  if (process.platform !== "win32") {
    console.error(
      `Self-update is only supported for Windows binaries or Bun/npm installs.`,
    );
    process.exit(1);
  }

  const baseUrl = `https://github.com/${REPO}/releases/download/${latest.tag}`;
  const sumsRes = await fetch(`${baseUrl}/SHA256SUMS.txt`, {
    signal: AbortSignal.timeout(15_000),
  });
  if (!sumsRes.ok) {
    console.error(`Failed to download SHA256SUMS.txt (${sumsRes.status})`);
    process.exit(1);
  }
  const sumsText = await sumsRes.text();

  const isPortable = BUILD_CHANNEL === "exe";
  const asset = isPortable
    ? "tiwi-windows-x64-portable.zip"
    : "tiwi-windows-x64.msi";
  const assetRes = await fetch(`${baseUrl}/${asset}`, {
    signal: AbortSignal.timeout(120_000),
  });
  if (!assetRes.ok) {
    console.error(`Failed to download ${asset} (${assetRes.status})`);
    process.exit(1);
  }
  const assetBytes = new Uint8Array(await assetRes.arrayBuffer());
  verifySha256(assetBytes, sumsText, asset);

  if (isPortable) {
    const tmpDir = fs.mkdtempSync(path.join(os.tmpdir(), "tiwi-update-"));
    try {
      const zipPath = path.join(tmpDir, asset);
      fs.writeFileSync(zipPath, assetBytes);
      const extract = Bun.spawnSync(
        ["tar", "-xf", zipPath, "-C", tmpDir, "tiwi.exe"],
        { stdio: ["ignore", "pipe", "pipe"] },
      );
      if (extract.exitCode !== 0) {
        throw new Error(
          `Failed to extract tiwi.exe: ${extract.stderr.toString().trim()}`,
        );
      }
      const newExe = path.join(tmpDir, "tiwi.exe");
      const oldExe = `${process.execPath}.old`;
      fs.rmSync(oldExe, { force: true });
      fs.renameSync(process.execPath, oldExe);
      try {
        fs.copyFileSync(newExe, process.execPath);
      } catch (err) {
        fs.renameSync(oldExe, process.execPath);
        throw err;
      }
    } finally {
      fs.rmSync(tmpDir, { recursive: true, force: true });
    }
    console.log(
      `Updated tiwi to ${latest.version}. Restart tiwi to use the new version.`,
    );
    process.exit(0);
  } else {
    const msiPath = path.join(
      os.tmpdir(),
      `tiwi-${latest.version}-windows-x64.msi`,
    );
    fs.writeFileSync(msiPath, assetBytes);
    const child = Bun.spawn(["msiexec", "/i", msiPath, "/passive"], {
      stdio: ["ignore", "ignore", "ignore"],
    });
    child.unref();
    console.log(`Launching installer for tiwi ${latest.version}...`);
    process.exit(0);
  }
}

const appDataDir =
  process.platform === "win32"
    ? path.join(
        process.env.APPDATA || path.join(os.homedir(), "AppData", "Roaming"),
        "tiwi",
      )
    : path.join(
        process.env.XDG_CONFIG_HOME || path.join(os.homedir(), ".config"),
        "tiwi",
      );

const configPath = values.config
  ? path.resolve(values.config)
  : BUILD_CHANNEL === "exe"
    ? path.join(path.dirname(process.execPath), "config.json")
    : path.join(appDataDir, "config.json");

type DaemonConfig = {
  token?: string;
  port?: number;
  rtx?: boolean;
  maxPeers?: number;
  maxUplinkMbps?: number;
};

let fileConfig: DaemonConfig = {};
if (fs.existsSync(configPath)) {
  const raw = fs.readFileSync(configPath, "utf8").trim();
  if (raw) {
    fileConfig = JSON.parse(raw) as DaemonConfig;
  }
}

if (!fileConfig.token) {
  fileConfig = {
    token: Buffer.from(crypto.getRandomValues(new Uint8Array(32))).toString(
      "base64url",
    ),
    port: fileConfig.port ?? DEFAULT_PORT,
    rtx: fileConfig.rtx ?? true,
    maxPeers: fileConfig.maxPeers ?? 0,
    maxUplinkMbps: fileConfig.maxUplinkMbps ?? 0,
  };
  fs.mkdirSync(path.dirname(configPath), { recursive: true });
  fs.writeFileSync(configPath, JSON.stringify(fileConfig, null, 2) + "\n");
}

const port = Number(values.port ?? fileConfig.port) || DEFAULT_PORT;
const parseLimit = (raw: string | number | undefined) => {
  const n = Number(raw);
  return Number.isFinite(n) && n > 0 ? n : 0;
};
const maxPeers = Math.floor(
  parseLimit(values["max-peers"] ?? fileConfig.maxPeers),
);
const maxUplinkMbps = parseLimit(
  values["max-uplink"] ?? fileConfig.maxUplinkMbps,
);
const maxUplinkKbps = maxUplinkMbps * 1000;
const useTui = Boolean(process.stdout.isTTY && !values.verbose);
if (useTui) {
  console.log =
    console.info =
    console.warn =
    console.debug =
    console.error =
      () => {};
  process.emitWarning = () => {};
}
const rtx = values.rtx ?? fileConfig.rtx ?? true;
const PLI_COOLDOWN_MS = rtx ? 1000 : 500;

const token = values.token || fileConfig.token!;
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
const UPLINK_RECOVERY_TICKS = 3;
let uplinkCongested = false;
let uplinkCleanTicks = 0;

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
  const worker = new Worker(new URL("./worker.ts", import.meta.url).href);
  const slot: WorkerSlot = { worker, peers: new Set() };
  worker.postMessage({
    type: "init",
    rtx,
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
        const isFailure =
          state.startsWith("failed") ||
          state.startsWith("timeout") ||
          state.endsWith("-failed");
        if (peerToWorker.delete(viewerId) && isFailure) {
          metrics.peers.totalFailedOrDisconnected++;
        }
        metrics.peers.activeCount = peerToWorker.size;
      }
      console.info(
        `[Peer ${viewerId.slice(0, 6)}] ${state} (${peerToWorker.size} active)`,
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
        new TextEncoder().encode(JSON.stringify({ sdp: answerSdp })),
      );
      if (!client?.connected || client.disconnecting) return;
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
      console.warn(`[Peer ${viewerId.slice(0, 6)}] offer failed: ${error}`);
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

const publishEvent = async (type: "start" | "stop") => {
  if (!client?.connected || client.disconnecting) return;
  const iv = crypto.getRandomValues(new Uint8Array(12));
  const cipher = await crypto.subtle.encrypt(
    { name: "AES-GCM", iv },
    aesKey,
    new TextEncoder().encode(JSON.stringify({ type })),
  );
  if (!client.connected || client.disconnecting) return;
  client.publish(
    eventsTopic,
    JSON.stringify({
      iv: Buffer.from(iv).toString("base64url"),
      data: Buffer.from(cipher).toString("base64url"),
    }),
  );
  console.info(`[MQTT] published event ${type}`);
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

const wsClients = new Set<Bun.ServerWebSocket<undefined>>();
let lastActiveSessions: PeerMetrics[] = [];

const formatUptime = (sec: number) => {
  const h = Math.floor(sec / 3600);
  const m = Math.floor((sec % 3600) / 60);
  const s = sec % 60;
  return `${String(h).padStart(2, "0")}:${String(m).padStart(2, "0")}:${String(s).padStart(2, "0")}`;
};

const formatBitrate = (kbps: number) =>
  kbps >= 1000 ? `${(kbps / 1000).toFixed(1)} Mbps` : `${kbps} kbps`;

const percentile = (sorted: number[], p: number) => {
  if (sorted.length === 0) return null;
  const idx = Math.min(
    sorted.length - 1,
    Math.ceil((p / 100) * sorted.length) - 1,
  );
  return sorted[idx]!;
};

const isPeerRttSpiked = (pm: PeerMetrics) => {
  const rtt = pm.transport.rttMs ?? pm.video.remoteRttMs;
  const baseRtt =
    pm.transport.rttMs != null
      ? pm.transport.minRttMs
      : pm.video.remoteMinRttMs;
  return (
    rtt != null &&
    baseRtt != null &&
    rtt >= baseRtt * 1.5 &&
    rtt - baseRtt >= 30
  );
};

const paint = (text: string, color: "red" | "yellow" | null) =>
  color === "red"
    ? `\x1b[31m${text}\x1b[0m`
    : color === "yellow"
      ? `\x1b[33m${text}\x1b[0m`
      : text;

const renderStatusLine = () => {
  if (!useTui) return;
  let outSumKbps = 0;
  let anyRttSpiked = false;
  const rtts: number[] = [];
  const losses: number[] = [];
  for (const pm of lastActiveSessions) {
    outSumKbps += pm.video.kbps + pm.audio.kbps;
    if (pm.connectionState !== "connected") continue;
    const rtt = pm.transport.rttMs ?? pm.video.remoteRttMs;
    if (rtt != null) rtts.push(rtt);
    if (isPeerRttSpiked(pm)) anyRttSpiked = true;
    losses.push(pm.video.remoteFractionLost * 100);
  }
  rtts.sort((a, b) => a - b);
  losses.sort((a, b) => a - b);

  const rttP99Val = percentile(rtts, 99);
  const lossP99Val = percentile(losses, 99) ?? 0;
  const rttP99 = rttP99Val != null ? `${rttP99Val}ms` : "0ms";
  const lossP99 = `${lossP99Val.toFixed(1)}%`;
  const inRate = formatBitrate(metrics.obs.video.kbps + metrics.obs.audio.kbps);
  const outRate = formatBitrate(outSumKbps);

  const peersText = paint(
    `${metrics.peers.activeCount} peers`,
    maxPeers > 0 && metrics.peers.activeCount >= maxPeers ? "yellow" : null,
  );
  const outText = paint(
    `out ${outRate}`,
    uplinkCongested
      ? "red"
      : maxUplinkKbps > 0 && outSumKbps >= maxUplinkKbps * 0.85
        ? "yellow"
        : null,
  );
  const rttText = paint(
    `rtt ${rttP99}`,
    anyRttSpiked ? (uplinkCongested ? "red" : "yellow") : null,
  );
  const lossText = paint(
    `loss ${lossP99}`,
    lossP99Val >= 5 ? "red" : lossP99Val >= 1 ? "yellow" : null,
  );

  const line = `${peersText} · in ${inRate} · ${outText} · ${rttText} · ${lossText} · up ${formatUptime(metrics.uptimeSec)}`;
  process.stdout.write(`\r\x1b[2K${line}`);
};

if (useTui) {
  setInterval(() => {
    metrics.uptimeSec = Math.round(
      (performance.now() - serverStartedAtMs) / 1000,
    );
    renderStatusLine();
  }, 1000);
}

const broadcastWs = (payload: unknown) => {
  if (wsClients.size === 0) return;
  const text = JSON.stringify(payload);
  for (const ws of wsClients) {
    ws.send(text);
  }
};

const nextMetricsDelay = () => {
  const rem = METRICS_INTERVAL_MS - (Date.now() % METRICS_INTERVAL_MS);
  return rem < 1000 ? rem + METRICS_INTERVAL_MS : rem;
};

const scheduleStats = () => {
  setTimeout(async () => {
    try {
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
      } else {
        metrics.obs.video.kbps = 0;
        metrics.obs.video.pps = 0;
        metrics.obs.audio.kbps = 0;
        metrics.obs.audio.pps = 0;
      }

      const activeSessions = (
        await Promise.all(workerSlots.map(requestWorkerStats))
      ).flat();
      lastActiveSessions = activeSessions;

      let connectedPeers = 0;
      let congestedPeers = 0;
      for (const pm of activeSessions) {
        metrics.peers.sessions[pm.viewerId] = pm;
        if (pm.connectionState !== "connected") continue;
        connectedPeers++;
        if (pm.video.remoteFractionLost >= 0.05 || isPeerRttSpiked(pm)) {
          congestedPeers++;
        }
      }
      const isCongestedNow =
        connectedPeers >= 2 && congestedPeers > connectedPeers / 2;
      if (isCongestedNow) {
        uplinkCleanTicks = 0;
        if (!uplinkCongested) {
          uplinkCongested = true;
          console.warn(
            `[Uplink] saturated (${congestedPeers}/${connectedPeers} peers congested) — pausing new peer joins`,
          );
        }
      } else if (uplinkCongested) {
        if (connectedPeers < 2 || ++uplinkCleanTicks >= UPLINK_RECOVERY_TICKS) {
          uplinkCongested = false;
          uplinkCleanTicks = 0;
          console.warn(`[Uplink] recovered — accepting new peers`);
        }
      }

      if (useTui) {
        renderStatusLine();
      } else if (activeObsPc || peerToWorker.size > 0) {
        const o = metrics.obs;
        const lines = [
          `[Metrics] up=${metrics.uptimeSec}s rtx=${rtx ? "on" : "off"} workers=${WORKER_COUNT} peers=${metrics.peers.activeCount} (ok=${metrics.peers.totalAccepted} rej=${metrics.peers.totalRejected} drop=${metrics.peers.totalFailedOrDisconnected})${uplinkCongested ? " [CONGESTED]" : ""}`,
          `  WHIP [${o.connectionState}] ${o.codec} | vid ${o.video.kbps}kbps (${o.video.pps}pps, lost=${o.video.packetsLost}, jit=${o.video.jitterMs}ms, nack=${o.video.nackCount}, pli=${o.video.pliCount}) | aud ${o.audio.kbps}kbps (${o.audio.pps}pps, lost=${o.audio.packetsLost})`,
        ];
        for (const pm of activeSessions) {
          const rtt = pm.transport.rttMs ?? pm.video.remoteRttMs;
          const minRtt =
            pm.transport.rttMs != null
              ? pm.transport.minRttMs
              : pm.video.remoteMinRttMs;
          const avgRtt =
            pm.transport.rttMs != null
              ? pm.transport.avgRttMs
              : pm.video.remoteAvgRttMs;
          lines.push(
            `  Peer ${pm.viewerId.slice(0, 6)} [${pm.connectionState}/${pm.iceConnectionState}] rtt=${rtt ?? "-"}ms (min=${minRtt ?? "-"}ms avg=${avgRtt ?? "-"}ms) ${pm.transport.protocol ?? "-"} ${pm.transport.localCandidate ?? "-"} -> ${pm.transport.remoteCandidate ?? "-"}`,
            `    vid ${pm.video.kbps}kbps (${pm.video.pps}pps, lost=${pm.video.remotePacketsLost}/${(pm.video.remoteFractionLost * 100).toFixed(1)}%, rtx=${pm.video.retransmittedPacketsSent}, nack=${pm.video.nackCount}, pli=${pm.video.pliCount}) | aud ${pm.audio.kbps}kbps (${pm.audio.pps}pps, lost=${pm.audio.remotePacketsLost})`,
          );
        }
        console.info(lines.join("\n"));
      }

      broadcastWs({
        type: "metrics",
        rtx,
        uplinkCongested,
        metrics,
        activeSessions,
      });
    } catch (err) {
      console.error("[Metrics] stats loop error:", err);
    } finally {
      scheduleStats();
    }
  }, nextMetricsDelay());
};
scheduleStats();

// 4. Start local WHIP + WebSocket server (http://127.0.0.1:2000)
try {
  Bun.serve({
    port,
    hostname: "127.0.0.1",
    async fetch(req, server) {
      const url = new URL(req.url);

      if (url.pathname === "/ws") {
        if (server.upgrade(req)) return undefined;
        return new Response("WebSocket upgrade failed", { status: 400 });
      }

      if (req.method === "POST" && url.pathname === "/whip") {
        if (activeObsPc) {
          await activeObsPc.close();
        }
        const obsPc = new RTCPeerConnection({ codecs: supportedCodecs });
        activeObsPc = obsPc;
        metrics.obs.connectedCount++;
        metrics.obs.lastConnectedAt = new Date().toISOString();
        metrics.obs.connectionState = obsPc.connectionState;

        let obsWasConnected = false;
        obsPc.connectionStateChange.subscribe((state) => {
          metrics.obs.connectionState = state;
          console.info(`[WHIP] ${state}`);
          if (state === "connected" && activeObsPc === obsPc) {
            obsWasConnected = true;
            void publishEvent("start");
          }
          if (
            (state === "closed" ||
              state === "failed" ||
              state === "disconnected") &&
            activeObsPc === obsPc
          ) {
            activeObsPc = null;
            obsVideoReceiver = null;
            obsVideoSsrc = undefined;
            for (const slot of workerSlots) {
              slot.worker.postMessage({
                type: "closeAll",
                reason: `whip-${state}`,
                notify: true,
              });
            }
            if (obsWasConnected) {
              obsWasConnected = false;
              void publishEvent("stop");
            }
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
                  const codecChanged =
                    matched.mimeType !== activeVideoCodec.mimeType ||
                    matched.parameters !== activeVideoCodec.parameters;
                  activeVideoCodec = matched;
                  if (codecChanged) {
                    broadcastCodecToWorkers(matched);
                    void publishEvent("start");
                  }
                  console.info(
                    `[WHIP] codec ${matched.mimeType} (pt=${matched.payloadType})`,
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
        }

        return new Response(obsPc.localDescription!.sdp, {
          status: 201,
          headers: {
            "Content-Type": "application/sdp",
            "Location": "/whip/session",
          },
        });
      }

      if (req.method === "DELETE" && url.pathname === "/whip/session") {
        if (activeObsPc) {
          await activeObsPc.close();
        }
        return new Response(null, { status: 200 });
      }

      return new Response("Not Found", { status: 404 });
    },
    websocket: {
      open(ws) {
        wsClients.add(ws);
        ws.send(
          JSON.stringify({
            type: "state",
            rtx,
            maxPeers,
            maxUplinkMbps,
            token,
            streamId,
            uplinkCongested,
            metrics,
            activeSessions: lastActiveSessions,
          }),
        );
      },
      message() {},
      close(ws) {
        wsClients.delete(ws);
      },
    },
  });
} catch (err) {
  if ((err as { code?: string })?.code === "EADDRINUSE") {
    process.stderr.write(
      `Error: Port ${port} is already in use. Is another tiwi instance running? (Use --port <port> to pick another)\n`,
    );
    process.exit(1);
  }
  throw err;
}

const viewerBaseUrl =
  BUILD_CHANNEL === "dev"
    ? "http://127.0.0.1:3000"
    : "https://qualterz.github.io/tiwi";

if (useTui) {
  process.stdout.write(
    `\nhttp://127.0.0.1:${port}/\nhttp://127.0.0.1:${port}/whip\n${viewerBaseUrl}/#${token}\n\n`,
  );
  renderStatusLine();
} else {
  const limits = [
    maxPeers > 0 ? `maxPeers=${maxPeers}` : null,
    maxUplinkMbps > 0 ? `maxUplink=${maxUplinkMbps}Mbps` : null,
  ]
    .filter(Boolean)
    .join(", ");
  console.info(
    [
      `[Daemon] ready (version=${VERSION}, port=${port}, rtx=${rtx ? "on" : "off"}, workers=${WORKER_COUNT}${limits ? `, ${limits}` : ""})`,
      `  Config:    ${configPath}`,
      `  Dashboard: http://127.0.0.1:${port}/`,
      `  WHIP:      http://127.0.0.1:${port}/whip`,
      `  WebSocket: ws://127.0.0.1:${port}/ws`,
      `  Viewer:    ${viewerBaseUrl}/#${token}`,
      `  Stream ID: ${streamId}`,
    ].join("\n"),
  );
}

// Non-blocking startup update check
if (
  BUILD_CHANNEL !== "dev" &&
  values["update-check"] !== false &&
  process.env.TIWI_NO_UPDATE_CHECK !== "1"
) {
  fetchLatestRelease()
    .then((latest) => {
      if (!latest || !isNewerVersion(latest.version, VERSION)) return;
      if (useTui) {
        process.stdout.write(
          `\r\x1b[2KUpdate available: ${VERSION} -> ${latest.version} (run 'tiwi update')\n\n`,
        );
        renderStatusLine();
      } else {
        console.info(
          `[Update] ${VERSION} -> ${latest.version} available (run 'tiwi update')`,
        );
      }
    })
    .catch(() => {});
}

// 5. Listen for encrypted peer Offers over public MQTT
const client = mqtt.connect("wss://broker.emqx.io:8084/mqtt");
const offerTopic = `stream/${streamId}/offer`;
const eventsTopic = `stream/${streamId}/events`;

client.on("connect", () => {
  client.subscribe(offerTopic, (err) => {
    if (err) {
      console.error(`[MQTT] subscribe error: ${err}`);
      return;
    }
    console.info("[MQTT] connected");
    if (activeObsPc?.connectionState === "connected") {
      void publishEvent("start");
    }
  });
});

client.on("offline", () => console.warn("[MQTT] offline"));
client.on("error", (err) => console.error(`[MQTT] error: ${err}`));

client.on("message", async (_topic, rawPayload) => {
  try {
    const { viewerId, iv, data } = JSON.parse(rawPayload.toString());
    if (!viewerId) return;
    if (!activeObsPc || activeObsPc.connectionState !== "connected") {
      metrics.peers.totalRejected++;
      return;
    }
    const inKbps = metrics.obs.video.kbps + metrics.obs.audio.kbps;
    const rejectReason = peerToWorker.has(viewerId)
      ? "duplicate"
      : maxPeers > 0 && peerToWorker.size >= maxPeers
        ? `max peers (${maxPeers})`
        : maxUplinkKbps > 0 &&
            inKbps > 0 &&
            (peerToWorker.size + 1) * inKbps > maxUplinkKbps
          ? `max uplink (${maxUplinkMbps} Mbps)`
          : uplinkCongested
            ? "host uplink congested"
            : null;
    if (rejectReason) {
      metrics.peers.totalRejected++;
      console.warn(`[Peer ${viewerId.slice(0, 6)}] rejected (${rejectReason})`);
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
    if (Math.abs(skewMs) > 30_000) {
      console.warn(
        `[Peer ${viewerId.slice(0, 6)}] large wall-clock skew (${skewMs}ms)`,
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
    console.warn(`[MQTT] ignored invalid offer: ${err}`);
  }
});

let shuttingDown = false;
const shutdown = async (signal: string) => {
  if (shuttingDown) return;
  shuttingDown = true;
  console.info(
    `[Daemon] shutting down (${signal}, closing ${peerToWorker.size} active peers)`,
  );
  setTimeout(() => process.exit(0), 500).unref();
  const wasLive = activeObsPc?.connectionState === "connected";
  for (const slot of workerSlots) {
    slot.worker.postMessage({ type: "closeAll" });
  }
  if (activeObsPc) {
    const obsPc = activeObsPc;
    activeObsPc = null;
    obsVideoReceiver = null;
    obsVideoSsrc = undefined;
    await obsPc.close().catch(() => {});
  }
  if (wasLive) {
    await publishEvent("stop");
  }
  client.end(false, () => process.exit(0));
};
process.on("SIGINT", () => void shutdown("SIGINT"));
process.on("SIGTERM", () => void shutdown("SIGTERM"));
