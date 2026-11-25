use std::fs;
use std::net::{IpAddr, Ipv4Addr};
use std::path::{Path, PathBuf};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use clap_serde_derive::ClapSerde;
use rand::RngCore;
use tracing::warn;

use crate::cli::Cli;

#[derive(ClapSerde)]
#[derive(Debug, Clone)]
pub struct Config {
    /// HTTP bind address for WHIP ingest
    #[default(IpAddr::V4(Ipv4Addr::LOCALHOST))]
    #[arg(long, env = "TIWI_BIND")]
    pub bind: IpAddr,

    /// HTTP port for WHIP ingest
    #[default(2000)]
    #[arg(short, long, env = "TIWI_PORT")]
    pub port: u16,

    /// Fixed UDP port for WebRTC media (0 = OS-assigned)
    #[default(0)]
    #[arg(long, env = "TIWI_MEDIA_PORT")]
    pub media_port: u16,

    /// Static public IP to advertise in ICE candidates (skips STUN when set)
    #[arg(long, env = "TIWI_PUBLIC_IP")]
    pub public_ip: Option<IpAddr>,

    /// STUN server address (host:port) for public IP discovery
    #[default("stun.l.google.com:19302".to_string())]
    #[arg(long)]
    pub stun_server: String,

    /// MQTT over WebSocket broker URL for viewer signaling
    #[default("wss://broker.emqx.io:8084/mqtt".to_string())]
    #[arg(long, env = "TIWI_MQTT_URL")]
    pub mqtt_url: String,

    /// Base viewer URL used when printing the shareable watch link
    #[default("https://qualterz.github.io/tiwi/".to_string())]
    #[arg(long)]
    pub viewer_url: String,

    /// Stream token for this session only (does not modify config.toml)
    #[arg(long, env = "TIWI_TOKEN")]
    pub token: String,

    /// Optional Bearer token required on WHIP POST/DELETE requests
    #[arg(long, env = "TIWI_WHIP_TOKEN")]
    pub whip_token: Option<String>,

    /// Maximum simultaneous viewers (0 = unlimited)
    #[default(0)]
    #[arg(long)]
    pub max_peers: usize,

    /// Maximum total viewer egress bitrate in Mbps (0 = unlimited)
    #[default(0.0)]
    #[arg(long)]
    pub max_uplink_mbps: f64,

    /// Ingest media silence timeout in seconds (0 = disabled)
    #[default(15)]
    #[arg(long)]
    pub ingest_timeout_secs: u64,

    /// Directory for per-session 1s JSONL.gz metrics files (default: appdata/tiwi/metrics)
    #[arg(long, env = "TIWI_METRICS_DIR")]
    pub metrics_dir: Option<PathBuf>,

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

        // 2. Layer precedence: defaults <- config.toml <- CLI / env
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

        // Normalize empty whip_token
        if cfg.whip_token.as_deref().is_some_and(str::is_empty) {
            cfg.whip_token = None;
        }

        // 3. Resolve stream token
        if let Some(token) = cli_token {
            cfg.token = token;
        } else if cli.new_token || cfg.token.is_empty() {
            let new_token = generate_token();
            let save_path = cfg
                .config_path
                .clone()
                .or_else(|| home_config_dir.map(|d| d.join("config.toml")));

            if let Some(path) = save_path {
                if let Err(e) = persist_token(&path, &new_token) {
                    warn!(
                        "Could not save token to {} ({e}); using ephemeral token for this run",
                        path.display()
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

    pub fn resolved_metrics_dir(&self) -> Option<PathBuf> {
        self.metrics_dir
            .clone()
            .or_else(|| dirs::data_local_dir().map(|d| d.join("tiwi").join("metrics")))
    }
}

pub fn generate_token() -> String {
    let mut bytes = [0u8; 24];
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
    if !path.exists() {
        return fs::write(path, format!("{token_line}\n"));
    }

    let existing = fs::read_to_string(path)?;
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
