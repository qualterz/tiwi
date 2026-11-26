use std::path::PathBuf;

use clap::Parser;
use clap_serde_derive::ClapSerde;

use crate::config::Config;

#[derive(Parser)]
#[command(
    name = "tiwi",
    version,
    disable_version_flag = true,
    about = "Zero-infrastructure peer-to-peer live streaming daemon"
)]
pub struct Cli {
    /// Generate a new random stream token and save it to config.toml
    #[arg(long, conflicts_with = "token")]
    pub regenerate_token: bool,

    /// Path to config.toml (overrides automatic discovery)
    #[arg(long)]
    pub config: Option<PathBuf>,

    /// Print default config.toml and exit
    #[arg(long)]
    pub default_config: bool,

    /// Enable verbose debug logging
    #[arg(long)]
    pub verbose: bool,

    /// Print version
    #[arg(long, action = clap::ArgAction::Version)]
    pub version: Option<bool>,

    #[command(flatten)]
    pub opts: <Config as ClapSerde>::Opt,
}
