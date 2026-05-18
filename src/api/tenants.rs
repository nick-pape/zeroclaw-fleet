//! `POST /api/tenants` — full per-tenant provisioning flow.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;

use crate::api::AppState;
use crate::provision::{TenantRequest, provision};

pub async fn create(
    State(state): State<AppState>,
    Json(req): Json<TenantRequest>,
) -> impl IntoResponse {
    let Some(deps) = state.provision.as_ref() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({
                "error": "provisioning disabled — orchestrator missing bao/authentik/litellm bootstrap",
                "hint": "configure FLEET_BAO_TOKEN + the bao paths the orchestrator reads for litellm/authentik creds"
            })),
        )
            .into_response();
    };
    match provision(&req, deps).await {
        Ok(out) => (StatusCode::OK, Json(out)).into_response(),
        Err(e) => {
            tracing::warn!(error = %e, tenant = %req.name, "provisioning failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "error": format!("{e:#}"),
                    "tenant": req.name,
                    "note": "this call is idempotent — re-POSTing the same body resumes from the last successful step"
                })),
            )
                .into_response()
        }
    }
}
