//! `/api/claws[/:name][/{start,stop,restart}]`.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::Serialize;

use crate::api::AppState;
use crate::driver::{ClawStatus, Driver};

#[derive(Debug, Serialize)]
pub struct ClawListEntry {
    /// Stable kebab-case identifier (e.g. `"grocery"`). Used for routing
    /// and as the docker container name.
    pub name: String,
    /// Friendly name from `[branding] display_name` (e.g. `"H-E-Buddy"`).
    /// Falls back to `name` if no override is set.
    pub display_name: String,
    pub status: Option<ClawStatus>,
    pub error: Option<String>,
}

pub async fn list(State(state): State<AppState>) -> impl IntoResponse {
    let names = state.claws.read().await.clone();
    let overlays = state.overlays.read().await;
    let mut out = Vec::with_capacity(names.len());
    for name in names {
        let display_name = overlays
            .get(&name)
            .map(|o| o.display_name())
            .unwrap_or_else(|| name.clone());
        let (status, error) = match state.driver.status(&name).await {
            Ok(s) => (Some(s), None),
            Err(e) => (None, Some(e.to_string())),
        };
        out.push(ClawListEntry { name, display_name, status, error });
    }
    Json(out)
}

pub async fn detail(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    match state.driver.status(&name).await {
        Ok(s) => Json(s).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

pub async fn start(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    match state.driver.start(&name).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

pub async fn stop(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    match state.driver.stop(&name).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

pub async fn restart(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    match state.driver.restart(&name).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}
