//! Static SPA assets embedded into the binary at build time.
//!
//! Two routes serve HTML:
//!   * `GET /`              → `index.html` (fleet dashboard)
//!   * `GET /claws/:name`   → `claw.html` (chrome + iframe to the claw)
//!
//! Static assets (CSS/JS) live under `/static/*` and are served by
//! [`asset`] with a content-type guessed by `mime_guess`.

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{HeaderValue, Response, StatusCode, header};
use axum::response::IntoResponse;
use axum::{Json, Router, routing::get};
use rust_embed::RustEmbed;

use crate::api::AppState;

#[derive(RustEmbed)]
#[folder = "web/"]
struct WebAssets;

/// Mount the SPA routes onto the fleet router.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", get(index))
        .route("/claws/{name}", get(claw_chrome))
        .route("/configs", get(configs_page))
        .route("/static/{*path}", get(static_asset))
        .route("/api/config", get(public_config))
}

async fn index() -> Response<Body> {
    serve("index.html")
}

async fn claw_chrome(Path(_name): Path<String>) -> Response<Body> {
    // The chrome HTML is identical for every claw; the client picks up
    // the name from `window.location.pathname`.
    serve("claw.html")
}

async fn configs_page() -> Response<Body> {
    serve("configs.html")
}

async fn static_asset(Path(path): Path<String>) -> Response<Body> {
    serve(&format!("static/{path}"))
}

/// `GET /api/config` returns the small JSON payload the SPA needs to
/// construct per-claw URLs (so the iframe's `src` points at the right
/// `<name>.<claw_suffix>`).
async fn public_config(State(state): State<AppState>) -> impl IntoResponse {
    Json(serde_json::json!({
        "fleet_host": state.cfg.proxy.fleet_host.as_ref(),
        "claw_suffix": state.cfg.proxy.claw_suffix.as_ref(),
    }))
}

fn serve(path: &str) -> Response<Body> {
    match WebAssets::get(path) {
        Some(asset) => {
            let mime = mime_guess::from_path(path)
                .first_or_octet_stream()
                .to_string();
            Response::builder()
                .status(StatusCode::OK)
                .header(
                    header::CONTENT_TYPE,
                    HeaderValue::from_str(&mime).unwrap_or(HeaderValue::from_static("application/octet-stream")),
                )
                .body(Body::from(asset.data.into_owned()))
                .unwrap_or_else(|_| not_found())
        }
        None => not_found(),
    }
}

fn not_found() -> Response<Body> {
    Response::builder()
        .status(StatusCode::NOT_FOUND)
        .body(Body::from("not found"))
        .unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_assets_include_index_and_static() {
        assert!(WebAssets::get("index.html").is_some(), "index.html embedded");
        assert!(WebAssets::get("claw.html").is_some(), "claw.html embedded");
        assert!(WebAssets::get("configs.html").is_some(), "configs.html embedded");
        assert!(WebAssets::get("static/app.css").is_some(), "app.css embedded");
        assert!(WebAssets::get("static/app.js").is_some(), "app.js embedded");
        assert!(WebAssets::get("static/configs.js").is_some(), "configs.js embedded");
    }
}
