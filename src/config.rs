//! Orchestrator runtime configuration.
//!
//! Distinct from per-claw `config.toml` (rendered by [`crate::render`]).
//! Provided via CLI arguments / env vars at startup; immutable for the
//! lifetime of the process.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use crate::proxy::ProxyConfig;

/// Shared, immutable configuration the orchestrator was started with.
#[derive(Debug, Clone)]
pub struct OrchestratorConfig {
    /// HTTP bind address.
    pub bind: SocketAddr,
    /// Path to the fleet manifest directory (contains `fleet.yaml`,
    /// `base.toml`, `claws/`, optional `prompts/`, optional `branding/`).
    pub fleet_dir: PathBuf,
    /// Per-claw state (rendered configs, paired_tokens, provisioning state).
    pub state_dir: PathBuf,
    /// Host-routing config: which host header is the fleet UI vs a claw.
    pub proxy: ProxyConfig,
    /// MCP hub URL written into every rendered claw config.
    pub mcp_server_url: String,
    /// MCP namespace name (becomes the ZeroClaw tool prefix).
    pub mcp_server_name: String,
    /// Placeholder substituted into rendered MCP Authorization headers
    /// until the rotation task replaces it with a live JWT.
    pub mcp_bearer_placeholder: String,
    /// Cost poller cadence in seconds.
    pub cost_poll_interval_secs: u64,
}

impl OrchestratorConfig {
    /// Wrap in `Arc` for cheap sharing across tasks + handlers.
    pub fn into_shared(self) -> Arc<Self> {
        Arc::new(self)
    }
}
