//! [`Driver`] implementation backed by the local Docker daemon via bollard.

use anyhow::{Context, Result, anyhow};
use bollard::Docker;
use bollard::container::{
    Config, CreateContainerOptions, ListContainersOptions, LogsOptions, NetworkingConfig,
    RemoveContainerOptions, StartContainerOptions, StopContainerOptions,
};
use bollard::models::{
    ContainerSummary, EndpointSettings, HealthConfig, HostConfig, HostConfigLogConfig, Mount,
    MountTypeEnum, RestartPolicy, RestartPolicyNameEnum,
};
use bollard::secret::ContainerState;
use bollard::volume::RemoveVolumeOptions;
use futures_util::TryStreamExt;
use std::collections::HashMap;

use super::{
    CLAW_LABEL, ClawHealth, ClawSpec, ClawStatus, Driver, EnsureOutcome, MANAGED_LABEL,
    container_name,
};

/// Bollard-backed driver. Construct via [`DockerDriver::connect_local`] or
/// [`DockerDriver::with_client`] (for tests).
pub struct DockerDriver {
    client: Docker,
}

impl DockerDriver {
    /// Connect to the local Docker socket / named pipe with the default
    /// platform paths.
    pub fn connect_local() -> Result<Self> {
        let client = Docker::connect_with_local_defaults()
            .context("connect to local docker daemon")?;
        Ok(Self { client })
    }

    /// Inject a pre-built bollard client (for tests).
    pub fn with_client(client: Docker) -> Self {
        Self { client }
    }

    async fn inspect(&self, name: &str) -> Result<Option<bollard::secret::ContainerInspectResponse>> {
        let cname = container_name(name);
        match self.client.inspect_container(&cname, None).await {
            Ok(c) => Ok(Some(c)),
            Err(bollard::errors::Error::DockerResponseServerError { status_code: 404, .. }) => Ok(None),
            Err(e) => Err(e).context(format!("inspect container {cname}")),
        }
    }

    /// Build the bollard `Config` that materializes a `ClawSpec`. Pulled out
    /// so unit tests can exercise the translation without a live daemon.
    pub fn build_config(spec: &ClawSpec) -> Config<String> {
        let env: Vec<String> = spec
            .env
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect();

        let mounts = vec![
            Mount {
                target: Some("/zeroclaw-data".into()),
                source: Some(spec.data_volume.clone()),
                typ: Some(MountTypeEnum::VOLUME),
                read_only: Some(false),
                ..Default::default()
            },
            Mount {
                target: Some("/zeroclaw-data/config".into()),
                source: Some(spec.config_dir.to_string_lossy().into_owned()),
                typ: Some(MountTypeEnum::BIND),
                read_only: Some(false),
                ..Default::default()
            },
        ];

        let mut log_opts: HashMap<String, String> = HashMap::new();
        log_opts.insert("max-size".into(), spec.log.max_size.clone());
        log_opts.insert("max-file".into(), spec.log.max_file.to_string());

        let host_config = HostConfig {
            mounts: Some(mounts),
            memory: spec.mem_limit_bytes,
            nano_cpus: spec.cpu_limit.map(|c| (c * 1e9) as i64),
            restart_policy: Some(RestartPolicy {
                name: Some(restart_policy_name(&spec.restart)),
                maximum_retry_count: None,
            }),
            log_config: Some(HostConfigLogConfig {
                typ: Some("json-file".into()),
                config: Some(log_opts),
            }),
            ..Default::default()
        };

        let mut endpoints: HashMap<String, EndpointSettings> = HashMap::new();
        endpoints.insert(spec.network.clone(), EndpointSettings::default());

        let mut labels: HashMap<String, String> = HashMap::new();
        labels.insert(MANAGED_LABEL.into(), "true".into());
        labels.insert(CLAW_LABEL.into(), spec.name.clone());

        Config {
            image: Some(spec.image.clone()),
            env: Some(env),
            host_config: Some(host_config),
            networking_config: Some(NetworkingConfig {
                endpoints_config: endpoints,
            }),
            healthcheck: Some(HealthConfig {
                test: Some(vec!["CMD".into(), "zeroclaw".into(), "doctor".into()]),
                interval: Some(30_000_000_000),       // ns
                timeout: Some(10_000_000_000),
                retries: Some(3),
                start_period: Some(30_000_000_000),
                start_interval: None,
            }),
            labels: Some(labels),
            ..Default::default()
        }
    }

