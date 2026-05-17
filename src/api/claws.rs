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
    pub name: String,
    pub status: Option<ClawStatus>,
    pub error: Option<String>,
}

pub async fn list(State(state): State<AppState>) -> impl IntoResponse {
    let names = state.claws.read().await.clone();
    let mut out = Vec::with_capacity(names.len());
    for name in names {
        let (status, error) = match state.driver.status(&name).await {
            Ok(s) => (Some(s), None),
            Err(e) => (None, Some(e.to_string())),
        };
        out.push(ClawListEntry { name, status, error });
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
