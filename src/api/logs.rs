//! `GET /api/claws/:name/logs?lines=N`. Tails docker logs.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::Deserialize;

use crate::api::AppState;
use crate::driver::Driver;

#[derive(Debug, Deserialize)]
pub struct LogsQuery {
    #[serde(default = "default_lines")]
    pub lines: usize,
}

fn default_lines() -> usize {
    200
}

pub async fn tail(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Query(q): Query<LogsQuery>,
) -> impl IntoResponse {
    let lines = q.lines.min(5000);
    match state.driver.logs(&name, lines).await {
        Ok(s) => (StatusCode::OK, s).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}