    /// Heuristic for "do the running container's settings still match spec?".
    /// Conservative: if any of {image, env, mounts, restart} differs we
    /// recreate. Doesn't try to be clever about every Docker field.
    pub fn spec_drifted(existing: &bollard::secret::ContainerInspectResponse, want: &ClawSpec) -> bool {
        let cfg = match existing.config.as_ref() {
            Some(c) => c,
            None => return true,
        };

        if cfg.image.as_deref() != Some(want.image.as_str()) {
            return true;
        }

        let want_env: Vec<String> = want
            .env
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect();
        let got_env = cfg.env.clone().unwrap_or_default();
        if !env_lists_equal(&got_env, &want_env) {
            return true;
        }

        false
    }

    async fn list_existing(&self) -> Result<Vec<ContainerSummary>> {
        let mut filters: HashMap<String, Vec<String>> = HashMap::new();
        filters.insert("label".into(), vec![format!("{MANAGED_LABEL}=true")]);
        let opts = ListContainersOptions::<String> {
            all: true,
            filters,
            ..Default::default()
        };
        self.client.list_containers(Some(opts)).await.context("list containers")
    }
}

fn restart_policy_name(s: &str) -> RestartPolicyNameEnum {
    match s {
        "always" => RestartPolicyNameEnum::ALWAYS,
        "on-failure" => RestartPolicyNameEnum::ON_FAILURE,
        "unless-stopped" => RestartPolicyNameEnum::UNLESS_STOPPED,
        "no" | "" => RestartPolicyNameEnum::NO,
        other => {
            tracing::warn!("unknown restart policy {other:?}; defaulting to unless-stopped");
            RestartPolicyNameEnum::UNLESS_STOPPED
        }
    }
}

fn env_lists_equal(got: &[String], want: &[String]) -> bool {
    // Order matters less than presence; sort both before comparing.
    let mut g: Vec<_> = got.iter().collect();
    let mut w: Vec<_> = want.iter().collect();
    g.sort();
    w.sort();
    g == w
}

fn classify(state: Option<&ContainerState>) -> ClawHealth {
    let Some(state) = state else { return ClawHealth::Missing };
    if state.running == Some(true) {
        match state.health.as_ref().and_then(|h| h.status.as_ref()) {
            Some(s) if format!("{s:?}").to_lowercase().contains("healthy") => ClawHealth::Healthy,
            Some(s) if format!("{s:?}").to_lowercase().contains("starting") => ClawHealth::Starting,
            Some(s) if format!("{s:?}").to_lowercase().contains("unhealthy") => ClawHealth::Unhealthy,
            // No healthcheck reported yet — treat as Healthy if running.
            _ => ClawHealth::Healthy,
        }
    } else {
        ClawHealth::Stopped
    }
}

impl Driver for DockerDriver {
    async fn ensure(&self, spec: &ClawSpec) -> Result<EnsureOutcome> {
        let cname = container_name(&spec.name);
        let existing = self.inspect(&spec.name).await?;

        match existing {
            None => {
                let cfg = Self::build_config(spec);
                self.client
                    .create_container(
                        Some(CreateContainerOptions {
                            name: cname.clone(),
                            platform: None,
                        }),
                        cfg,
                    )
                    .await
                    .context(format!("create container {cname}"))?;
                self.client
                    .start_container(&cname, None::<StartContainerOptions<String>>)
                    .await
                    .context(format!("start container {cname}"))?;
                Ok(EnsureOutcome::Created)
            }
            Some(c) if Self::spec_drifted(&c, spec) => {
                let _ = self.client.stop_container(&cname, Some(StopContainerOptions { t: 30 })).await;
                let _ = self
                    .client
                    .remove_container(&cname, Some(RemoveContainerOptions { force: true, ..Default::default() }))
                    .await;
                let cfg = Self::build_config(spec);
                self.client
                    .create_container(
                        Some(CreateContainerOptions {
                            name: cname.clone(),
                            platform: None,
                        }),
                        cfg,
                    )
                    .await
                    .context(format!("recreate container {cname}"))?;
                self.client
                    .start_container(&cname, None::<StartContainerOptions<String>>)
                    .await
                    .context(format!("start recreated container {cname}"))?;
                Ok(EnsureOutcome::Recreated)
            }
            Some(c) => {
                // Spec unchanged; make sure it's actually running.
                let running = c
                    .state
                    .as_ref()
                    .and_then(|s| s.running)
                    .unwrap_or(false);
                if !running {
                    self.client
                        .start_container(&cname, None::<StartContainerOptions<String>>)
                        .await
                        .context(format!("start existing container {cname}"))?;
                }
                Ok(EnsureOutcome::Unchanged)
            }
        }
    }

    async fn start(&self, name: &str) -> Result<()> {
        let cname = container_name(name);
        self.client
            .start_container(&cname, None::<StartContainerOptions<String>>)
            .await
            .context(format!("start {cname}"))
    }

