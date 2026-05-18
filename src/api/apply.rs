//! `POST /api/apply` (all claws) and `POST /api/claws/{name}/up` (one).
//!
//! Both:
//!   1. Re-read `base.toml` + the claw's overlay from disk.
//!   2. Render the per-claw `config.toml` into
//!      `<state_dir>/<name>/config/config.toml` (atomic write).
//!   3. Make sure a per-claw bearer exists at `<state_dir>/<name>/bearer.txt`
//!      (mode 0400 on Unix); generate one if missing.
//!   4. Build a `ClawSpec` and hand it to the driver's `ensure()` so the
//!      container is created (or recreated if drift detected) and running.
//!
//! Idempotent. Side-effect-free if the rendered config equals what's already
//! on disk and the container's spec hasn't drifted.

use anyhow::{Context, Result};
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use crate::api::AppState;
use crate::driver::{ClawSpec, Driver, EnsureOutcome, LogSettings};
use crate::manifest::ClawOverlay;
use crate::render::{self, Injections};

#[derive(Debug, Serialize)]
pub struct ApplyResult {
    pub name: String,
    pub rendered: bool,
    pub container: String,
    pub steps: Vec<String>,
    pub warnings: Vec<String>,
}

pub async fn apply_all(State(state): State<AppState>) -> impl IntoResponse {
    let names = state.claws.read().await.clone();
    let mut results = Vec::with_capacity(names.len());
    for name in names {
        match apply_one_inner(&state, &name).await {
            Ok(r) => results.push(r),
            Err(e) => results.push(ApplyResult {
                name: name.clone(),
                rendered: false,
                container: format!("error: {e:#}"),
                steps: vec![],
                warnings: vec![e.to_string()],
            }),
        }
    }
    Json(results)
}

pub async fn up_one(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    match apply_one_inner(&state, &name).await {
        Ok(r) => (StatusCode::OK, Json(r)).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")).into_response(),
    }
}

async fn apply_one_inner(state: &AppState, name: &str) -> Result<ApplyResult> {
    let mut steps = Vec::new();
    let mut warnings = Vec::new();

    let overlay = {
        let r = state.overlays.read().await;
        r.get(name)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("no overlay for {name}"))?
    };

    // 1. Render config.
    let base_path = state.cfg.fleet_dir.join("base.toml");
    let base = std::fs::read_to_string(&base_path)
        .with_context(|| format!("read {}", base_path.display()))?;

    let bearer = ensure_bearer(state, name)?;
    steps.push(format!("bearer:{}", if bearer.is_new { "generated" } else { "existing" }));

    let inj = Injections {
        orchestrator_bearer: bearer.value.clone(),
        mcp_bearer_placeholder: state.cfg.mcp_bearer_placeholder.clone(),
        mcp_server_url: state.cfg.mcp_server_url.clone(),
        mcp_server_name: state.cfg.mcp_server_name.clone(),
    };
    let rendered = render::render(&base, &overlay, &inj)?;
    let cfg_path = state.cfg.state_dir.join(name).join("config").join("config.toml");
    write_atomic(&cfg_path, &rendered)?;
    steps.push(format!("config:wrote:{}", cfg_path.display()));

    // 2. Build a ClawSpec.
    let spec = build_spec(state, &overlay, &cfg_path)?;
    if spec.env_file.is_none() {
        warnings.push(format!(
            "no env_file at /etc/zeroclaw-fleet/claws/{name}.env — claw will start without OPENAI_API_KEY; LiteLLM calls will fail"
        ));
    }

    // 3. Ensure container.
    let outcome = state.driver.ensure(&spec).await?;
    let container = match outcome {
        EnsureOutcome::Created => "created+started".into(),
        EnsureOutcome::Recreated => "recreated+started".into(),
        EnsureOutcome::Unchanged => "unchanged".into(),
    };
    steps.push(format!("container:{container}"));

    Ok(ApplyResult { name: name.to_string(), rendered: true, container, steps, warnings })
}

struct Bearer { value: String, is_new: bool }

fn ensure_bearer(state: &AppState, name: &str) -> Result<Bearer> {
    let p = state.cfg.state_dir.join(name).join("bearer.txt");
    if let Ok(existing) = std::fs::read_to_string(&p) {
        let v = existing.trim().to_string();
        if !v.is_empty() {
            return Ok(Bearer { value: v, is_new: false });
        }
    }
    let new = format!("zc_{}", uuid::Uuid::new_v4().simple());
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent).context("create bearer dir")?;
    }
    std::fs::write(&p, &new).context("write bearer")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o400));
    }
    Ok(Bearer { value: new, is_new: true })
}

