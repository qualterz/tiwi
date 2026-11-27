# Tiwi OBS Studio Plugin

Native OBS Studio plugin embedding the Rust Tiwi WebRTC relay (`daemon/`) directly into OBS as an independent streaming output (reading encoder/bitrate settings from **Settings → Output → Streaming**) with a native Qt6 dock.

## Architecture

- **`obs/rust/`** — Thin panic-safe `extern "C"` `staticlib` crate (`tiwi_obs`) wrapping `tiwi_daemon::DirectEngine` and forwarding `tracing` logs to OBS `blog()`.
- **`obs/src/`** — Thin C++/Qt6 OBS shim:
  - `tiwi-service.cpp`: `obs_service_info` (`tiwi_service`) enforcing `bf = 0`, `keyint_sec = 2`, `repeat_headers = true` for WebRTC.
  - `tiwi-output.cpp`: `obs_output_info` (`OBS_OUTPUT_AV | OBS_OUTPUT_ENCODED | OBS_OUTPUT_SERVICE`) forwarding H.264 / HEVC / AV1 + Opus frames directly into Rust for RTP packetization and WebRTC fanout.
  - `tiwi-dock.cpp`: Native OBS-styled Qt6 dock managing the standalone stream, token, watch link, and 1-second telemetry status bar.
  - `plugin-main.cpp`: Module registration, Rust log callback hookup, and clean shutdown on `OBS_FRONTEND_EVENT_EXIT`.
- **`obs/CMakeLists.txt` + `obs/buildspec.json` + `obs/CMakePresets.json`** — Self-contained deterministic CMake build (automatically fetches & verifies pinned `obs-studio`, `obs-deps`, and `qt6` on Windows/macOS, or links system `libobs-dev` on Linux).

## Building

### Windows (x64)

Requires Visual Studio (Desktop development with C++ + Windows 10/11 SDK), CMake 3.28+, and Rust (`cargo`).

```powershell
cd obs
cmake --preset windows-x64
cmake --build --preset windows-x64
```

Output: `obs/build_x64/RelWithDebInfo/tiwi-obs.dll`

Install:

- `C:\ProgramData\obs-studio\plugins\tiwi-obs\tiwi-obs.dll`
- `C:\ProgramData\obs-studio\plugins\tiwi-obs\data\locale\en-US.ini`

### Linux (Ubuntu x86_64)

Requires `libobs-dev` (`ppa:obsproject/obs-studio`), `qt6-base-dev`, `ninja-build`, `cmake`, and Rust (`cargo`).

```bash
cd obs
cmake --preset ubuntu-x86_64
cmake --build --preset ubuntu-x86_64
```

Install: `~/.config/obs-studio/plugins/tiwi-obs/bin/64bit/tiwi-obs.so` and `data/locale/en-US.ini`.

### macOS (Universal)

Requires Xcode, CMake 3.28+, and Rust with `aarch64-apple-darwin` + `x86_64-apple-darwin` targets.

```bash
cd obs
cmake --preset macos
cmake --build --preset macos
```

Install: copy `obs/build_macos/RelWithDebInfo/tiwi-obs.plugin` to `~/Library/Application Support/obs-studio/plugins/`.

### Rust core unit tests

```bash
cargo test --manifest-path obs/rust/Cargo.toml
```
