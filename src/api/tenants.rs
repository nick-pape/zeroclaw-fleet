//! `POST /api/tenants` — full per-tenant provisioning flow.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::Deserialize;

use crate::api::AppState;
use crate::provision::{TenantRequest, deprovision, provision};
use crate::driver::Driver;

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

#[derive(Debug, Deserialize, Default)]
pub struct DeleteQuery {
    /// User must POST the tenant name in the request body's
    /// `confirm` field — guards against accidental clicks.
    pub confirm: Option<String>,
    /// If true, also remove the docker container + named volume. Default
    /// false so a partial delete (only external systems) is possible.
    #[serde(default = "default_true")]
    pub purge_container: bool,
    /// MCP scopes the tenant had — used to clean up bao JWT roles.
    /// Empty list is fine; jwt_role_delete absorbs 404s as warnings.
    #[serde(default)]
    pub mcp_scopes: Vec<String>,
    /// Defaults to `"services"` (matches provisioning default).
    pub secret_prefix: Option<String>,
}

fn default_true() -> bool { true }

pub async fn delete_tenant(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(body): Json<DeleteQuery>,
) -> impl IntoResponse {
    // Defense in depth: require the body's `confirm` field to match the
    // path param. The UI also gates the request behind a typed-name
    // modal — this is a backstop against scripted accidents.
    if body.confirm.as_deref() != Some(name.as_str()) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "missing or mismatched `confirm` field",
                "hint": format!("POST body must include {{\"confirm\": \"{name}\"}}"),
            })),
        )
            .into_response();
    }

    let Some(deps) = state.provision.as_ref() else {
        // Even without bao bootstrap, we can still tear down the container
        // and let the operator clean up external systems themselves.
        let mut warnings = vec!["provisioning bootstrap missing — only docker cleanup available".to_string()];
        if body.purge_container {
            if let Err(e) = state.driver.purge(&name, &format!("claw-data-{name}")).await {
                warnings.push(format!("driver purge: {e}"));
            }
        }
        return (StatusCode::PARTIAL_CONTENT, Json(serde_json::json!({
            "name": name,
            "steps_completed": ["container:purged"],
            "warnings": warnings,
        }))).into_response();
    };

    // 1. Container teardown if requested.
    let mut purge_warnings = Vec::new();
    if body.purge_container {
        if let Err(e) = state.driver.purge(&name, &format!("claw-data-{name}")).await {
            purge_warnings.push(format!("driver purge: {e}"));
        }
    }

    // 2. External system cleanup.
    let secret_prefix = body.secret_prefix.unwrap_or_else(|| "services".into());
    match deprovision(&name, &body.mcp_scopes, &secret_prefix, deps).await {
        Ok(mut out) => {
            out.warnings.extend(purge_warnings);
            if body.purge_container {
                out.steps_completed.insert(0, "container:purged".into());
            }
            (StatusCode::OK, Json(out)).into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "error": format!("{e:#}"),
                "tenant": name,
                "purge_warnings": purge_warnings,
            })),
        )
            .into_response(),
    }
}
