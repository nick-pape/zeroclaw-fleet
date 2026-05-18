//! Claw container lifecycle.
//!
//! The orchestrator never shells out to `docker`; it talks to the daemon
//! through [bollard]. Today the only [`Driver`] implementation is
//! [`docker::DockerDriver`]; a future remote driver (SSH to a Proxmox CT
//! that itself runs Docker) could slot in here without callers changing.
//!
//! Naming convention: every fleet-managed container is named
//! `claw-<name>` and labeled `com.zeroclaw-fleet.managed=true` so the
//! orchestrator's reconcile loop can find them on restart.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

pub mod docker;

/// Everything the orchestrator needs to create or reconcile a claw container.
#[derive(Debug, Clone)]
pub struct ClawSpec {
    /// Stable kebab-case identifier (e.g. `"grocery"`).
    pub name: String,
    /// Container image — digest-pinned strongly encouraged.
    pub image: String,
    /// Hard memory limit in bytes. `None` = no limit (not recommended).
    pub mem_limit_bytes: Option<i64>,
    /// CPU limit as a fraction (1.0 = one core, 0.5 = half a core).
    /// Translated to docker `NanoCPUs` (= `cpu * 1e9`).
    pub cpu_limit: Option<f64>,
    /// Docker restart policy (`"unless-stopped"`, `"always"`, `"no"`).
    pub restart: String,
    /// Environment variables passed straight through to the container.
    pub env: BTreeMap<String, String>,
    /// Path on the host containing the per-claw `config.toml` (rendered by
    /// [`crate::render`]). Bind-mounted at `/zeroclaw-data/config` inside
    /// the claw. Mounted read-write so the MCP bearer rotation task can
    /// rewrite atomically.
    pub config_dir: PathBuf,
    /// Optional path on the host to an env-file containing `KEY=VALUE`
    /// lines (e.g. `OPENAI_API_KEY=...`). Read by docker at start time;
    /// keeps the secret out of compose env + container inspect output.
    pub env_file: Option<PathBuf>,
    /// Name of a docker named volume that persists `/zeroclaw-data`
    /// (workspace, sqlite memory db, dashboard SPA). Survives container
    /// recreate.
    pub data_volume: String,
    /// Docker network the container attaches to (e.g.
    /// `"claws-internal"`). Must be created out-of-band.
    pub network: String,
    /// Docker logging driver settings (json-file rotation).
    pub log: LogSettings,
    /// Container→host port publishes (in addition to the gateway port,
    /// which is reached via the orchestrator's proxy). Used for
    /// channel listeners that must be reachable from outside the
    /// orchestrator's docker network.
    pub published_ports: Vec<(u16, u16)>,
}

/// `docker logs` retention knobs. Maps to docker's `json-file` driver opts.
#[derive(Debug, Clone)]
pub struct LogSettings {
    /// Max size per log file (e.g. `"20m"`).
    pub max_size: String,
    /// Number of rotated files to keep.
    pub max_file: u32,
}

impl Default for LogSettings {
    fn default() -> Self {
        Self { max_size: "20m".into(), max_file: 3 }
    }
}

/// Health states a claw container can be in. Maps loosely to docker's
/// `State.Status` + `State.Health.Status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClawHealth {
    /// Container is running and either has no healthcheck or its
    /// healthcheck reports `healthy`.
    Healthy,
    /// Container is running but its healthcheck is in `starting` (still
    /// inside `start_period`).
    Starting,
    /// Container is running but its healthcheck reports `unhealthy`.
    Unhealthy,
    /// Container exists but is not running (exited, paused, dead).
    Stopped,
    /// No container exists for this claw name.
    Missing,
}

/// Snapshot returned by [`Driver::status`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClawStatus {
    pub name: String,
    pub health: ClawHealth,
    /// Resolved image (after image-pin / digest resolution by the daemon).
    pub image: Option<String>,
    /// Docker container ID (12-char short form), if it exists.
    pub container_id: Option<String>,
    /// Last-started timestamp, RFC3339, if the container has ever run.
    pub started_at: Option<String>,
}

/// Outcome of an idempotent [`Driver::ensure`] call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnsureOutcome {
    /// No container existed; we created and started one.
    Created,
    /// A container existed with a matching spec; we left it alone.
    Unchanged,
    /// A container existed but its spec drifted (e.g. image was bumped);
    /// we stopped and recreated it.
    Recreated,
}

/// Claw lifecycle operations. Implementations should be idempotent and
/// crash-safe: a partial failure followed by retry must converge.
pub trait Driver {
    /// Create the container if missing, or recreate it if its spec drifted.
    /// Always leaves the container in the `running` state on success.
    fn ensure(
        &self,
        spec: &ClawSpec,
    ) -> impl std::future::Future<Output = anyhow::Result<EnsureOutcome>> + Send;

    /// Start a stopped container. No-op if already running.
    fn start(
        &self,
        name: &str,
    ) -> impl std::future::Future<Output = anyhow::Result<()>> + Send;

    /// Stop a running container with a graceful SIGTERM. No-op if already
    /// stopped.
    fn stop(
        &self,
        name: &str,
    ) -> impl std::future::Future<Output = anyhow::Result<()>> + Send;

    /// Restart a container (stop + start).
    fn restart(
        &self,
        name: &str,
    ) -> impl std::future::Future<Output = anyhow::Result<()>> + Send;

    /// Stop + remove the container. Keeps the named volume.
    fn remove(
        &self,
        name: &str,
    ) -> impl std::future::Future<Output = anyhow::Result<()>> + Send;

    /// Stop + remove the container AND its named volume. Used during full
    /// tenant deprovision.
    fn purge(
        &self,
        name: &str,
        data_volume: &str,
    ) -> impl std::future::Future<Output = anyhow::Result<()>> + Send;

    /// Return the current status. `health == Missing` when no container
    /// exists for the given name.
    fn status(
        &self,
        name: &str,
    ) -> impl std::future::Future<Output = anyhow::Result<ClawStatus>> + Send;

    /// Last `lines` lines of stdout+stderr. For streaming, use the
    /// gateway's SSE log endpoint instead.
    fn logs(
        &self,
        name: &str,
        lines: usize,
    ) -> impl std::future::Future<Output = anyhow::Result<String>> + Send;
}

/// Label every fleet-managed container with this so reconcile can find them.
pub const MANAGED_LABEL: &str = "com.zeroclaw-fleet.managed";
/// Holds the claw's logical name (matches `ClawSpec.name`).
pub const CLAW_LABEL: &str = "com.zeroclaw-fleet.claw";

/// Canonical container name for a claw.
pub fn container_name(claw: &str) -> String {
    format!("claw-{claw}")
}
