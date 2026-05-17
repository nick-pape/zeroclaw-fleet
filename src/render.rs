//! Per-claw `config.toml` rendering.
//!
//! Input:
//!   * `base.toml` — fleet-wide ZeroClaw config (everything every claw shares).
//!   * the claw's [`ClawOverlay`] body — small TOML that deep-merges over base.
//!   * a set of orchestrator-controlled [`Injections`] (bearer hash, MCP
//!     credential placeholder, mandatory gateway settings).
//!
//! Output: the final `config.toml` text written to the claw's mounted config
//! directory.
//!
//! Deep-merge semantics:
//!   * Both sides are tables → recurse.
//!   * Either side is not a table → overlay value replaces base value.
//!     (Arrays are atomic; we never element-merge — that's almost never what
//!     you want for config like `auto_approve`.)

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use toml::Value;
use toml::value::Table;

use crate::manifest::ClawOverlay;

/// Fields the orchestrator unconditionally controls. They are written *after*
/// the deep-merge, so an overlay cannot accidentally shadow them.
pub struct Injections {
    /// Raw bearer token (e.g. `zc_<uuid>`) used by the orchestrator when
    /// calling the claw's `/api/*`. Hashed into `[gateway] paired_tokens`.
    pub orchestrator_bearer: String,

    /// Placeholder string the rotation task substitutes when minting a fresh
    /// MCP bearer. Written into the MCP `Authorization` header.
    pub mcp_bearer_placeholder: String,

    /// Hub MCP endpoint (e.g. `https://mcp.example.com/mcp`). Wired into the
    /// `[[mcp.servers]]` block keyed by `mcp_server_name`.
    pub mcp_server_url: String,

    /// Name to use for the `[[mcp.servers]]` block (becomes the MCP-tool
    /// prefix that ZeroClaw exposes — e.g. `papehouse` → `papehouse__heb_*`).
    pub mcp_server_name: String,
}

/// Render a claw's final `config.toml`.
pub fn render(base_toml: &str, overlay: &ClawOverlay, inj: &Injections) -> Result<String> {
    let mut base: Table = toml::from_str(base_toml).context("parse base.toml")?;
    deep_merge(&mut base, overlay.body.clone());
    inject_gateway(&mut base, inj);
    inject_mcp(&mut base, inj);
    Ok(toml::to_string_pretty(&base).context("serialize merged config")?)
}

/// Recursively merge `source` into `target`. Tables merge; everything else is
/// replaced.
pub fn deep_merge(target: &mut Table, source: Table) {
    for (key, value) in source {
        match (target.get_mut(&key), value) {
            (Some(Value::Table(t_tbl)), Value::Table(s_tbl)) => {
                deep_merge(t_tbl, s_tbl);
            }
            (_, v) => {
                target.insert(key, v);
            }
        }
    }
}

/// Hash a bearer token the way ZeroClaw's `paired_tokens` expects.
///
/// ZeroClaw accepts either a plaintext `zc_...` token (which it hashes on
/// load) OR a pre-hashed 64-char hex string. We always pre-hash so a
/// `cat config.toml` doesn't leak the bearer.
pub fn hash_bearer(bearer: &str) -> String {
    let digest = Sha256::digest(bearer.as_bytes());
    hex::encode(digest)
}

fn inject_gateway(base: &mut Table, inj: &Injections) {
    let gw = base
        .entry("gateway".to_string())
        .or_insert(Value::Table(Table::new()))
        .as_table_mut()
        .expect("gateway is a table");

    let hashed = hash_bearer(&inj.orchestrator_bearer);
    gw.insert(
        "paired_tokens".into(),
        Value::Array(vec![Value::String(hashed)]),
    );
    gw.insert("trust_forwarded_headers".into(), Value::Boolean(true));
    gw.insert(
        "web_dist_dir".into(),
        Value::String("/usr/share/zeroclawlabs/web/dist".into()),
    );
}

