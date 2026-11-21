import fs from "fs";
import path from "path";
import { parseArgs } from "util";

const { values, positionals } = parseArgs({
  args: Bun.argv.slice(2),
  options: {
    version: { type: "string" },
    commit: { type: "string" },
  },
  allowPositionals: true,
});

const mode = positionals[0] ?? "npm";
if (mode !== "npm" && mode !== "exe") {
  console.error("Usage: bun run scripts/build.ts <npm|exe> [--version X.Y.Z] [--commit SHA]");
  process.exit(1);
}

const daemonDir = path.resolve(import.meta.dir, "..");
const distDir = path.join(daemonDir, "dist");

const gitSha = () => {
  const proc = Bun.spawnSync(["git", "rev-parse", "--short", "HEAD"], {
    cwd: daemonDir,
  });
  return proc.exitCode === 0 ? proc.stdout.toString().trim() : "dev";
};

const version = values.version || "0.0.0-dev";
const commit = values.commit || gitSha();

fs.rmSync(distDir, { recursive: true, force: true });
fs.mkdirSync(distDir, { recursive: true });

if (mode === "npm") {
  const res = await Bun.build({
    entrypoints: [
      path.join(daemonDir, "src/index.ts"),
      path.join(daemonDir, "src/worker.ts"),
    ],
    outdir: distDir,
    target: "bun",
    format: "esm",
    packages: "external",
    define: {
      __TIWI_VERSION__: JSON.stringify(version),
      __TIWI_COMMIT__: JSON.stringify(commit),
      __TIWI_BUILD__: JSON.stringify("npm"),
    },
  });

  if (!res.success) {
    for (const log of res.logs) console.error(log);
    process.exit(1);
  }

  const indexPath = path.join(distDir, "index.js");
  const code = fs
    .readFileSync(indexPath, "utf8")
    .replaceAll("./worker.ts", "./worker.js");
  fs.writeFileSync(
    indexPath,
    code.startsWith("#!") ? code : `#!/usr/bin/env bun\n${code}`,
  );
  console.log(`Built npm bundle in ${distDir} (version=${version}, commit=${commit})`);
} else {
  const winVersion = /^\d+\.\d+\.\d+$/.test(version) ? `${version}.0` : "0.0.0.0";
  const outFile = path.join(distDir, "tiwi.exe");
  const proc = Bun.spawnSync(
    [
      process.execPath,
      "build",
      "--compile",
      path.join(daemonDir, "src/index.ts"),
      path.join(daemonDir, "src/worker.ts"),
      "--outfile",
      outFile,
      "--define",
      `__TIWI_VERSION__=${JSON.stringify(version)}`,
      "--define",
      `__TIWI_COMMIT__=${JSON.stringify(commit)}`,
      "--define",
      `__TIWI_BUILD__="exe"`,
      "--windows-title=Tiwi",
      "--windows-description=Zero-infrastructure peer-to-peer live streaming daemon",
      `--windows-version=${winVersion}`,
    ],
    { cwd: daemonDir, stdio: ["inherit", "inherit", "inherit"] },
  );
  if (proc.exitCode !== 0) process.exit(proc.exitCode);
  console.log(`Built executable ${outFile} (version=${version}, commit=${commit})`);
}