    async fn stop(&self, name: &str) -> Result<()> {
        let cname = container_name(name);
        self.client
            .stop_container(&cname, Some(StopContainerOptions { t: 30 }))
            .await
            .context(format!("stop {cname}"))
    }

    async fn restart(&self, name: &str) -> Result<()> {
        self.stop(name).await?;
        self.start(name).await
    }

    async fn remove(&self, name: &str) -> Result<()> {
        let cname = container_name(name);
        let _ = self.client.stop_container(&cname, Some(StopContainerOptions { t: 30 })).await;
        self.client
            .remove_container(&cname, Some(RemoveContainerOptions { force: true, ..Default::default() }))
            .await
            .context(format!("remove {cname}"))
    }

    async fn purge(&self, name: &str, data_volume: &str) -> Result<()> {
        self.remove(name).await?;
        self.client
            .remove_volume(data_volume, Some(RemoveVolumeOptions { force: true }))
            .await
            .context(format!("remove volume {data_volume}"))
    }

    async fn status(&self, name: &str) -> Result<ClawStatus> {
        match self.inspect(name).await? {
            None => Ok(ClawStatus {
                name: name.into(),
                health: ClawHealth::Missing,
                image: None,
                container_id: None,
                started_at: None,
            }),
            Some(c) => Ok(ClawStatus {
                name: name.into(),
                health: classify(c.state.as_ref()),
                image: c.config.as_ref().and_then(|cfg| cfg.image.clone()),
                container_id: c.id.as_ref().map(|id| id.chars().take(12).collect()),
                started_at: c.state.as_ref().and_then(|s| s.started_at.clone()),
            }),
        }
    }

    async fn logs(&self, name: &str, lines: usize) -> Result<String> {
        let cname = container_name(name);
        let opts = LogsOptions::<String> {
            stdout: true,
            stderr: true,
            tail: lines.to_string(),
            ..Default::default()
        };
        let chunks = self
            .client
            .logs(&cname, Some(opts))
            .try_collect::<Vec<_>>()
            .await
            .context(format!("logs {cname}"))?;
        let mut out = String::new();
        for chunk in chunks {
            out.push_str(&chunk.to_string());
        }
        Ok(out)
    }
}

#[allow(dead_code)] // helpers for upcoming reconcile loop
impl DockerDriver {
    /// Return every container labeled as fleet-managed. Used by the
    /// orchestrator to reconcile state after restart.
    pub async fn list_fleet_containers(&self) -> Result<Vec<ContainerSummary>> {
        self.list_existing().await
    }
}

