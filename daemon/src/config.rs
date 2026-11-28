use std::fmt::Write as _;
use std::fs;
use std::net::{IpAddr, Ipv4Addr};
use std::path::{Path, PathBuf};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use clap::Args as _;
use clap_serde_derive::ClapSerde;
use rand::RngCore;
use serde::Serialize;
use tiwi_daemon::{DEFAULT_MQTT_URL, DEFAULT_STUN_SERVER, DEFAULT_VIEWER_URL};
use tracing::warn;

use crate::cli::Cli;

#[derive(ClapSerde, Serialize, Debug, Clone)]
pub struct Config {
    /// Stream token for the watch link
    ///
    /// This shared secret identifies your stream and encrypts signaling. Passing --token or TIWI_TOKEN overrides it for one run without changing config.toml.
    ///
    /// Values: non-empty URL-safe string. Leave empty ("") to generate and save a random token on first run.
    #[arg(long, env = "TIWI_TOKEN", help_heading = "Stream")]
    pub token: String,

    /// Optional Bearer token for WHIP ingest
    ///
    /// When set, broadcasters must send "Authorization: Bearer <token>" on WHIP POST and DELETE requests. Set this if bind is not a loopback address.
    ///
    /// Values: non-empty string. Leave unset to allow unauthenticated WHIP ingest.
    #[arg(
        long,
        env = "TIWI_WHIP_TOKEN",
        value_name = "TOKEN",
        help_heading = "Stream"
    )]
    pub whip_token: Option<String>,

    /// Local IP address for WHIP ingest
    ///
    /// Network interface where the WHIP server listens (http://<bind>:<port>/whip).
    ///
    /// Values: IPv4 or IPv6 address (e.g. "127.0.0.1" for local-only, "0.0.0.0" for all interfaces, "::1").
    #[default(IpAddr::V4(Ipv4Addr::LOCALHOST))]
    #[arg(long, env = "TIWI_BIND", help_heading = "Networking")]
    pub bind: IpAddr,

    /// HTTP port for WHIP ingest
    ///
    /// TCP port where OBS or other WHIP broadcasters push streams (http://<bind>:<port>/whip).
    ///
    /// Values: 1-65535.
    #[default(2000)]
    #[arg(short, long, env = "TIWI_PORT", help_heading = "Networking")]
    pub port: u16,

    /// Static public IP for WebRTC ICE candidates
    ///
    /// Set this if you run behind a static IP or port-forwarded router. When set, tiwi skips STUN discovery.
    ///
    /// Values: IPv4 or IPv6 address (e.g. "203.0.113.10"). Leave unset to find your public IP via stun_server.
    #[arg(
        long,
        env = "TIWI_PUBLIC_IP",
        value_name = "IP",
        help_heading = "Networking"
    )]
    pub public_ip: Option<IpAddr>,

    /// Base fixed UDP port for WebRTC media
    ///
    /// If you forward UDP ports through a firewall or NAT, set a fixed port. tiwi binds media_port for ingest and media_port + 1..=shards for viewers.
    ///
    /// Values: 0 (OS-assigned ports) or 1-65535.
    #[default(0)]
    #[arg(long, env = "TIWI_MEDIA_PORT", help_heading = "Networking")]
    pub media_port: u16,

    /// Maximum simultaneous viewers
    ///
    /// tiwi rejects new viewers once the connected count reaches this limit.
    ///
    /// Values: 0 (unlimited) or positive integer (e.g. 10).
    #[default(0)]
    #[arg(long, help_heading = "Relay & Limits")]
    pub max_peers: usize,

    /// Maximum total viewer upload bitrate in Mbps
    ///
    /// Caps total upload bandwidth across all viewers so the stream does not saturate your upload link.
    ///
    /// Values: 0.0 (unlimited) or positive float in Mbps (e.g. 25.0).
    #[default(0.0)]
    #[arg(long, alias = "max-uplink", help_heading = "Relay & Limits")]
    pub max_uplink_mbps: f64,

    /// Save daemon logs and stream stats to disk
    ///
    /// Each run writes a plain-text .log file and a compressed .metrics.jsonl.gz stats file to logs_dir. tiwi keeps the 20 newest files of each type. Set false to log to stdout only (e.g. under systemd).
    ///
    /// Values: true (enabled) or false (stdout-only).
    #[default(true)]
    #[arg(long, action = clap::ArgAction::Set, help_heading = "Logging")]
    pub logs: bool,

    /// Folder where daemon logs and stream stats are saved
    ///
    /// When logs is true, tiwi writes .log files and .metrics.jsonl.gz stats (viewer count, bitrate, latency, packet loss) here.
    ///
    /// Values: folder path (e.g. "C:/logs/tiwi" or "/var/log/tiwi"). Leave unset or "" to use the OS local app data folder (tiwi/logs).
    #[arg(
        long,
        env = "TIWI_LOGS_DIR",
        value_name = "PATH",
        help_heading = "Logging"
    )]
    pub logs_dir: Option<PathBuf>,

    /// Base viewer URL for the watch link
    ///
    /// tiwi prints "<viewer_url>#<token>" at startup. If you host your own viewer, set its URL here.
    ///
    /// Values: "http://..." or "https://..." URL.
    #[default(DEFAULT_VIEWER_URL.to_string())]
    #[arg(long, help_heading = "Infrastructure")]
    pub viewer_url: String,

    /// MQTT over WebSocket broker URL for viewer signaling
    ///
    /// tiwi uses this MQTT broker to exchange encrypted WebRTC SDP offers and answers with viewers.
    ///
    /// Values: "ws://..." or "wss://..." URL.
    #[default(DEFAULT_MQTT_URL.to_string())]
    #[arg(long, env = "TIWI_MQTT_URL", help_heading = "Infrastructure")]
    pub mqtt_url: String,

    /// STUN server for public IP discovery
    ///
    /// tiwi queries this server to find your public IP for WebRTC ICE candidates. If you set public_ip, tiwi ignores this setting.
    ///
    /// Values: "host:port" (e.g. "stun.l.google.com:19302", "stun.cloudflare.com:3478").
    #[default(DEFAULT_STUN_SERVER.to_string())]
    #[arg(long, help_heading = "Infrastructure")]
    pub stun_server: String,

    #[arg(skip)]
    #[serde(skip)]
    pub config_path: Option<PathBuf>,
}