fn inject_mcp(base: &mut Table, inj: &Injections) {
    let mcp = base
        .entry("mcp".to_string())
        .or_insert(Value::Table(Table::new()))
        .as_table_mut()
        .expect("mcp is a table");
    mcp.insert("enabled".into(), Value::Boolean(true));
    // ZeroClaw defaults this to true, which forces all MCP tools through a
    // `tool_search` meta-tool and triggers the post-inference gateway hang
    // we observed in prod. Force false so every hub tool surfaces directly
    // in the model's tool schema.
    mcp.insert("deferred_loading".into(), Value::Boolean(false));

    let mut server = Table::new();
    server.insert("name".into(), Value::String(inj.mcp_server_name.clone()));
    server.insert("transport".into(), Value::String("http".into()));
    server.insert("url".into(), Value::String(inj.mcp_server_url.clone()));
    let mut headers = Table::new();
    headers.insert(
        "Authorization".into(),
        Value::String(format!("Bearer {}", inj.mcp_bearer_placeholder)),
    );
    server.insert("headers".into(), Value::Table(headers));
    server.insert("tool_timeout_secs".into(), Value::Integer(60));

    mcp.insert("servers".into(), Value::Array(vec![Value::Table(server)]));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_inj() -> Injections {
        Injections {
            orchestrator_bearer: "zc_test_bearer".into(),
            mcp_bearer_placeholder: "__MCP_BEARER__".into(),
            mcp_server_url: "https://hub.example.com/mcp".into(),
            mcp_server_name: "hub".into(),
        }
    }

    #[test]
    fn deep_merge_replaces_scalars_and_arrays() {
        let mut base: Table = toml::from_str(r#"
greeting = "hello"
tags = ["a", "b"]
"#).unwrap();
        let overlay: Table = toml::from_str(r#"
greeting = "hi"
tags = ["c"]
"#).unwrap();
        deep_merge(&mut base, overlay);
        assert_eq!(base["greeting"].as_str().unwrap(), "hi");
        assert_eq!(base["tags"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn deep_merge_recurses_into_tables() {
        let mut base: Table = toml::from_str(r#"
[autonomy]
level = "supervised"
max_actions_per_hour = 20
"#).unwrap();
        let overlay: Table = toml::from_str(r#"
[autonomy]
max_actions_per_hour = 40
"#).unwrap();
        deep_merge(&mut base, overlay);
        assert_eq!(base["autonomy"]["level"].as_str().unwrap(), "supervised");
        assert_eq!(base["autonomy"]["max_actions_per_hour"].as_integer().unwrap(), 40);
    }

    #[test]
    fn render_injects_gateway_fields_after_merge() {
        let base = r#"
[autonomy]
level = "supervised"

[gateway]
port = 42617
host = "0.0.0.0"
require_pairing = true
paired_tokens = ["overridden-by-renderer"]
"#;
        let overlay = ClawOverlay::from_str(r#"
[_fleet]
name = "alpha"
"#).unwrap();

        let rendered = render(base, &overlay, &sample_inj()).unwrap();
        let parsed: Table = toml::from_str(&rendered).unwrap();

        // Renderer wins for these fields:
        let tokens = parsed["gateway"]["paired_tokens"].as_array().unwrap();
        assert_eq!(tokens.len(), 1);
        assert_eq!(tokens[0].as_str().unwrap().len(), 64); // sha256 hex
        assert_eq!(parsed["gateway"]["trust_forwarded_headers"].as_bool().unwrap(), true);
        assert_eq!(parsed["gateway"]["web_dist_dir"].as_str().unwrap(), "/usr/share/zeroclawlabs/web/dist");

        // Non-injected gateway fields survive:
        assert_eq!(parsed["gateway"]["port"].as_integer().unwrap(), 42617);
        assert_eq!(parsed["gateway"]["require_pairing"].as_bool().unwrap(), true);
    }

    #[test]
    fn render_emits_mcp_server_block_with_bearer_placeholder() {
        let base = r#"
[autonomy]
level = "supervised"
"#;
        let overlay = ClawOverlay::from_str(r#"
[_fleet]
name = "alpha"
"#).unwrap();

        let rendered = render(base, &overlay, &sample_inj()).unwrap();
        let parsed: Table = toml::from_str(&rendered).unwrap();

        let mcp = &parsed["mcp"];
        assert_eq!(mcp["enabled"].as_bool().unwrap(), true);
        assert_eq!(mcp["deferred_loading"].as_bool().unwrap(), false);
        let server = &mcp["servers"][0];
        assert_eq!(server["name"].as_str().unwrap(), "hub");
        assert_eq!(server["url"].as_str().unwrap(), "https://hub.example.com/mcp");
        assert_eq!(
            server["headers"]["Authorization"].as_str().unwrap(),
            "Bearer __MCP_BEARER__"
        );
    }

    #[test]
    fn render_lets_overlay_drive_per_claw_branding_and_autonomy() {
        let base = r#"
[autonomy]
level = "supervised"
auto_approve = []
"#;
        let overlay = ClawOverlay::from_str(r#"
[_fleet]
name = "alpha"

[branding]
display_name = "Alpha"
default_color_theme = "dark"

[autonomy]
auto_approve = ["one", "two", "three"]
"#).unwrap();

        let rendered = render(base, &overlay, &sample_inj()).unwrap();
        let parsed: Table = toml::from_str(&rendered).unwrap();

        assert_eq!(parsed["branding"]["display_name"].as_str().unwrap(), "Alpha");
        assert_eq!(parsed["autonomy"]["auto_approve"].as_array().unwrap().len(), 3);
        assert_eq!(parsed["autonomy"]["level"].as_str().unwrap(), "supervised");
    }

    #[test]
    fn hash_bearer_is_deterministic_64_hex_chars() {
        let h1 = hash_bearer("zc_alpha");
        let h2 = hash_bearer("zc_alpha");
        let h3 = hash_bearer("zc_beta");
        assert_eq!(h1, h2);
        assert_ne!(h1, h3);
        assert_eq!(h1.len(), 64);
        assert!(h1.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
