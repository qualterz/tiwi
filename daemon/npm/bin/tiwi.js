#!/usr/bin/env node
import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import {
  chmodSync,
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  renameSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { homedir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const TARGETS = {
  "win32-x64": { suffix: "windows-x64.zip", bin: "tiwi.exe" },
  "linux-x64": { suffix: "linux-x64.tar.gz", bin: "tiwi" },
  "darwin-arm64": { suffix: "macos-arm64.tar.gz", bin: "tiwi" },
  "darwin-x64": { suffix: "macos-x64.tar.gz", bin: "tiwi" },
};

const key = `${process.platform}-${process.arch}`;
const target = TARGETS[key];
if (!target) {
  console.error(`Unsupported platform: ${key} (supported: ${Object.keys(TARGETS).join(", ")})`);
  process.exit(1);
}

const pkgPath = join(dirname(fileURLToPath(import.meta.url)), "..", "package.json");
const { version } = JSON.parse(readFileSync(pkgPath, "utf8"));
const asset = `tiwi-${version}-${target.suffix}`;

function cacheRoot() {
  if (process.platform === "win32") {
    return join(process.env.LOCALAPPDATA || join(homedir(), "AppData", "Local"), "tiwi", "bin");
  }
  if (process.platform === "darwin") {
    return join(homedir(), "Library", "Caches", "tiwi", "bin");
  }
  return join(process.env.XDG_CACHE_HOME || join(homedir(), ".cache"), "tiwi", "bin");
}

function findChecksum(sumsText, filename) {
  for (const line of sumsText.split(/\r?\n/)) {
    const match = line.match(/^\s*([0-9a-fA-F]{64})\s+\*?(.+?)\s*$/);
    if (match && match[2] === filename) {
      return match[1].toLowerCase();
    }
  }
  return null;
}

const binDir = join(cacheRoot(), `v${version}`);
const binPath = join(binDir, target.bin);

if (!existsSync(binPath)) {
  const baseUrl = `https://github.com/qualterz/tiwi/releases/download/v${version}`;
  const archiveUrl = `${baseUrl}/${asset}`;
  const sumsUrl = `${baseUrl}/SHA256SUMS.txt`;

  console.error(`Downloading tiwi v${version} (${key})...`);
  const [archiveRes, sumsRes] = await Promise.all([fetch(archiveUrl), fetch(sumsUrl)]);
  if (!archiveRes.ok) {
    console.error(`Failed to download ${archiveUrl}: HTTP ${archiveRes.status} ${archiveRes.statusText}`);
    process.exit(1);
  }
  if (!sumsRes.ok) {
    console.error(`Failed to download ${sumsUrl}: HTTP ${sumsRes.status} ${sumsRes.statusText}`);
    process.exit(1);
  }

  const [archiveBuf, sumsText] = await Promise.all([
    archiveRes.arrayBuffer().then((ab) => Buffer.from(ab)),
    sumsRes.text(),
  ]);

  const expectedHash = findChecksum(sumsText, asset);
  if (!expectedHash) {
    console.error(`Could not find checksum for ${asset} in SHA256SUMS.txt.`);
    process.exit(1);
  }
  const actualHash = createHash("sha256").update(archiveBuf).digest("hex");
  if (actualHash !== expectedHash) {
    console.error(`SHA256 verification failed for ${asset} (expected ${expectedHash}, got ${actualHash}).`);
    process.exit(1);
  }

  mkdirSync(binDir, { recursive: true });
  const tmpDir = mkdtempSync(join(binDir, ".tmp-"));
  try {
    const archivePath = join(tmpDir, asset);
    writeFileSync(archivePath, archiveBuf);

    const tarBin =
      process.platform === "win32"
        ? join(process.env.SystemRoot || "C:\\Windows", "System32", "tar.exe")
        : "tar";
    const tar = spawnSync(tarBin, ["-xf", archivePath, "-C", tmpDir], { stdio: "inherit" });
    if (tar.error || tar.status !== 0) {
      console.error(`Failed to extract ${asset}${tar.error ? `: ${tar.error.message}` : ""}`);
      process.exit(1);
    }

    const extracted = join(tmpDir, target.bin);
    if (!existsSync(extracted)) {
      console.error(`Extracted archive does not contain ${target.bin}.`);
      process.exit(1);
    }
    if (process.platform !== "win32") {
      chmodSync(extracted, 0o755);
    }
    try {
      renameSync(extracted, binPath);
    } catch (err) {
      if (!existsSync(binPath)) throw err;
    }
  } finally {
    rmSync(tmpDir, { recursive: true, force: true });
  }
}

const run = spawnSync(binPath, process.argv.slice(2), { stdio: "inherit" });
if (run.error) {
  console.error(`Failed to execute ${binPath}: ${run.error.message}`);
  process.exit(1);
}
if (run.signal) {
  process.kill(process.pid, run.signal);
}
process.exit(run.status ?? 1);
