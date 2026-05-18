use anyhow::{Context, Result};
use axum::Router;
use axum::body::Body;
use axum::extract::Request;
use axum::response::IntoResponse;
use clap::{Parser, Subcommand};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::signal;
use tokio::sync::RwLock;
use tower::Service;
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

use config::OrchestratorConfig;
use provision::{ProvisionDeps, authentik::AuthentikClient, bao::BaoClient, litellm::LiteLlmClient};
use proxy::ProxyConfig;

#[derive(Parser, Debug)]
#[command(version, about = "Orchestrator for running multiple isolated ZeroClaw instances")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Run the orchestrator HTTP server (default).
    Serve(ServeArgs),

    /// Render a per-claw `config.toml` from a base + overlay and print to
    /// stdout. Useful for local parity testing against a known-good config.
    Render {
        #[arg(long)]
        base: PathBuf,
        #[arg(long)]
        overlay: PathBuf,
        #[arg(long, default_value = "zc_render_demo_bearer")]
        bearer: String,
        #[arg(long, default_value = "__MCP_BEARER__")]
        mcp_bearer_placeholder: String,
        #[arg(long, default_value = "https://hub.example.com/mcp")]
        mcp_server_url: String,
        #[arg(long, default_value = "hub")]
        mcp_server_name: String,
    },
}

#[derive(Parser, Debug, Clone)]
struct ServeArgs {
    /// HTTP bind address.
    #[arg(long, env = "FLEET_BIND", default_value = "0.0.0.0:8080")]
    bind: SocketAddr,

    /// Path to the fleet manifest directory.
    #[arg(long, env = "FLEET_DIR", default_value = "/etc/zeroclaw-fleet")]
    fleet_dir: PathBuf,

    /// Path to per-claw state.
    #[arg(long, env = "FLEET_STATE_DIR", default_value = "/var/lib/zeroclaw-fleet")]
    state_dir: PathBuf,

    /// Host header that identifies the fleet UI (e.g. `claws.example.com`).
    #[arg(long, env = "FLEET_HOST")]
    fleet_host: String,

    /// Host suffix that identifies a per-claw subdomain (e.g.
    /// `claw.example.com` matches `<name>.claw.example.com`).
    #[arg(long, env = "FLEET_CLAW_SUFFIX")]
    claw_suffix: String,

    /// MCP hub URL the renderer wires into every claw.
    #[arg(long, env = "FLEET_MCP_SERVER_URL")]
    mcp_server_url: String,

    /// Namespace name (becomes ZeroClaw's MCP tool prefix).
    #[arg(long, env = "FLEET_MCP_SERVER_NAME", default_value = "hub")]
    mcp_server_name: String,

    /// Placeholder substituted into the rendered MCP Authorization header.
    #[arg(long, env = "FLEET_MCP_BEARER_PLACEHOLDER", default_value = "__MCP_BEARER__")]
    mcp_bearer_placeholder: String,

    /// Upstream port on every claw container (always 42617 for ZeroClaw).
    #[arg(long, env = "FLEET_CLAW_PORT", default_value_t = 42617)]
    claw_port: u16,

    /// Cost poller cadence in seconds.
    #[arg(long, env = "FLEET_COST_POLL_INTERVAL", default_value_t = 30)]
    cost_poll_interval_secs: u64,

    // --- provisioning bootstrap (T6) ---

    /// Bao HTTP URL (e.g. `https://bao.example.com`). When set together
    /// with `--bao-token`, the orchestrator bootstraps the provisioning
    /// deps from bao at startup. If either is missing, provisioning is
    /// disabled and `/api/tenants` returns 503.
    #[arg(long, env = "FLEET_BAO_URL")]
    bao_url: Option<String>,

    /// Bao service token. Mount as a docker secret in production.
    #[arg(long, env = "FLEET_BAO_TOKEN", hide_env_values = true)]
    bao_token: Option<String>,

