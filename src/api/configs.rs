//! Read-only viewer for the on-disk manifest (base.toml + per-claw overlays).
//!
//! Editing happens in git — no write path through the API. The UI surfaces
//! a "View on git" link via the `git_url` field on each listing entry.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::Serialize;

use crate::api::AppState;

#[derive(Debug, Serialize)]
pub struct ConfigEntry {
    /// Stable URL slug used by the API (`base` or `claws/<name>`).
    pub key: String,
    /// On-disk path relative to the fleet manifest dir.
    pub path: String,
    /// Human label for the UI.
    pub label: String,
    /// File size in bytes (rough heuristic for "is this thing huge?").
    pub size_bytes: u64,
    /// Optional link into the git repo at the file's blob URL.
    pub git_url: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ConfigContent {
    pub key: String,
    pub path: String,
    pub content: String,
    pub git_url: Option<String>,
}

/// `GET /api/configs` — list every config file the viewer can serve.
pub async fn list(State(state): State<AppState>) -> impl IntoResponse {
    let fleet_dir = &state.cfg.fleet_dir;
    let claws = state.claws.read().await.clone();
    let mut out = Vec::with_capacity(1 + claws.len());

    let base = fleet_dir.join("base.toml");
    if let Ok(meta) = std::fs::metadata(&base) {
        out.push(ConfigEntry {
            key: "base".into(),
            path: "base.toml".into(),
            label: "base.toml (fleet-wide)".into(),
            size_bytes: meta.len(),
            git_url: git_url_for(state.cfg.repo_blob_base.as_deref(), "base.toml"),
        });
    }

    let fleet_yaml = fleet_dir.join("fleet.yaml");
    if let Ok(meta) = std::fs::metadata(&fleet_yaml) {
        out.push(ConfigEntry {
            key: "fleet".into(),
            path: "fleet.yaml".into(),
            label: "fleet.yaml (manifest)".into(),
            size_bytes: meta.len(),
            git_url: git_url_for(state.cfg.repo_blob_base.as_deref(), "fleet.yaml"),
        });
    }

    for claw in claws {
        let rel = format!("claws/{claw}.toml");
        let p = fleet_dir.join(&rel);
        if let Ok(meta) = std::fs::metadata(&p) {
            out.push(ConfigEntry {
                key: format!("claws/{claw}"),
                path: rel.clone(),
                label: format!("claws/{claw}.toml (overlay)"),
                size_bytes: meta.len(),
                git_url: git_url_for(state.cfg.repo_blob_base.as_deref(), &rel),
            });
        }
    }

    Json(out)
}

/// `GET /api/configs/base` — returns `base.toml`.
pub async fn base(State(state): State<AppState>) -> impl IntoResponse {
    serve(&state, "base", "base.toml")
}

/// `GET /api/configs/fleet` — returns `fleet.yaml`.
pub async fn fleet(State(state): State<AppState>) -> impl IntoResponse {
    serve(&state, "fleet", "fleet.yaml")
}

/// `GET /api/configs/claws/{name}` — returns the named claw's overlay.
pub async fn claw_overlay(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    // Guard against ../ traversal — claw names are always single
    // kebab-case labels.
    if !name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-') {
        return (StatusCode::BAD_REQUEST, "invalid claw name").into_response();
    }
    let key = format!("claws/{name}");
    let path = format!("claws/{name}.toml");
    serve(&state, &key, &path)
}

fn serve(state: &AppState, key: &str, rel_path: &str) -> axum::response::Response {
    let full = state.cfg.fleet_dir.join(rel_path);
    let content = match std::fs::read_to_string(&full) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return (StatusCode::NOT_FOUND, format!("no config at {rel_path}")).into_response();
        }
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("read failed: {e}")).into_response();
        }
    };
    Json(ConfigContent {
        key: key.into(),
        path: rel_path.into(),
        content,
        git_url: git_url_for(state.cfg.repo_blob_base.as_deref(), rel_path),
    })
    .into_response()
}

fn git_url_for(base: Option<&str>, rel_path: &str) -> Option<String> {
    let base = base?;
    let trimmed = base.trim_end_matches('/');
    Some(format!("{trimmed}/{rel_path}"))
}
