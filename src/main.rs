use anyhow::Result;
use axum::Router;
use axum::routing::get;
use clap::Parser;
use std::net::SocketAddr;
use tokio::net::TcpListener;
use tokio::signal;
use tracing::info;
use tracing_subscriber::EnvFilter;

mod api;
mod config;
mod cost_poller;
mod driver;
mod manifest;
mod provision;
mod proxy;
mod render;
mod web;

#[derive(Parser, Debug)]
#[command(version, about = "Orchestrator for running multiple isolated ZeroClaw instances")]
struct Cli {
    /// HTTP bind address
    #[arg(long, env = "FLEET_BIND", default_value = "0.0.0.0:8080")]
    bind: SocketAddr,

    /// Path to the fleet manifest directory (contains fleet.yaml, base.toml, claws/, prompts/)
    #[arg(long, env = "FLEET_DIR", default_value = "/etc/zeroclaw-fleet")]
    fleet_dir: std::path::PathBuf,

    /// Path to per-claw state (paired_tokens, rendered configs, provisioning state)
    #[arg(long, env = "FLEET_STATE_DIR", default_value = "/var/lib/zeroclaw-fleet")]
    state_dir: std::path::PathBuf,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .init();

    let cli = Cli::parse();
    info!(bind = %cli.bind, fleet_dir = %cli.fleet_dir.display(), state_dir = %cli.state_dir.display(), "starting zeroclaw-fleet");

    let app = Router::new().route("/healthz", get(healthz));

    let listener = TcpListener::bind(cli.bind).await?;
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    Ok(())
}

async fn healthz() -> &'static str {
    "ok"
}

async fn shutdown_signal() {
    let ctrl_c = async { signal::ctrl_c().await.ok(); };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
    info!("shutdown signal received");
}
