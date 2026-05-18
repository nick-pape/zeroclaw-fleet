//! Periodic background task that mints a fresh client_credentials JWT for
//! each claw from Authentik, rewrites the claw's rendered `config.toml`
//! to substitute the MCP `Authorization: Bearer __PAPEHOUSE_TOKEN__`
//! placeholder, and restarts the claw container so it picks up the new
//! header.
//!
//! Replaces the per-CT `render-config.sh` cron that lived on grocery
//! (CT 127) and alfred (CT 129) before the fleet existed.

use anyhow::{Context, Result, anyhow};
use serde::Deserialize;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tokio::time::interval;
use tracing::{info, warn};

use crate::config::OrchestratorConfig;
use crate::driver::Driver;
use crate::driver::docker::DockerDriver;
use crate::provision::bao::BaoClient;

/// Spawn the rotation task. Runs until process exit.
pub fn spawn(
    cfg: Arc<OrchestratorConfig>,
    driver: Arc<DockerDriver>,
    claws: Arc<RwLock<Vec<String>>>,
    http: reqwest::Client,
    bao: Option<BaoClient>,
    authentik_token_url: String,
    refresh_window_secs: u64,
    poll_interval_secs: u64,
) {
    let Some(bao) = bao else {
        warn!("bearer rotation disabled — no bao client (provisioning bootstrap missing)");
        return;
    };

    info!(
        poll_interval_secs,
        refresh_window_secs,
        token_url = %authentik_token_url,
        "starting MCP bearer rotation task"
    );

    tokio::spawn(async move {
        let mut tick = interval(Duration::from_secs(poll_interval_secs.max(60)));
        loop {
            tick.tick().await;
            let names = claws.read().await.clone();
            for name in names {
                if let Err(e) = rotate_one(
                    &name,
                    &cfg,
                    &driver,
                    &http,
                    &bao,
                    &authentik_token_url,
                    refresh_window_secs,
                )
                .await
                {
                    warn!(claw = %name, error = %e, "bearer rotation failed");
                }
            }
        }
    });
}

async fn rotate_one(
    name: &str,
    cfg: &OrchestratorConfig,
    driver: &DockerDriver,
    http: &reqwest::Client,
    bao: &BaoClient,
    authentik_token_url: &str,
    refresh_window_secs: u64,
) -> Result<()> {
    let cfg_path = cfg.state_dir.join(name).join("config").join("config.toml");
    let content = std::fs::read_to_string(&cfg_path)
        .with_context(|| format!("read {}", cfg_path.display()))?;

    // Pull the current `Authorization: Bearer <jwt>` value out of the
    // rendered config so we can check its expiry. The format the renderer
    // emits is one TOML row per [[mcp.servers]] block; we scan for the
    // first Bearer in the file.
    let current = current_bearer(&content);
    let needs_refresh = match current.as_deref() {
        None => true,
        Some(s) if s == cfg.mcp_bearer_placeholder => true,
        Some(jwt) => needs_refresh(jwt, refresh_window_secs).unwrap_or(true),
    };
    if !needs_refresh {
        return Ok(());
    }

    // Pull the per-claw client_secret from bao. Path matches the existing
    // homelab convention (memory: agent-durable-secrets).
    let secret_path = format!("services/{name}/papehouse");
    let client_secret = bao
        .kv_get_field(&secret_path, "client_secret")
        .await
        .with_context(|| format!("bao read {secret_path}.client_secret"))?;

    // client_credentials grant. Authentik puts the scope name in `aud`.
    let client_id = format!("mcp-{name}");
    let scope = format!("openid {client_id}");
    let form = [
        ("grant_type", "client_credentials"),
        ("client_id", client_id.as_str()),
        ("client_secret", client_secret.as_str()),
        ("scope", scope.as_str()),
    ];
    let resp = http
        .post(authentik_token_url)
        .form(&form)
        .send()
        .await
        .context("authentik token POST")?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(anyhow!("authentik token endpoint -> {status}: {body}"));
    }
    let token: AuthentikToken = resp.json().await.context("authentik token parse")?;

    // Rewrite the rendered config and restart the claw.
    let updated = substitute_bearer(&content, &cfg.mcp_bearer_placeholder, &token.access_token, current.as_deref());
    write_atomic_preserve_owner(&cfg_path, &updated)?;
    info!(claw = %name, "rotated MCP bearer; restarting claw");
    driver.restart(name).await.context("restart claw")?;
    Ok(())
}

#[derive(Debug, Deserialize)]
struct AuthentikToken {
    access_token: String,
    #[serde(default)]
    #[allow(dead_code)]
    expires_in: u64,
}

/// Pull the first `Authorization = "Bearer ..."` value from a rendered
/// config.toml. Returns the bearer string (without "Bearer "). None if no
/// match.
fn current_bearer(content: &str) -> Option<String> {
    // The renderer emits `Authorization = "Bearer <token>"`.
    let prefix = "Authorization = \"Bearer ";
    let start = content.find(prefix)?;
    let after = &content[start + prefix.len()..];
    let end = after.find('"')?;
    Some(after[..end].to_string())
}

