use std::fs;
use std::net::{IpAddr, Ipv4Addr};
use std::path::{Path, PathBuf};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rand::RngCore;
use serde::Deserialize;
use tracing::warn;

use crate::cli::Cli;

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct FileConfig {
    bind: Option<IpAddr>,
    port: Option<u16>,
    media_port: Option<u16>,
    public_ip: Option<IpAddr>,
    stun_server: Option<String>,
    mqtt_url: Option<String>,
    viewer_url: Option<String>,
    token: Option<String>,
    whip_token: Option<String>,
    max_peers: Option<usize>,
    max_uplink_mbps: Option<f64>,
    ingest_timeout_secs: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub bind: IpAddr,
    pub port: u16,
    pub media_port: u16,
    pub public_ip: Option<IpAddr>,
    pub stun_server: String,
    pub mqtt_url: String,
    pub viewer_url: String,
    pub token: String,
    pub whip_token: Option<String>,
    pub max_peers: usize,
    pub max_uplink_mbps: f64,
    pub ingest_timeout_secs: u64,
    pub verbose: bool,
    pub config_path: Option<PathBuf>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            bind: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: 2000,
            media_port: 0,
            public_ip: None,
            stun_server: "stun.l.google.com:19302".to_string(),
            mqtt_url: "wss://broker.emqx.io:8084/mqtt".to_string(),
            viewer_url: "https://qualterz.github.io/tiwi/".to_string(),
            token: String::new(),
            whip_token: None,
            max_peers: 16,
            max_uplink_mbps: 0.0,
            ingest_timeout_secs: 15,
            verbose: false,
            config_path: None,
        }
    }
}

impl Config {
    pub fn load(cli: Cli) -> Result<Self, String> {
        let exe_dir = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(Path::to_path_buf));
        let home_config_dir = dirs::config_dir().map(|d| d.join("tiwi"));
        let mut cfg = Self::default();

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

        if let Some(ref path) = discovered {
            let raw = fs::read_to_string(path)
                .map_err(|e| format!("Failed to read {}: {e}", path.display()))?;
            let file_cfg: FileConfig = toml::from_str(&raw)
                .map_err(|e| format!("Failed to parse {}: {e}", path.display()))?;
            cfg.apply_file(file_cfg);
            cfg.config_path = Some(path.clone());
        }

        // 2. Apply CLI flag overrides
        if let Some(bind) = cli.bind {
            cfg.bind = bind;
        }
        if let Some(port) = cli.port {
            cfg.port = port;
        }
        if let Some(media_port) = cli.media_port {
            cfg.media_port = media_port;
        }
        if let Some(public_ip) = cli.public_ip {
            cfg.public_ip = Some(public_ip);
        }
        if let Some(stun_server) = cli.stun_server {
            cfg.stun_server = stun_server;
        }
        if let Some(mqtt_url) = cli.mqtt_url {
            cfg.mqtt_url = mqtt_url;
        }
        if let Some(viewer_url) = cli.viewer_url {
            cfg.viewer_url = viewer_url;
        }
        if let Some(whip_token) = cli.whip_token {
            cfg.whip_token = (!whip_token.is_empty()).then_some(whip_token);
        }
        if let Some(max_peers) = cli.max_peers {
            cfg.max_peers = max_peers;
        }
        if let Some(max_uplink_mbps) = cli.max_uplink_mbps {
            cfg.max_uplink_mbps = max_uplink_mbps;
        }
        if let Some(ingest_timeout_secs) = cli.ingest_timeout_secs {
            cfg.ingest_timeout_secs = ingest_timeout_secs;
        }
        cfg.verbose = cli.verbose;

        // Normalize empty whip_token from file
        if cfg.whip_token.as_deref().is_some_and(str::is_empty) {
            cfg.whip_token = None;
        }

        // 3. Resolve stream token
        if let Some(cli_token) = cli.token.filter(|t| !t.is_empty()) {
            // Session-only override; never writes disk.
            cfg.token = cli_token;
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

    fn apply_file(&mut self, f: FileConfig) {
        if let Some(v) = f.bind {
            self.bind = v;
        }
        if let Some(v) = f.port {
            self.port = v;
        }
        if let Some(v) = f.media_port {
            self.media_port = v;
        }
        if let Some(v) = f.public_ip {
            self.public_ip = Some(v);
        }
        if let Some(v) = f.stun_server {
            self.stun_server = v;
        }
        if let Some(v) = f.mqtt_url {
            self.mqtt_url = v;
        }
        if let Some(v) = f.viewer_url {
            self.viewer_url = v;
        }
        if let Some(v) = f.token {
            self.token = v;
        }
        if let Some(v) = f.whip_token {
            self.whip_token = Some(v);
        }
        if let Some(v) = f.max_peers {
            self.max_peers = v;
        }
        if let Some(v) = f.max_uplink_mbps {
            self.max_uplink_mbps = v;
        }
        if let Some(v) = f.ingest_timeout_secs {
            self.ingest_timeout_secs = v;
        }
    }

    pub fn watch_url(&self) -> String {
        let base = self.viewer_url.trim_end_matches('#');
        format!("{base}#{}", self.token)
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