    /// KV-v2 mount the orchestrator's bao token can read/write under.
    #[arg(long, env = "FLEET_BAO_MOUNT", default_value = "secret")]
    bao_mount: String,

    /// Bao path the orchestrator reads to find the LiteLLM master key.
    /// Field name is always `api_key`.
    #[arg(long, env = "FLEET_LITELLM_KEY_PATH", default_value = "services/litellm/master")]
    bao_litellm_key_path: String,

    /// LiteLLM admin URL.
    #[arg(long, env = "FLEET_LITELLM_URL")]
    litellm_url: Option<String>,

    /// Bao path the orchestrator reads to find the Authentik admin API token.
    /// Field name is always `api_token`.
    #[arg(long, env = "FLEET_AUTHENTIK_TOKEN_PATH", default_value = "services/authentik/admin_token")]
    bao_authentik_token_path: String,

    /// Authentik base URL.
    #[arg(long, env = "FLEET_AUTHENTIK_URL")]
    authentik_url: Option<String>,

    /// Existing Authentik OAuth2 provider name to clone for signing_key
    /// + authorization_flow.
    #[arg(long, env = "FLEET_AUTHENTIK_TEMPLATE", default_value = "mcp-interactive")]
    authentik_template: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .init();

    let cli = Cli::parse();
    match cli.command {
        Some(Command::Serve(args)) => serve(args).await,
        Some(Command::Render { base, overlay, bearer, mcp_bearer_placeholder, mcp_server_url, mcp_server_name }) => {
            render_to_stdout(base, overlay, bearer, mcp_bearer_placeholder, mcp_server_url, mcp_server_name)
        }
        None => {
            eprintln!("usage: zeroclaw-fleet <serve|render> ...");
            eprintln!("       try --help for details");
            std::process::exit(2);
        }
    }
}

async fn serve(args: ServeArgs) -> Result<()> {
    let proxy_cfg = ProxyConfig {
        fleet_host: Arc::from(args.fleet_host.as_str()),
        claw_suffix: Arc::from(args.claw_suffix.as_str()),
        claw_port: args.claw_port,
    };

    let cfg = OrchestratorConfig {
        bind: args.bind,
        fleet_dir: args.fleet_dir.clone(),
        state_dir: args.state_dir.clone(),
        proxy: proxy_cfg,
        mcp_server_url: args.mcp_server_url.clone(),
        mcp_server_name: args.mcp_server_name.clone(),
        mcp_bearer_placeholder: args.mcp_bearer_placeholder.clone(),
        cost_poll_interval_secs: args.cost_poll_interval_secs,
    }
    .into_shared();

    info!(
        bind = %cfg.bind,
        fleet_dir = %cfg.fleet_dir.display(),
        state_dir = %cfg.state_dir.display(),
        fleet_host = %cfg.proxy.fleet_host,
        claw_suffix = %cfg.proxy.claw_suffix,
        "starting zeroclaw-fleet"
    );

    let driver = Arc::new(
        driver::docker::DockerDriver::connect_local()
            .context("connect to docker daemon at startup")?,
    );

    let claw_list = try_load_claw_list(&cfg.fleet_dir).unwrap_or_else(|e| {
        tracing::warn!(error = %e, "no fleet.yaml or unreadable; starting with empty fleet");
        Vec::new()
    });
    info!(count = claw_list.len(), "loaded claw list from fleet.yaml");
    let overlay_map = load_overlays(&cfg.fleet_dir, &claw_list);
    info!(count = overlay_map.len(), "loaded claw overlays");
    let claws = Arc::new(RwLock::new(claw_list));
    let overlays = Arc::new(RwLock::new(overlay_map));

    let cost_cache = cost_poller::new_cache();
    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .context("build reqwest client")?;

    cost_poller::spawn(cfg.clone(), cost_cache.clone(), http.clone(), claws.clone());

    let provision = bootstrap_provision(&args, &http, &cfg.state_dir).await;
    match provision.as_ref() {
        Some(_) => info!("provisioning bootstrap OK — /api/tenants enabled"),
        None => tracing::warn!("provisioning bootstrap incomplete — /api/tenants will return 503"),
    }

    let state = api::AppState {
        cfg: cfg.clone(),
        driver,
        cost_cache,
        claws,
        overlays,
        http: http.clone(),
        provision,
    };

    let fleet_router = api::router(state);
    let app = build_top_level_router(cfg.clone(), http, fleet_router);

    let listener = TcpListener::bind(cfg.bind).await?;
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    Ok(())
}