/// Substitute the bearer value. Prefer replacing the literal placeholder
/// (first-render case) when it's still present; otherwise replace the
/// previous bearer the renderer last wrote. We do a single string replace
/// for predictability.
fn substitute_bearer(content: &str, placeholder: &str, new_bearer: &str, current: Option<&str>) -> String {
    if content.contains(placeholder) {
        return content.replacen(placeholder, new_bearer, 1);
    }
    if let Some(c) = current {
        if !c.is_empty() {
            return content.replacen(c, new_bearer, 1);
        }
    }
    content.to_string()
}

/// Decode the JWT payload (no signature verification — we trust the
/// rotation task's fetch path) and return true if the token expires within
/// `window_secs` from now.
fn needs_refresh(jwt: &str, window_secs: u64) -> Result<bool> {
    let parts: Vec<&str> = jwt.split('.').collect();
    if parts.len() != 3 {
        return Err(anyhow!("jwt does not have 3 segments"));
    }
    let payload_b64 = parts[1];
    let payload = base64_url_decode(payload_b64)?;
    let payload: serde_json::Value = serde_json::from_slice(&payload)?;
    let exp = payload
        .get("exp")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| anyhow!("no exp in jwt payload"))?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs();
    Ok(exp <= now + window_secs)
}

/// Minimal base64url decoder (no external dep). Handles missing padding.
fn base64_url_decode(s: &str) -> Result<Vec<u8>> {
    let mut s: String = s.replace('-', "+").replace('_', "/");
    while s.len() % 4 != 0 {
        s.push('=');
    }
    use base64_std::Engine;
    base64_std::engine::general_purpose::STANDARD
        .decode(&s)
        .map_err(|e| anyhow!("base64 decode: {e}"))
}

fn write_atomic_preserve_owner(path: &std::path::Path, content: &str) -> Result<()> {
    let tmp = path.with_extension("toml.rot.tmp");
    std::fs::write(&tmp, content).context("write tmp")?;
    // Try to mirror the existing file's mode + owner — the original was
    // written by the apply path as root, then chowned to 65534 manually.
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if let Ok(meta) = std::fs::metadata(path) {
            let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(meta.mode() & 0o777));
            use std::os::unix::fs::lchown;
            let _ = lchown(&tmp, Some(meta.uid()), Some(meta.gid()));
        }
    }
    std::fs::rename(&tmp, path).context("rename")?;
    Ok(())
}

// Use the base64 crate directly under an alias so we don't conflict with
// uses of `base64` as a verb elsewhere.
use base64 as base64_std;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_bearer_pulls_first_authorization_value() {
        let s = r#"
something = 1
[[mcp.servers]]
headers = { Authorization = "Bearer eyJhbGciOi.payload.sig" }
"#;
        assert_eq!(current_bearer(s).as_deref(), Some("eyJhbGciOi.payload.sig"));
    }

    #[test]
    fn current_bearer_returns_none_without_match() {
        assert_eq!(current_bearer("no bearer here").as_deref(), None);
    }

    #[test]
    fn substitute_bearer_replaces_placeholder_first() {
        let before = r#"Authorization = "Bearer __PAPEHOUSE_TOKEN__""#;
        let after = substitute_bearer(before, "__PAPEHOUSE_TOKEN__", "new.jwt.value", None);
        assert!(after.contains("Bearer new.jwt.value"));
        assert!(!after.contains("__PAPEHOUSE_TOKEN__"));
    }

    #[test]
    fn substitute_bearer_falls_back_to_current_when_placeholder_absent() {
        let before = r#"Authorization = "Bearer old.jwt.value""#;
        let after = substitute_bearer(before, "__PAPEHOUSE_TOKEN__", "new.jwt.value", Some("old.jwt.value"));
        assert!(after.contains("Bearer new.jwt.value"));
    }

    #[test]
    fn needs_refresh_returns_true_when_exp_within_window() {
        // Build a JWT-like string with exp 60s from now and window 300s.
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let payload = serde_json::json!({"exp": now + 60});
        let payload_b64 = b64_url_encode(payload.to_string().as_bytes());
        let jwt = format!("header.{payload_b64}.sig");
        assert!(needs_refresh(&jwt, 300).unwrap());
    }

    #[test]
    fn needs_refresh_returns_false_when_exp_safely_in_future() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let payload = serde_json::json!({"exp": now + 7200});
        let payload_b64 = b64_url_encode(payload.to_string().as_bytes());
        let jwt = format!("header.{payload_b64}.sig");
        assert!(!needs_refresh(&jwt, 300).unwrap());
    }

    fn b64_url_encode(b: &[u8]) -> String {
        use base64_std::Engine;
        base64_std::engine::general_purpose::URL_SAFE_NO_PAD.encode(b)
    }
}
