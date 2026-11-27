# Tiwi

Zero-infrastructure peer-to-peer live streaming. Stream directly from your machine to browsers over WebRTC with no servers to host.

## Quick start

### OBS Studio plugin

The native OBS Studio 33 plugin embeds the Rust WebRTC relay directly into OBS as an independent output with its own **Tiwi** dock. It uses your OBS streaming settings (**Settings → Output → Streaming**), and on first launch offers to configure recommended low-latency defaults in your current OBS profile.

- **Windows (x64):** Download and run the [Windows Installer (.msi)](https://github.com/qualterz/tiwi/releases/latest/download/tiwi-obs-windows-x64.msi), or run in PowerShell:
  ```powershell
  irm https://qualterz.github.io/tiwi/install.ps1 | iex
  ```
- **macOS (Universal) / Linux (x64, native or Flatpak):**
  ```sh
  curl -fsSL https://qualterz.github.io/tiwi/install.sh | sh
  ```
- **Manual install:** Download the archive from [GitHub Releases](https://github.com/qualterz/tiwi/releases) and extract it to the OBS plugins directory (`C:\ProgramData\obs-studio\plugins\` on Windows, `~/.config/obs-studio/plugins/` on Linux, or `~/Library/Application Support/obs-studio/plugins/` on macOS).

Open OBS Studio, click **Start Streaming** in the **Tiwi** dock (or via **Docks → Tiwi**), then click **Copy Link** to share the watch URL (`https://qualterz.github.io/tiwi/#<token>`).

### Standalone daemon (WHIP)

Use the standalone `tiwi` daemon with OBS or any WHIP-compatible broadcaster.

Run it with `npx` (requires Node.js ≥ 18). This command downloads and caches the release binary automatically:

```bash
npx @qualterz/tiwi
```

You can also download a prebuilt binary from [GitHub Releases](https://github.com/qualterz/tiwi/releases) and run `tiwi`.

To stream from OBS:

1. Open **Settings → Stream** and choose **WHIP** as the service.
2. Set the server to `http://127.0.0.1:2000/whip`.
3. Start streaming, then share the watch URL that `tiwi` prints.

## Configuration

The OBS plugin uses OBS's streaming settings (**Settings → Output → Streaming**) and stores the stream token (and optional overrides) in its plugin `config.json`.

The standalone daemon checks these locations for `config.toml` in order:

1. `--config <path>`
2. `config.toml` next to the `tiwi` binary (portable mode)
3. User config directory (`%APPDATA%\tiwi\config.toml`, `~/.config/tiwi/config.toml`, or `~/Library/Application Support/tiwi/config.toml`)

Settings follow this precedence: defaults < `config.toml` < environment variables < CLI flags. Run `tiwi --default-config` to generate a documented `config.toml`, or run `tiwi --help` to list all CLI flags.

## How it works

The Rust relay (`str0m`) accepts encoded H.264, HEVC (H.265), or AV1 video with Opus audio, either directly from OBS or over local WHIP. It distributes RTP packets over DTLS-SRTP across sharded UDP egress threads.

The 256-bit stream token stays in the watch URL's `#fragment`, so requests to the static viewer host never include it. Both sides derive the MQTT topic namespace with `HMAC-SHA256(token, "stream-id")`. They encrypt all SDP offers, answers, and stream start/stop events with `AES-256-GCM` (`SHA-256(token)`) and apply IV replay deduplication.

### Limits

- Broadcaster upload bandwidth scales linearly with viewer count. In the standalone daemon, set `max_peers` or `max_uplink_mbps` to avoid saturating your connection.
- NAT traversal uses STUN only, with no TURN relay. Symmetric NATs or strict firewalls may block direct peer connections unless you forward UDP ports (`media_port` through `media_port + shards` in the standalone daemon).
- WebRTC playback requires no B-frames (`bf=0`). The OBS plugin enforces this setting automatically.

## Development

The repository has three components:

- [`daemon/`](daemon): Rust WebRTC relay library (`tiwi_daemon`), CLI/TUI binary (`tiwi`), and npm wrapper (`daemon/npm`).
- [`obs/`](obs): Native OBS Studio plugin (C++/Qt6 + Rust FFI). See [`obs/README.md`](obs/README.md) for platform prerequisites and CMake build instructions.
- [`web/`](web): Static SolidJS + Vite landing page and web player deployed to GitHub Pages.

```bash
# Daemon
cargo test --manifest-path daemon/Cargo.toml
cargo run --manifest-path daemon/Cargo.toml

# OBS plugin Rust core tests
cargo test --manifest-path obs/rust/Cargo.toml

# Web landing & viewer (requires Node.js)
cd web
npm install
npm run dev        # local dev server on :3000
npm run typecheck
npm run build

# Pre-commit formatting hook
git config core.hooksPath .githooks
```
