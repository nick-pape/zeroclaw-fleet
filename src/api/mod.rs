//! REST API surface for the orchestrator.
//!
//! All endpoints live behind the operator's edge proxy (Caddy +
//! forward_auth in the reference deployment). The orchestrator trusts
//! whatever identity the edge supplies via `X-Authentik-Username` /
//! `X-Forwarded-User`.

use std::sync::Arc;

use axum::Router;
use axum::routing::{get, post};
use tokio::sync::RwLock;

use crate::config::OrchestratorConfig;
use crate::cost_poller::CostCache;
use crate::driver::docker::DockerDriver;

pub mod claws;
pub mod cost;
pub mod logs;
pub mod tenants;

/// Shared application state for axum handlers. Cheap to clone — every
/// field is an `Arc`.
#[derive(Clone)]
pub struct AppState {
    pub cfg: Arc<OrchestratorConfig>,
    pub driver: Arc<DockerDriver>,
    pub cost_cache: CostCache,
    /// Active claw list (mirror of `fleet.yaml` `claws:`). Mutated by the
    /// manifest reload path; read by every handler.
    pub claws: Arc<RwLock<Vec<String>>>,
    pub http: reqwest::Client,
}

/// Build the fleet UI / API router. The HTTP+WS reverse proxy is wired
/// at the very top in `main.rs` as a fallback that defers to this
/// router only when the Host header is the fleet UI's.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/api/claws", get(claws::list))
        .route("/api/claws/:name", get(claws::detail))
        .route("/api/claws/:name/start", post(claws::start))
        .route("/api/claws/:name/stop", post(claws::stop))
        .route("/api/claws/:name/restart", post(claws::restart))
        .route("/api/claws/:name/logs", get(logs::tail))
        .route("/api/claws/:name/cost", get(cost::per_claw))
        .route("/api/cost", get(cost::fleet))
        .route("/api/tenants", post(tenants::create))
        .with_state(state)
}

async fn healthz() -> &'static str {
    "ok"
}