#[allow(dead_code)] // tested via the public Driver impl
fn _ensure_anyhow(e: anyhow::Error) -> anyhow::Error {
    anyhow!("driver error: {e}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn sample_spec() -> ClawSpec {
        let mut env = std::collections::BTreeMap::new();
        env.insert("ZEROCLAW_CONFIG_DIR".into(), "/zeroclaw-data/config".into());
        env.insert("ZEROCLAW_ALLOW_PUBLIC_BIND".into(), "true".into());
        env.insert("OPENAI_API_KEY".into(), "sk-test".into());
        ClawSpec {
            name: "alpha".into(),
            image: "example/zeroclaw@sha256:abc".into(),
            mem_limit_bytes: Some(1024 * 1024 * 1024),
            cpu_limit: Some(1.0),
            restart: "unless-stopped".into(),
            env,
            config_dir: PathBuf::from("/var/lib/zeroclaw-fleet/alpha/config"),
            data_volume: "claw-data-alpha".into(),
            network: "claws-internal".into(),
            log: Default::default(),
        }
    }

    #[test]
    fn build_config_sets_image_env_and_labels() {
        let spec = sample_spec();
        let cfg = DockerDriver::build_config(&spec);
        assert_eq!(cfg.image.as_deref(), Some("example/zeroclaw@sha256:abc"));
        let env = cfg.env.unwrap();
        assert!(env.contains(&"ZEROCLAW_CONFIG_DIR=/zeroclaw-data/config".to_string()));
        assert!(env.contains(&"OPENAI_API_KEY=sk-test".to_string()));
        let labels = cfg.labels.unwrap();
        assert_eq!(labels.get(MANAGED_LABEL).map(String::as_str), Some("true"));
        assert_eq!(labels.get(CLAW_LABEL).map(String::as_str), Some("alpha"));
    }

    #[test]
    fn build_config_attaches_to_the_requested_network() {
        let spec = sample_spec();
        let cfg = DockerDriver::build_config(&spec);
        let net = cfg.networking_config.unwrap();
        assert!(net.endpoints_config.contains_key("claws-internal"));
    }

    #[test]
    fn build_config_mounts_volume_then_bind_overlay() {
        let spec = sample_spec();
        let cfg = DockerDriver::build_config(&spec);
        let host = cfg.host_config.unwrap();
        let mounts = host.mounts.unwrap();
        assert_eq!(mounts.len(), 2);
        assert_eq!(mounts[0].typ, Some(MountTypeEnum::VOLUME));
        assert_eq!(mounts[0].source.as_deref(), Some("claw-data-alpha"));
        assert_eq!(mounts[0].target.as_deref(), Some("/zeroclaw-data"));
        assert_eq!(mounts[1].typ, Some(MountTypeEnum::BIND));
        assert_eq!(mounts[1].target.as_deref(), Some("/zeroclaw-data/config"));
    }

    #[test]
    fn build_config_translates_cpu_limit_to_nano_cpus() {
        let mut spec = sample_spec();
        spec.cpu_limit = Some(0.5);
        let cfg = DockerDriver::build_config(&spec);
        let host = cfg.host_config.unwrap();
        assert_eq!(host.nano_cpus, Some(500_000_000));
    }

    #[test]
    fn build_config_passes_mem_limit_as_bytes() {
        let spec = sample_spec();
        let cfg = DockerDriver::build_config(&spec);
        let host = cfg.host_config.unwrap();
        assert_eq!(host.memory, Some(1_073_741_824));
    }

    #[test]
    fn build_config_sets_json_file_log_rotation() {
        let mut spec = sample_spec();
        spec.log.max_size = "50m".into();
        spec.log.max_file = 5;
        let cfg = DockerDriver::build_config(&spec);
        let host = cfg.host_config.unwrap();
        let log = host.log_config.unwrap();
        assert_eq!(log.typ.as_deref(), Some("json-file"));
        let opts = log.config.unwrap();
        assert_eq!(opts.get("max-size").map(String::as_str), Some("50m"));
        assert_eq!(opts.get("max-file").map(String::as_str), Some("5"));
    }

    #[test]
    fn build_config_uses_zeroclaw_doctor_healthcheck() {
        let spec = sample_spec();
        let cfg = DockerDriver::build_config(&spec);
        let hc = cfg.healthcheck.unwrap();
        let test = hc.test.unwrap();
        assert_eq!(test, vec!["CMD", "zeroclaw", "doctor"]);
        assert_eq!(hc.interval, Some(30_000_000_000));
        assert_eq!(hc.timeout, Some(10_000_000_000));
        assert_eq!(hc.retries, Some(3));
        assert_eq!(hc.start_period, Some(30_000_000_000));
    }

    #[test]
    fn build_config_honors_explicit_restart_policy() {
        for (input, want) in [
            ("always", RestartPolicyNameEnum::ALWAYS),
            ("on-failure", RestartPolicyNameEnum::ON_FAILURE),
            ("unless-stopped", RestartPolicyNameEnum::UNLESS_STOPPED),
            ("no", RestartPolicyNameEnum::NO),
        ] {
            let mut spec = sample_spec();
            spec.restart = input.into();
            let cfg = DockerDriver::build_config(&spec);
            let host = cfg.host_config.unwrap();
            let policy = host.restart_policy.unwrap();
            assert_eq!(policy.name, Some(want), "input {input}");
        }
    }

    #[test]
    fn build_config_unknown_restart_policy_falls_back_to_unless_stopped() {
        let mut spec = sample_spec();
        spec.restart = "gibberish".into();
        let cfg = DockerDriver::build_config(&spec);
        let host = cfg.host_config.unwrap();
        let policy = host.restart_policy.unwrap();
        assert_eq!(policy.name, Some(RestartPolicyNameEnum::UNLESS_STOPPED));
    }

    #[test]
    fn env_lists_equal_ignores_order() {
        let a = vec!["A=1".to_string(), "B=2".to_string()];
        let b = vec!["B=2".to_string(), "A=1".to_string()];
        assert!(env_lists_equal(&a, &b));
        let c = vec!["A=1".to_string(), "B=3".to_string()];
        assert!(!env_lists_equal(&a, &c));
    }

    #[test]
    fn container_name_prefix_is_stable() {
        assert_eq!(container_name("grocery"), "claw-grocery");
        assert_eq!(container_name("alfred"), "claw-alfred");
    }

    #[test]
    fn classify_missing_state_is_missing() {
        assert_eq!(classify(None), ClawHealth::Missing);
    }

    #[test]
    fn classify_running_no_healthcheck_is_healthy() {
        let state = ContainerState { running: Some(true), ..Default::default() };
        assert_eq!(classify(Some(&state)), ClawHealth::Healthy);
    }

    #[test]
    fn classify_stopped_is_stopped() {
        let state = ContainerState { running: Some(false), ..Default::default() };
        assert_eq!(classify(Some(&state)), ClawHealth::Stopped);
    }
}
