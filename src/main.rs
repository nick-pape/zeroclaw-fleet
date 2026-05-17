use anyhow::{Context, Result};
use axum::Router;
use axum::routing::get;
use clap::{Parser, Subcommand};
use std::net::SocketAddr;
use std::path::PathBuf;
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
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Run the orchestrator HTTP server (default).
    Serve {
        /// HTTP bind address
        #[arg(long, env = "FLEET_BIND", default_value = "0.0.0.0:8080")]
        bind: SocketAddr,

        /// Path to the fleet manifest directory.
        #[arg(long, env = "FLEET_DIR", default_value = "/etc/zeroclaw-fleet")]
        fleet_dir: PathBuf,

        /// Path to per-claw state.
        #[arg(long, env = "FLEET_STATE_DIR", default_value = "/var/lib/zeroclaw-fleet")]
        state_dir: PathBuf,
    },

    /// Render a per-claw `config.toml` from a base + overlay and print to
    /// stdout. Useful for local parity testing against a known-good config.
    Render {
        /// Path to `base.toml`.
        #[arg(long)]
        base: PathBuf,

        /// Path to the claw overlay TOML.
        #[arg(long)]
        overlay: PathBuf,

        /// Bearer the orchestrator would use to scrape this claw's API.
        /// Hashed before injection. Defaults to a deterministic test value.
        #[arg(long, default_value = "zc_render_demo_bearer")]
        bearer: String,

        /// Placeholder substituted into `[[mcp.servers]].headers.Authorization`.
        #[arg(long, default_value = "__MCP_BEARER__")]
        mcp_bearer_placeholder: String,

        /// MCP hub URL.
        #[arg(long, default_value = "https://hub.example.com/mcp")]
        mcp_server_url: String,

        /// Namespace name (becomes the MCP tool prefix in ZeroClaw —
        /// e.g. `papehouse` → `papehouse__heb_*`).
        #[arg(long, default_value = "hub")]
        mcp_server_name: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .init();

    let cli = Cli::parse();
    match cli.command.unwrap_or(default_serve()) {
        Command::Serve { bind, fleet_dir, state_dir } => serve(bind, fleet_dir, state_dir).await,
        Command::Render { base, overlay, bearer, mcp_bearer_placeholder, mcp_server_url, mcp_server_name } => {
            render_to_stdout(base, overlay, bearer, mcp_bearer_placeholder, mcp_server_url, mcp_server_name)
        }
    }
}

fn default_serve() -> Command {
    Command::Serve {
        bind: "0.0.0.0:8080".parse().expect("default bind"),
        fleet_dir: PathBuf::from("/etc/zeroclaw-fleet"),
        state_dir: PathBuf::from("/var/lib/zeroclaw-fleet"),
    }
}

async fn serve(bind: SocketAddr, fleet_dir: PathBuf, state_dir: PathBuf) -> Result<()> {
    info!(
        bind = %bind,
        fleet_dir = %fleet_dir.display(),
        state_dir = %state_dir.display(),
        "starting zeroclaw-fleet"
    );

    let app = Router::new().route("/healthz", get(healthz));

    let listener = TcpListener::bind(bind).await?;
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    Ok(())
}

fn render_to_stdout(
    base: PathBuf,
    overlay: PathBuf,
    bearer: String,
    mcp_bearer_placeholder: String,
    mcp_server_url: String,
    mcp_server_name: String,
) -> Result<()> {
    let base_src = std::fs::read_to_string(&base)
        .with_context(|| format!("read base.toml {}", base.display()))?;
    let claw = manifest::ClawOverlay::from_path(&overlay)?;
    let inj = render::Injections {
        orchestrator_bearer: bearer,
        mcp_bearer_placeholder,
        mcp_server_url,
        mcp_server_name,
    };
    let rendered = render::render(&base_src, &claw, &inj)?;
    print!("{rendered}");
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
