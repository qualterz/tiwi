mod cli;
mod config;
mod relay;
mod signal;
mod whip;

use std::net::SocketAddr;

use clap::Parser;
use tokio::sync::{mpsc, watch};
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

use crate::cli::Cli;
use crate::config::Config;

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let default_filter = if cli.verbose {
        "tiwi=debug,str0m=info"
    } else {
        "tiwi=info"
    };
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_filter)),
        )
        .with_target(false)
        .init();

    let cfg = match Config::load(cli) {
        Ok(c) => c,
        Err(e) => {
            error!(err = %e, "Failed to load configuration");
            std::process::exit(1);
        }
    };

    if !cfg.bind.is_loopback() && cfg.whip_token.is_none() {
        warn!(
            bind = %cfg.bind,
            "WHIP endpoint is exposed on a non-loopback address without whip_token; set whip_token in config.toml or --whip-token to protect ingest"
        );
    }

    if let Some(path) = &cfg.config_path {
        info!(config = %path.display(), "Loaded configuration");
    }
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
                whip_rx,
                offer_rx,
                signal_tx,
                media_shutdown,
            ));
        })
        .expect("failed to spawn media thread");

    // Control-plane MQTT signaling task.
    let signal_task = tokio::spawn(signal::run(
        cfg.mqtt_url.clone(),
        cfg.token.clone(),
        offer_tx,
        signal_rx,
        shutdown_rx.clone(),
    ));

    // Control-plane WHIP HTTP server.
    let app = whip::router(whip_tx, cfg.whip_token.clone());
    let bind_addr = SocketAddr::new(cfg.bind, cfg.port);
    let listener = match tokio::net::TcpListener::bind(bind_addr).await {
        Ok(l) => l,
        Err(e) => {
            error!(addr = %bind_addr, err = %e, "Failed to bind WHIP HTTP listener");
            let _ = shutdown_tx.send(true);
            let _ = media_handle.join();
            std::process::exit(1);
        }
    };

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

    let _ = shutdown_tx.send(true);
    let _ = signal_task.await;
    let _ = media_handle.join();
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