impl Config {
    pub fn load(mut cli: Cli) -> Result<Self, String> {
        let exe_dir = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(Path::to_path_buf));
        let home_config_dir = dirs::config_dir().map(|d| d.join("tiwi"));

        // 1. Discover config file: --config > <exe_dir>/config.toml > <home_config_dir>/config.toml
        let discovered = if let Some(explicit) = cli.config.clone() {
            if !explicit.exists() {
                return Err(format!("Config file not found: {}", explicit.display()));
            }
            Some(explicit)
        } else if let Some(p) = exe_dir
            .as_deref()
            .map(|d| d.join("config.toml"))
            .filter(|p| p.exists())
        {
            Some(p)
        } else {
            home_config_dir
                .as_deref()
                .map(|d| d.join("config.toml"))
                .filter(|p| p.exists())
        };

        // Take CLI token before merging so it stays session-only and never touches disk.
        let cli_token = cli.opts.token.take().filter(|t| !t.is_empty());

        // 2. Layer precedence: defaults < config.toml < env < CLI
        let mut cfg = if let Some(ref path) = discovered {
            let raw = fs::read_to_string(path)
                .map_err(|e| format!("Failed to read {}: {e}", path.display()))?;
            let file_opts: <Self as ClapSerde>::Opt = toml::from_str(&raw)
                .map_err(|e| format!("Failed to parse {}: {e}", path.display()))?;
            Self::from(file_opts).merge(&mut cli.opts)
        } else {
            Self::from(&mut cli.opts)
        };
        cfg.config_path = discovered;

        // Normalize empty optional values
        if cfg.whip_token.as_deref().is_some_and(str::is_empty) {
            cfg.whip_token = None;
        }
        if cfg
            .logs_dir
            .as_deref()
            .is_some_and(|p| p.as_os_str().is_empty())
        {
            cfg.logs_dir = None;
        }

        // 3. Resolve stream token
        if let Some(token) = cli_token {
            cfg.token = token;
        } else if cli.regenerate_token || cfg.token.is_empty() {
            let new_token = generate_token();
            let save_path = cfg
                .config_path
                .clone()
                .or_else(|| home_config_dir.map(|d| d.join("config.toml")));

            if let Some(path) = save_path {
                if let Err(e) = persist_token(&path, &new_token) {
                    warn!(
                        path = %path.display(),
                        err = %e,
                        "Could not save token; using ephemeral token for this run"
                    );
                } else {
                    cfg.config_path = Some(path);
                }
            }
            cfg.token = new_token;
        }

