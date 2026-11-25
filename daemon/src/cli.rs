use std::net::IpAddr;
use std::path::PathBuf;

use clap::Parser;

#[derive(Debug, Parser)]
#[command(
    name = "tiwi",
    version,
    about = "Zero-infrastructure P2P live streaming daemon"
)]
pub struct Cli {
    /// Path to config.toml (overrides automatic discovery)
    #[arg(long)]
    pub config: Option<PathBuf>,

    /// HTTP bind address for WHIP ingest
    #[arg(long, env = "TIWI_BIND")]
    pub bind: Option<IpAddr>,

    /// HTTP port for WHIP ingest
    #[arg(short, long, env = "TIWI_PORT")]
    pub port: Option<u16>,

    /// Fixed UDP port for WebRTC media (0 = OS-assigned)
    #[arg(long, env = "TIWI_MEDIA_PORT")]
    pub media_port: Option<u16>,

    /// Static public IP to advertise in ICE candidates (skips STUN when set)
    #[arg(long, env = "TIWI_PUBLIC_IP")]
    pub public_ip: Option<IpAddr>,

    /// STUN server address (host:port) for public IP discovery
    #[arg(long)]
    pub stun_server: Option<String>,

    /// MQTT over WebSocket broker URL for viewer signaling
    #[arg(long, env = "TIWI_MQTT_URL")]
    pub mqtt_url: Option<String>,

    /// Base viewer URL used when printing the shareable watch link
    #[arg(long)]
    pub viewer_url: Option<String>,

    /// Stream token for this session only (does not modify config.toml)
    #[arg(long, env = "TIWI_TOKEN", conflicts_with = "new_token")]
    pub token: Option<String>,

    /// Generate a new random stream token and save it to config.toml
    #[arg(long)]
    pub new_token: bool,

    /// Optional Bearer token required on WHIP POST/DELETE requests
    #[arg(long, env = "TIWI_WHIP_TOKEN")]
    pub whip_token: Option<String>,

    /// Maximum simultaneous viewers
    #[arg(long)]
    pub max_peers: Option<usize>,

    /// Maximum total viewer egress bitrate in Mbps (0 = unlimited)
    #[arg(long)]
    pub max_uplink_mbps: Option<f64>,

    /// Ingest media silence timeout in seconds (0 = disabled)
    #[arg(long)]
    pub ingest_timeout_secs: Option<u64>,

    /// Enable verbose debug logging
    #[arg(short, long)]
    pub verbose: bool,
}