fn try_load_claw_list(fleet_dir: &PathBuf) -> Result<Vec<String>> {
    let path = fleet_dir.join("fleet.yaml");
    let m = manifest::FleetManifest::from_path(&path)?;
    Ok(m.claws)
}

/// Load per-claw overlays from `<fleet_dir>/claws/<name>.toml`. Skips
/// (with a warning) any that fail to parse — degrades the UI to using
/// the kebab name as display_name, but keeps the rest of the orchestrator
/// running.
fn load_overlays(fleet_dir: &PathBuf, names: &[String]) -> std::collections::HashMap<String, manifest::ClawOverlay> {
    let mut out = std::collections::HashMap::with_capacity(names.len());
    for name in names {
        let p = fleet_dir.join("claws").join(format!("{name}.toml"));
        match manifest::ClawOverlay::from_path(&p) {
            Ok(o) => { out.insert(name.clone(), o); }
            Err(e) => tracing::warn!(claw = %name, error = %e, "failed to load overlay {}", p.display()),
        }
    }
    out
}

/// Try to bootstrap the provisioning clients from bao. Returns None if
/// the required env vars / bao secrets aren't present — the orchestrator
/// keeps running with `/api/tenants` disabled.
async fn bootstrap_provision(
    args: &ServeArgs,
    http: &reqwest::Client,
    state_dir: &PathBuf,
) -> Option<Arc<ProvisionDeps>> {
    let bao_url = args.bao_url.as_ref()?;
    let bao_token = args.bao_token.as_ref()?;
    let litellm_url = args.litellm_url.as_ref()?;
    let authentik_url = args.authentik_url.as_ref()?;

    let bao = BaoClient::new(http.clone(), bao_url, bao_token).with_mount(&args.bao_mount);

    let litellm_master = match bao.kv_get_field(&args.bao_litellm_key_path, "api_key").await {
        Ok(k) => k,
        Err(e) => {
            tracing::warn!(error = %e, "bao read of LiteLLM master key failed");
            return None;
        }
    };
    let authentik_token = match bao.kv_get_field(&args.bao_authentik_token_path, "api_token").await {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(error = %e, "bao read of Authentik admin token failed");
            return None;
        }
    };

    Some(Arc::new(ProvisionDeps {
        bao,
        litellm: LiteLlmClient::new(http.clone(), litellm_url, litellm_master),
        authentik: AuthentikClient::new(http.clone(), authentik_url, authentik_token),
        authentik_template_name: Arc::from(args.authentik_template.as_str()),
        state_dir: state_dir.clone(),
    }))
}

/// Wrap the fleet router with a top-level host-based dispatcher.
/// Requests addressed to a per-claw subdomain proxy to the matching
/// upstream; everything else flows into `fleet_router`.
fn build_top_level_router(
    cfg: Arc<OrchestratorConfig>,
    http: reqwest::Client,
    fleet_router: Router,
) -> Router {
    Router::new().fallback(move |req: Request<Body>| {
        let cfg = cfg.clone();
        let http = http.clone();
        let mut inner = fleet_router.clone();
        async move {
            match proxy::maybe_proxy(&cfg.proxy, &http, req).await {
                Ok(resp) => resp,
                Err(req) => {
                    // Fleet UI host — hand off to the inner router.
                    match inner.call(req).await {
                        Ok(resp) => resp.into_response(),
                        Err(e) => (
                            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                            format!("router error: {e}"),
                        )
                            .into_response(),
                    }
                }
            }
        }
    })
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
