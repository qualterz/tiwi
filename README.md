# Tiwi

Zero-infrastructure peer-to-peer live streaming. Stream directly from your machine to browsers over WebRTC with no servers to host.

## Quick start

### OBS Studio plugin

The native OBS Studio 33 plugin embeds the Rust WebRTC relay directly into OBS as an independent output. It uses your encoder and bitrate settings from **Settings → Output → Streaming**.

1. Download the `tiwi-obs` archive for your OS from [GitHub Releases](https://github.com/qualterz/tiwi/releases).
2. Extract the archive to the OBS plugins directory:
   - Windows (x64): `C:\ProgramData\obs-studio\plugins\`
   - Linux (x64): `~/.config/obs-studio/plugins/`
   - macOS (Universal): `~/Library/Application Support/obs-studio/plugins/`
3. Restart OBS and open **Docks → Tiwi**.
4. In the **Tiwi** dock, click **Start Streaming**, then click **Copy Link** to share the watch URL (`https://qualterz.github.io/tiwi/#<token>`).

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

The settings below apply to the standalone daemon. The OBS plugin stores `token`, `viewer_url`, `mqtt_url`, and `stun_server` separately in its OBS plugin `config.json` and lets you edit or regenerate the token in the **Tiwi** dock.

The standalone daemon checks these locations for `config.toml` in order:

1. `--config <path>`
2. `config.toml` next to the `tiwi` binary (portable mode)
3. User config directory (`%APPDATA%\tiwi\config.toml`, `~/.config/tiwi/config.toml`, or `~/Library/Application Support/tiwi/config.toml`)

Settings follow this precedence: defaults < `config.toml` < environment variables < CLI flags. Run `tiwi --default-config` to generate a documented `config.toml`, or run `tiwi --help` to list all CLI flags.

| Setting           | CLI / Env                            | Default                            | Purpose                                                                                                                                                                       |
| ----------------- | ------------------------------------ | ---------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `token`           | `--token` / `TIWI_TOKEN`             | Auto-generated                     | Shared stream secret for the watch URL and signaling encryption. CLI or env overrides apply to one run. Use `--regenerate-token` to rotate and save it.                       |
| `whip_token`      | `--whip-token` / `TIWI_WHIP_TOKEN`   | Unset                              | Requires `Authorization: Bearer <token>` on WHIP requests. Recommended if `bind` is non-loopback.                                                                             |
| `bind`            | `--bind` / `TIWI_BIND`               | `127.0.0.1`                        | Local IP address for WHIP ingest (`http://<bind>:<port>/whip`).                                                                                                               |
| `port`            | `-p, --port` / `TIWI_PORT`           | `2000`                             | Local TCP port for WHIP ingest (`http://<bind>:<port>/whip`).                                                                                                                 |
| `public_ip`       | `--public-ip` / `TIWI_PUBLIC_IP`     | Unset                              | Static public IP for ICE candidates. Skips STUN discovery.                                                                                                                    |
| `media_port`      | `--media-port` / `TIWI_MEDIA_PORT`   | `0` (OS-assigned)                  | Fixed base UDP port for firewall or NAT forwarding. Ingest uses `media_port`, and viewer shards use `media_port + 1..=shards` (up to 32, based on CPU cores and `max_peers`). |
| `max_peers`       | `--max-peers`                        | `0` (unlimited)                    | Maximum simultaneous viewers.                                                                                                                                                 |
| `max_uplink_mbps` | `--max-uplink-mbps` (`--max-uplink`) | `0.0` (unlimited)                  | Caps total viewer upload bandwidth in Mbps.                                                                                                                                   |
| `viewer_url`      | `--viewer-url`                       | `https://qualterz.github.io/tiwi/` | Base URL for printing the watch URL. Set this if you self-host `viewer/`.                                                                                                     |
| `mqtt_url`        | `--mqtt-url` / `TIWI_MQTT_URL`       | `wss://broker.emqx.io:8084/mqtt`   | MQTT-over-WebSocket broker for encrypted signaling. Must match `MQTT_URL` in `viewer/src/viewer.ts` if changed.                                                               |
| `stun_server`     | `--stun-server`                      | `stun.l.google.com:19302`          | STUN server for public IP discovery.                                                                                                                                          |
| `logs`            | `--logs <true\|false>`               | `true`                             | Writes rolling `.log` and `.metrics.jsonl.gz` files and keeps the 20 newest of each. Pass `--verbose` to log to stdout instead of the TUI.                                    |
| `logs_dir`        | `--logs-dir` / `TIWI_LOGS_DIR`       | OS app data (`tiwi/logs`)          | Directory for `.log` and `.metrics.jsonl.gz` files when `logs` is `true`.                                                                                                     |

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
- [`viewer/`](viewer): Static SolidJS + Vite web player deployed to GitHub Pages.

```bash
# Daemon
cargo test --manifest-path daemon/Cargo.toml
cargo run --manifest-path daemon/Cargo.toml

# OBS plugin Rust core tests
cargo test --manifest-path obs/rust/Cargo.toml

# Viewer (requires Bun)
cd viewer
bun install
bun run dev        # local dev server on :3000
bun run typecheck
bun run build

# Pre-commit formatting hook
git config core.hooksPath .githooks
```
