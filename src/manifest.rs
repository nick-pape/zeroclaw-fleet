//! Manifest parsing.
//!
//! The fleet is described by two kinds of files:
//!
//! * **`fleet.yaml`** — top-level: defaults that apply to every claw plus the
//!   ordered list of active claw names.
//! * **`claws/<name>.toml`** — one per claw. Combines orchestrator metadata
//!   (under a `[_fleet]` table) with the TOML body to deep-merge onto
//!   `base.toml` before rendering.
//!
//! The renderer (see `render.rs`) consumes both, plus `base.toml`, and emits
//! the final per-claw `config.toml` that ZeroClaw loads.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Top-level `fleet.yaml`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct FleetManifest {
    #[serde(default)]
    pub defaults: ClawDefaults,
    #[serde(default)]
    pub claws: Vec<String>,
}

/// Defaults applied to every claw unless overridden in the overlay.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ClawDefaults {
    /// Container image (digest-pinned strongly encouraged).
    pub image: Option<String>,
    /// Memory limit string accepted by docker (e.g. `"1g"`).
    pub mem_limit: Option<String>,
    /// CPU limit (e.g. `1.0`).
    pub cpu_limit: Option<f64>,
    /// Restart policy (e.g. `"unless-stopped"`).
    pub restart: Option<String>,
    /// Max size per log file (docker json-file driver).
    pub log_max_size: Option<String>,
    /// Number of rotated log files to keep.
    pub log_max_file: Option<u32>,
}

/// Orchestrator-only metadata extracted from a claw overlay's `[_fleet]` table.
///
/// Everything outside `[_fleet]` is the merge body and is handed to the
/// renderer untouched.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct FleetMetadata {
    /// Stable kebab-case identifier. Used as docker container name, secret
    /// store path segment, OAuth client id, MCP hub identity key, and host
    /// label.
    pub name: String,

    /// Optional per-claw image override. Falls back to `defaults.image`.
    pub image: Option<String>,

    /// Friendly tags pointing at MCP scopes this claw should reach (e.g.
    /// `["heb", "kitchenowl"]`). Used by provisioning + future dynamic
    /// `auto_approve` expansion. Cosmetic until then.
    #[serde(default)]
    pub mcp_scopes: Vec<String>,

    /// Optional path (relative to fleet dir) to a markdown file containing
    /// the per-claw system prompt.
    pub prompt_path: Option<PathBuf>,

    /// When true, provisioning is skipped during tenant create — the claw is
    /// expected to already have its identity, secrets, and hub policy block
    /// in place. Used for migrating an existing standalone ZeroClaw instance
    /// into the fleet without re-minting credentials.
    #[serde(default)]
    pub import: bool,
}

/// Parsed claw overlay: orchestrator metadata + the raw merge body.
#[derive(Debug, Clone)]
pub struct ClawOverlay {
    pub fleet: FleetMetadata,
    /// The TOML body to deep-merge onto base.toml. Already had `[_fleet]`
    /// stripped.
    pub body: toml::Table,
}

impl ClawOverlay {
    /// Friendly name from `[branding] display_name`, if set. Falls back to
    /// the kebab `_fleet.name` so the UI always has something to render.
    pub fn display_name(&self) -> String {
        self.body
            .get("branding")
            .and_then(|v| v.as_table())
            .and_then(|t| t.get("display_name"))
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .unwrap_or_else(|| self.fleet.name.clone())
    }
}

impl ClawOverlay {
    /// Parse a claw overlay from a TOML string.
    pub fn from_str(src: &str) -> Result<Self> {
        let mut table: toml::Table = toml::from_str(src).context("parse overlay TOML")?;
        let fleet_value = table
            .remove("_fleet")
            .context("overlay is missing required `[_fleet]` table")?;
        let fleet: FleetMetadata = fleet_value
            .try_into()
            .context("parse `[_fleet]` metadata")?;
        Ok(Self { fleet, body: table })
    }

    /// Parse a claw overlay from a file on disk.
    pub fn from_path(path: &Path) -> Result<Self> {
        let src = std::fs::read_to_string(path)
            .with_context(|| format!("read claw overlay {}", path.display()))?;
        Self::from_str(&src)
    }
}

impl FleetManifest {
    /// Parse `fleet.yaml` from a string.
    pub fn from_str(src: &str) -> Result<Self> {
        serde_yaml::from_str(src).context("parse fleet.yaml")
    }

    /// Parse `fleet.yaml` from a file on disk.
    pub fn from_path(path: &Path) -> Result<Self> {
        let src = std::fs::read_to_string(path)
            .with_context(|| format!("read fleet manifest {}", path.display()))?;
        Self::from_str(&src)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fleet_manifest_parses_with_defaults_and_claws() {
        let src = r#"
defaults:
  image: example/zeroclaw:latest
  mem_limit: "1g"
  cpu_limit: 1.0
claws:
  - alpha
  - beta
"#;
        let m = FleetManifest::from_str(src).unwrap();
        assert_eq!(m.defaults.image.as_deref(), Some("example/zeroclaw:latest"));
        assert_eq!(m.claws, vec!["alpha".to_string(), "beta".to_string()]);
    }

    #[test]
    fn overlay_parses_fleet_metadata_and_keeps_body() {
        let src = r#"
[_fleet]
name = "alpha"
mcp_scopes = ["search"]

[providers]
fallback = "openai"

[branding]
display_name = "Alpha"
"#;
        let o = ClawOverlay::from_str(src).unwrap();
        assert_eq!(o.fleet.name, "alpha");
        assert_eq!(o.fleet.mcp_scopes, vec!["search".to_string()]);
        assert!(o.body.contains_key("providers"));
        assert!(o.body.contains_key("branding"));
        assert!(!o.body.contains_key("_fleet"));
    }

    #[test]
    fn display_name_prefers_branding_field() {
        let with_branding = ClawOverlay::from_str(r#"
[_fleet]
name = "grocery"
[branding]
display_name = "H-E-Buddy"
"#).unwrap();
        assert_eq!(with_branding.display_name(), "H-E-Buddy");
    }

    #[test]
    fn display_name_falls_back_to_kebab_name() {
        let no_branding = ClawOverlay::from_str(r#"
[_fleet]
name = "alpha"
"#).unwrap();
        assert_eq!(no_branding.display_name(), "alpha");
    }

    #[test]
    fn display_name_falls_back_when_branding_table_lacks_field() {
        let partial = ClawOverlay::from_str(r#"
[_fleet]
name = "beta"
[branding]
default_color_theme = "dark"
"#).unwrap();
        assert_eq!(partial.display_name(), "beta");
    }

    #[test]
    fn overlay_missing_fleet_table_is_rejected() {
        let src = r#"[providers]
fallback = "openai"
"#;
        let err = ClawOverlay::from_str(src).unwrap_err();
        assert!(err.to_string().contains("`[_fleet]`"));
    }
}
