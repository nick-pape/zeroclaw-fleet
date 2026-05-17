//! `/api/cost` (fleet rollup) and `/api/claws/:name/cost` (single claw).

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;

use crate::api::AppState;
use crate::cost_poller::{fleet_rollup, poll_once};

pub async fn fleet(State(state): State<AppState>) -> impl IntoResponse {
    let r = fleet_rollup(&state.cost_cache).await;
    Json(r)
}

pub async fn per_claw(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    // Read-through to cache for snappy responses, falling back to a fresh
    // poll if the cache hasn't been populated yet.
    {
        let r = state.cost_cache.read().await;
        if let Some(snap) = r.get(&name) {
            return Json(snap.clone()).into_response();
        }
    }
    match poll_once(&state.http, &state.cfg.proxy.upstream_base(&name)).await {
        Ok(summary) => Json(summary).into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, e.to_string()).into_response(),
    }
}
