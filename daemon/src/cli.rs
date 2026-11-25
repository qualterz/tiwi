use std::path::PathBuf;

use clap::Parser;
use clap_serde_derive::ClapSerde;

use crate::config::Config;

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

    /// Generate a new random stream token and save it to config.toml
    #[arg(long, conflicts_with = "token")]
    pub new_token: bool,

    /// Enable verbose debug logging
    #[arg(short, long)]
    pub verbose: bool,

    #[command(flatten)]
    pub opts: <Config as ClapSerde>::Opt,
}
