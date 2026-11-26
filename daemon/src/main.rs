mod cli;
mod config;
mod relay;
mod signal;
mod whip;

use std::fs::{self, File};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use clap::Parser;
use tokio::sync::{mpsc, watch};
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

use crate::cli::Cli;
use crate::config::Config;

const MAX_LOG_FILES: usize = 20;

fn open_log_file(dir: &Path, ts: &str) -> std::io::Result<(PathBuf, File)> {
    fs::create_dir_all(dir)?;
    let raw_path = dir.join(format!("{ts}.log"));
    let path = std::path::absolute(&raw_path).unwrap_or(raw_path);
    let file = File::create(&path)?;
    relay::metrics::prune_old_files(dir, MAX_LOG_FILES, |n| n.ends_with(".log"));
    Ok((path, file))
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    if cli.default_config {
        print!("{}", Config::default_toml());
        return;
    }
    let verbose = cli.verbose;

    let cfg = match Config::load(cli) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Failed to load configuration: {e}");
            std::process::exit(1);
        }
    };

    let default_filter = if verbose {
        "tiwi=debug,str0m=info"
    } else {
        "tiwi=info"
    };
    let env_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_filter));

    let run_ts = relay::metrics::format_utc_filename_ts(SystemTime::now());
    let logs_dir = cfg.resolved_logs_dir();
    let mut log_file_err = None;
    let mut log_file_path = None;
    let file_layer = logs_dir
        .as_deref()
        .and_then(|dir| match open_log_file(dir, &run_ts) {
            Ok((path, file)) => {
                log_file_path = Some(path);
                Some(
                    tracing_subscriber::fmt::layer()
                        .with_ansi(false)
                        .with_target(false)
                        .with_writer(Mutex::new(file)),
                )
            }
            Err(e) => {
                log_file_err = Some((dir.to_path_buf(), e));
                None
            }
        });

    tracing_subscriber::registry()
        .with(env_filter)
        .with(tracing_subscriber::fmt::layer().with_target(false))
        .with(file_layer)
        .init();

    if let Some(path) = &cfg.config_path {
        info!(config = %path.display(), "Loaded configuration");
    }
    if let Some((dir, err)) = log_file_err {
        warn!(dir = %dir.display(), err = %err, "Could not open log file");
    }
    if let Some(path) = &log_file_path {
        info!(path = %path.display(), "Log file");
    }

    if !cfg.bind.is_loopback() && cfg.whip_token.is_none() {
        warn!(
            bind = %cfg.bind,
            "WHIP endpoint is exposed on a non-loopback address without whip_token; set whip_token in config.toml or --whip-token to protect ingest"
        );
    }

    let bind_addr = SocketAddr::new(cfg.bind, cfg.port);
    let listener = match tokio::net::TcpListener::bind(bind_addr).await {
        Ok(l) => l,
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            error!(
                "Port {} is already in use. Is another tiwi instance running? (Use --port <port> to pick another)",
                cfg.port
            );
            std::process::exit(1);
        }
        Err(e) => {
            error!(addr = %bind_addr, err = %e, "Failed to bind WHIP HTTP listener");
            std::process::exit(1);
        }
    };

    let metrics_writer = logs_dir.as_deref().and_then(|dir| {
        match relay::metrics::MetricsWriter::open(dir, &run_ts) {
            Ok(w) => Some(w),
            Err(e) => {
                warn!(dir = %dir.display(), err = %e, "Could not open metrics file");
                None
            }
        }
    });

    info!(
        whip = %format!("http://{}:{}/whip", cfg.bind, cfg.port),
        auth = cfg.whip_token.is_some(),
        "WHIP ingest endpoint"
    );
    info!(watch = %cfg.watch_url(), "Viewer URL");

    let (whip_tx, whip_rx) = mpsc::channel(32);
    let (offer_tx, offer_rx) = mpsc::channel(64);
    let (signal_tx, signal_rx) = mpsc::channel(32);
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    // Dedicated OS thread running a single-threaded tokio reactor for str0m media I/O.
    let media_cfg = cfg.clone();
    let media_shutdown = shutdown_rx.clone();
    let media_handle = std::thread::Builder::new()
        .name("tiwi-media".to_string())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("failed to build media runtime");
            rt.block_on(relay::run(
                media_cfg,
                metrics_writer,
                whip_rx,
                offer_rx,
                signal_tx,
                media_shutdown,
            ));
        })
        .expect("failed to spawn media thread");

    // Control-plane MQTT signaling task (exits cleanly once relay closes signal_tx).
    let signal_task = tokio::spawn(signal::run(
        cfg.mqtt_url.clone(),
        cfg.token.clone(),
        offer_tx,
        signal_rx,
    ));

    // Control-plane WHIP HTTP server.
    let app = whip::router(whip_tx, cfg.whip_token.clone());

    let mut http_shutdown = shutdown_rx.clone();
    let server = axum::serve(listener, app).with_graceful_shutdown(async move {
        let _ = http_shutdown.changed().await;
    });

    tokio::select! {
        res = server => {
            if let Err(e) = res {
                warn!(err = %e, "WHIP HTTP server error");
            }
        }
        _ = shutdown_signal() => {
            info!("Shutdown signal received");
        }
    }

    // Stop relay first so StreamStop is queued on signal_tx, then let signal_task flush & disconnect.
    let _ = shutdown_tx.send(true);
    let _ = media_handle.join();
    let _ = signal_task.await;
    info!("Stopped cleanly");
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut sig) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            sig.recv().await;
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
}