        Ok(cfg)
    }

    pub fn watch_url(&self) -> String {
        let base = self.viewer_url.trim_end_matches('#');
        format!("{base}#{}", self.token)
    }

    pub fn resolved_logs_dir(&self) -> Option<PathBuf> {
        if !self.logs {
            return None;
        }
        self.logs_dir.clone().or_else(tiwi_daemon::default_logs_dir)
    }

    /// Generates a documented default `config.toml` directly from `Config` metadata and defaults.
    pub fn default_toml() -> String {
        let defaults = toml::Value::try_from(Self::from(<Self as ClapSerde>::Opt::default()))
            .expect("Config default must serialize to TOML");
        let table = defaults
            .as_table()
            .expect("Config default must serialize to a TOML table");
        let cmd = <Self as ClapSerde>::Opt::augment_args(clap::Command::new("config"));

        let mut out = String::from(
            "# Tiwi Daemon Configuration\n\
             #\n\
             # Discovery order:\n\
             #   1. --config <path>\n\
             #   2. config.toml next to the tiwi executable (portable mode)\n\
             #   3. User config directory (e.g. %APPDATA%\\tiwi\\config.toml, ~/.config/tiwi/config.toml)\n\
             #\n\
             # Precedence: defaults < config.toml < environment variables < CLI flags.\n\
             # Optional settings are commented out. Uncomment a line to enable one.\n",
        );

        let mut current_heading: Option<&str> = None;
        for arg in cmd.get_arguments() {
            let heading = arg.get_help_heading();
            if heading != current_heading {
                current_heading = heading;
                if let Some(h) = heading {
                    let _ = write!(out, "\n# --- {h} ---\n");
                }
            }

            let key = arg.get_id().as_str();
            out.push('\n');

            if let Some(help) = arg.get_long_help().or_else(|| arg.get_help()) {
                for line in help.to_string().lines() {
                    if line.is_empty() {
                        continue;
                    }
                    let _ = writeln!(out, "# {line}");
                }
            }
            if let Some(env) = arg.get_env() {
                let _ = writeln!(out, "# Env: {}", env.to_string_lossy());
            }

            if let Some(val) = table.get(key) {
                let _ = writeln!(out, "{key} = {val}");
            } else {
                let placeholder = arg
                    .get_value_names()
                    .and_then(|names| names.first())
                    .map(|n| n.as_str())
                    .unwrap_or("");
                let _ = writeln!(out, "# {key} = \"<{placeholder}>\"");
            }
        }

        out
    }
}

pub fn generate_token() -> String {
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

/// Surgically updates or inserts `token = "..."` in `config.toml` while preserving
/// existing user comments, formatting, and untouched keys.
fn persist_token(path: &Path, token: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let token_line = format!("token = \"{token}\"");
    let existing = if path.exists() {
        fs::read_to_string(path)?
    } else {
        Config::default_toml()
    };

    let mut replaced = false;
    let mut lines: Vec<String> = existing
        .lines()
        .map(|line| {
            let trimmed = line.trim_start();
            if !replaced && !trimmed.starts_with('#') && is_token_key_line(trimmed) {
                replaced = true;
                token_line.clone()
            } else {
                line.to_string()
            }
        })
        .collect();

    if !replaced {
        lines.insert(0, token_line);
    }

    let mut out = lines.join("\n");
    out.push('\n');
    fs::write(path, out)
}

fn is_token_key_line(trimmed: &str) -> bool {
    let Some(rest) = trimmed.strip_prefix("token") else {
        return false;
    };
    rest.trim_start().starts_with('=')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_toml_roundtrips() {
        let generated = Config::default_toml();
        let parsed: <Config as ClapSerde>::Opt =
            toml::from_str(&generated).expect("generated config.toml must be valid TOML");
        let cfg = Config::from(parsed);
        let expected = Config::from(<Config as ClapSerde>::Opt::default());
        assert_eq!(cfg.bind, expected.bind);
        assert_eq!(cfg.port, expected.port);
        assert_eq!(cfg.media_port, expected.media_port);
        assert_eq!(cfg.public_ip, expected.public_ip);
        assert_eq!(cfg.stun_server, expected.stun_server);
        assert_eq!(cfg.mqtt_url, expected.mqtt_url);
        assert_eq!(cfg.viewer_url, expected.viewer_url);
        assert_eq!(cfg.token, expected.token);
        assert_eq!(cfg.whip_token, expected.whip_token);
        assert_eq!(cfg.max_peers, expected.max_peers);
        assert_eq!(cfg.max_uplink_mbps, expected.max_uplink_mbps);
        assert_eq!(cfg.logs, expected.logs);
        assert_eq!(cfg.logs_dir, expected.logs_dir);
    }
}
