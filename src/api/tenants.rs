//! `POST /api/tenants` — full per-tenant provisioning flow.
//!
//! T6 implements the actual provisioning sequence (Authentik OAuth client,
//! secret store, MCP hub identity block, LiteLLM virtual key). This file
//! is the API endpoint stub until then.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;

use crate::api::AppState;

pub async fn create(State(_state): State<AppState>) -> impl IntoResponse {
    (
        StatusCode::NOT_IMPLEMENTED,
        Json(serde_json::json!({
            "error": "tenant provisioning not yet implemented",
            "tracked_in": "T6"
        })),
    )
}