fn write_atomic(path: &PathBuf, content: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).context("create config dir")?;
    }
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, content).context("write tmp")?;
    std::fs::rename(&tmp, path).context("rename into place")?;
    Ok(())
}

fn build_spec(state: &AppState, overlay: &ClawOverlay, cfg_path: &PathBuf) -> Result<ClawSpec> {
    let name = &overlay.fleet.name;

    // Re-read fleet.yaml every apply so defaults track what's on disk
    // (operator may have bumped `defaults.image` since startup).
    let fleet_yaml = state.cfg.fleet_dir.join("fleet.yaml");
    let manifest_defaults = crate::manifest::FleetManifest::from_path(&fleet_yaml)
        .map(|m| m.defaults)
        .unwrap_or_default();

    let image = overlay
        .fleet
        .image
        .clone()
        .or(manifest_defaults.image)
        .or_else(|| state.cfg.default_image.clone())
        .ok_or_else(|| anyhow::anyhow!("no image — set fleet.defaults.image or overlay [_fleet] image"))?;

    let mem_limit_bytes = manifest_defaults
        .mem_limit
        .as_deref()
        .and_then(parse_mem_limit)
        .or(state.cfg.default_mem_limit_bytes);
    let cpu_limit = manifest_defaults.cpu_limit.or(state.cfg.default_cpu_limit);
    let restart = manifest_defaults
        .restart
        .clone()
        .or_else(|| state.cfg.default_restart.clone())
        .unwrap_or_else(|| "unless-stopped".into());
    let log_max_size = manifest_defaults
        .log_max_size
        .clone()
        .or_else(|| state.cfg.default_log_max_size.clone())
        .unwrap_or_else(|| "20m".into());
    let log_max_file = manifest_defaults
        .log_max_file
        .or(state.cfg.default_log_max_file)
        .unwrap_or(3);

    let mut env: BTreeMap<String, String> = BTreeMap::new();
    env.insert("ZEROCLAW_ALLOW_PUBLIC_BIND".into(), "true".into());
    env.insert("ZEROCLAW_GATEWAY_PORT".into(), state.cfg.proxy.claw_port.to_string());
    env.insert("ZEROCLAW_CONFIG_DIR".into(), "/zeroclaw-data/config".into());

    // Per-claw env file discovery. Lives at /etc/zeroclaw-fleet-secrets/
    // on the host (separate from /etc/zeroclaw-fleet/ to avoid colliding
    // with the manifest's `claws/` subdir bind). Mounted at the same path
    // inside the orchestrator container so existence check + the path
    // passed to docker resolve to the same host file.
    let candidates = [
        PathBuf::from(format!("/etc/zeroclaw-fleet-secrets/{name}.env")),
    ];
    let env_file = candidates.into_iter().find(|p| p.exists());

    let config_dir_host = cfg_path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("config path has no parent"))?
        .to_path_buf();

    Ok(ClawSpec {
        name: name.clone(),
        image,
        mem_limit_bytes,
        cpu_limit,
        restart,
        env,
        config_dir: config_dir_host,
        env_file,
        data_volume: format!("claw-data-{name}"),
        network: state.cfg.claws_network.clone(),
        log: LogSettings { max_size: log_max_size, max_file: log_max_file },
    })
}

/// Parse docker-style memory limits: `"1g"`, `"512m"`, `"1024"` (bytes).
fn parse_mem_limit(s: &str) -> Option<i64> {
    let s = s.trim();
    if s.is_empty() { return None; }
    let (num, mult): (&str, i64) = if let Some(n) = s.strip_suffix(|c: char| c.eq_ignore_ascii_case(&'g')) {
        (n, 1024 * 1024 * 1024)
    } else if let Some(n) = s.strip_suffix(|c: char| c.eq_ignore_ascii_case(&'m')) {
        (n, 1024 * 1024)
    } else if let Some(n) = s.strip_suffix(|c: char| c.eq_ignore_ascii_case(&'k')) {
        (n, 1024)
    } else {
        (s, 1)
    };
    num.trim().parse::<i64>().ok().map(|n| n.saturating_mul(mult))
}

// Lifetime adapter so the BTreeMap import is reachable even if a future
// edit drops the only use.
#[allow(dead_code)]
fn _types() -> Arc<()> { Arc::new(()) }
