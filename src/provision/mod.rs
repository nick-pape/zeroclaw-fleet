//! Per-tenant provisioning. Drives Authentik, secret store, hub policy
//! snippet generation, and LiteLLM virtual key minting in one idempotent
//! sequence so that `POST /api/tenants` is a single call.
//!
//! Each step's outcome is persisted to
//! `<state_dir>/tenants/<name>/state.json` so a partially-failed create
//! can be retried with the same payload and pick up where it left off.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

pub mod authentik;
pub mod bao;
pub mod hub_policy;
pub mod litellm;

use authentik::AuthentikClient;
use bao::BaoClient;
use litellm::LiteLlmClient;

/// Inputs for a tenant provisioning run.
#[derive(Debug, Clone, Deserialize)]
pub struct TenantRequest {
    pub name: String,
    pub display_name: Option<String>,
    pub mcp_scopes: Vec<String>,
    /// Monthly LiteLLM budget. Hard cap at the LiteLLM virtual-key layer.
    #[serde(default = "default_budget")]
    pub monthly_budget_usd: f64,
    /// Allowed models (empty = all LiteLLM-fronted models).
    #[serde(default)]
    pub models: Vec<String>,
    /// uid_base for the hub policy identity block. Caller picks; the
    /// orchestrator suggests one in the UI by reading the highest
    /// existing uid_base in policy.yaml.
    pub uid_base: u32,
    /// Path prefix the hub uses for per-tenant secrets in bao
    /// (typically `"services"`).
    #[serde(default = "default_secret_prefix")]
    pub secret_prefix: String,
}

fn default_budget() -> f64 {
    100.0
}

fn default_secret_prefix() -> String {
    "services".into()
}

/// Per-step idempotency state persisted to disk.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TenantState {
    pub litellm_key_minted: bool,
    pub litellm_secret_written: bool,
    pub authentik_provider_created: Option<i64>,
    pub authentik_secret_written: bool,
    pub jwt_roles_created: Vec<String>,
    pub orchestrator_bearer_generated: bool,
}

impl TenantState {
    fn path(state_dir: &PathBuf, name: &str) -> PathBuf {
        state_dir.join("tenants").join(name).join("state.json")
    }

    fn load(state_dir: &PathBuf, name: &str) -> Self {
        let p = Self::path(state_dir, name);
        std::fs::read_to_string(&p)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn save(&self, state_dir: &PathBuf, name: &str) -> Result<()> {
        let p = Self::path(state_dir, name);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).context("create tenant state dir")?;
        }
        let tmp = p.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?).context("write tenant state")?;
        std::fs::rename(&tmp, &p).context("rename tenant state")?;
        Ok(())
    }
}

/// Output of a successful provision call. Includes the hub policy snippet
/// the operator must append + commit.
#[derive(Debug, Clone, Serialize)]
pub struct TenantProvisioned {
    pub name: String,
    pub provider_pk: i64,
    pub paired_bearer: String,
    /// YAML fragment to append under `identities:` in the hub's
    /// `policy.yaml`. The UI surfaces this; the operator commits.
    pub hub_policy_snippet: String,
    pub steps_completed: Vec<String>,
}

/// Output of a deprovision call. Includes the YAML removal snippet the
/// operator applies to the hub's `policy.yaml` + the fleet manifest.
#[derive(Debug, Clone, Serialize)]
pub struct TenantDeprovisioned {
    pub name: String,
    pub hub_policy_removal_snippet: String,
    pub fleet_manifest_removal_snippet: String,
    pub steps_completed: Vec<String>,
    pub warnings: Vec<String>,
}

/// Bundle of clients the provisioner needs.
#[derive(Clone)]
pub struct ProvisionDeps {
    pub bao: BaoClient,
    pub litellm: LiteLlmClient,
    pub authentik: AuthentikClient,
    pub authentik_template_name: Arc<str>,
    pub state_dir: PathBuf,
}

/// Run the full 7-step provisioning sequence. Idempotent: re-running with
/// the same request picks up where the previous attempt left off.
pub async fn provision(req: &TenantRequest, deps: &ProvisionDeps) -> Result<TenantProvisioned> {
    let mut state = TenantState::load(&deps.state_dir, &req.name);
    let mut steps = Vec::new();

    // 1. Mint LiteLLM virtual key.
    let virt_key = if state.litellm_key_minted {
        steps.push("litellm:already_minted".into());
        // Fetch back so we can re-persist if the secret write failed.
        deps.bao
            .kv_get_field(&format!("{}/{}/litellm", req.secret_prefix, req.name), "api_key")
            .await?
    } else {
        let models_refs: Vec<&str> = req.models.iter().map(String::as_str).collect();
        let k = deps
            .litellm
            .generate_key(&req.name, &models_refs, req.monthly_budget_usd)
            .await
            .context("litellm key mint")?;
        state.litellm_key_minted = true;
        state.save(&deps.state_dir, &req.name)?;
        steps.push("litellm:minted".into());
        k
    };

    // 2. Persist LiteLLM key to bao.
    if !state.litellm_secret_written {
        let mut fields = BTreeMap::new();
        fields.insert("api_key".into(), virt_key.clone());
        deps.bao
            .kv_put(&format!("{}/{}/litellm", req.secret_prefix, req.name), &fields)
            .await
            .context("bao kv put litellm")?;
        state.litellm_secret_written = true;
        state.save(&deps.state_dir, &req.name)?;
        steps.push("bao:litellm_written".into());
    } else {
        steps.push("bao:litellm_already_written".into());
    }

    // 3 + 4. Authentik provider + bao client_secret. Secret path is
    // `<prefix>/<tenant>/papehouse` to match the existing convention
    // used by bootstrap-agent-secrets.sh (memory: agent-durable-secrets).
    let provider_pk = if let Some(pk) = state.authentik_provider_created {
        steps.push("authentik:already_created".into());
        pk
    } else {
        let template = deps
            .authentik
            .fetch_provider_template(&deps.authentik_template_name)
            .await
            .with_context(|| {
                format!("fetch Authentik template provider {}", deps.authentik_template_name)
            })?;
        let created = deps.authentik.create_oauth(&req.name, &template).await?;
        let mut fields = BTreeMap::new();
        fields.insert("client_secret".into(), created.client_secret.clone());
        deps.bao
            .kv_put(&format!("{}/{}/papehouse", req.secret_prefix, req.name), &fields)
            .await
            .context("bao kv put auth")?;
        state.authentik_provider_created = Some(created.provider_pk);
        state.authentik_secret_written = true;
        state.save(&deps.state_dir, &req.name)?;
        steps.push("authentik:created".into());
        steps.push("bao:auth_written".into());
        created.provider_pk
    };

    // 5. Per-scope JWT roles in bao.
    let aud = format!("mcp-{}", req.name);
    for scope in &req.mcp_scopes {
        let role = format!("mcp-{scope}-{}", req.name);
        if state.jwt_roles_created.contains(&role) {
            steps.push(format!("jwt:already_created:{role}"));
            continue;
        }
        let policy = format!("mcp-{scope}-{}", req.name);
        deps.bao
            .jwt_role_upsert(&role, &[&aud], &[&policy], "sub", 3600)
            .await
            .with_context(|| format!("bao jwt role {role}"))?;
        state.jwt_roles_created.push(role.clone());
        state.save(&deps.state_dir, &req.name)?;
        steps.push(format!("jwt:created:{role}"));
    }

    // 6. Hub policy snippet (UI surfaces; operator commits).
    let snippet = hub_policy::build_block(
        &req.name,
        req.uid_base,
        &req.mcp_scopes,
        &req.secret_prefix,
    )
    .to_yaml();
    steps.push("hub_policy:snippet_generated".into());

    // 7. Orchestrator paired bearer (always regenerate if missing —
    //    matches the renderer's expectation that one exists).
    let paired = if state.orchestrator_bearer_generated {
        // Read back the existing one.
        let p = deps.state_dir.join("tenants").join(&req.name).join("bearer.txt");
        std::fs::read_to_string(&p)
            .with_context(|| format!("read existing paired bearer {}", p.display()))?
            .trim()
            .to_string()
    } else {
        let new_bearer = format!("zc_{}", uuid::Uuid::new_v4().simple());
        let p = deps.state_dir.join("tenants").join(&req.name).join("bearer.txt");
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).context("create tenant bearer dir")?;
        }
        std::fs::write(&p, &new_bearer).context("write paired bearer")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o400));
        }
        state.orchestrator_bearer_generated = true;
        state.save(&deps.state_dir, &req.name)?;
        steps.push("bearer:generated".into());
        new_bearer
    };

    Ok(TenantProvisioned {
        name: req.name.clone(),
        provider_pk,
        paired_bearer: paired,
        hub_policy_snippet: snippet,
        steps_completed: steps,
    })
}

/// Reverse the provisioning sequence. Best-effort — every step is
/// tolerant of "already absent" (404, missing state). Returns warnings
/// for any non-fatal issues so the UI can surface them.
///
/// `mcp_scopes` is the scope list to clean up (matches what `provision`
/// was given). Pass the original list or the empty vec if you don't know;
/// missing-role 404s are absorbed as warnings either way.
pub async fn deprovision(
    name: &str,
    mcp_scopes: &[String],
    secret_prefix: &str,
    deps: &ProvisionDeps,
) -> Result<TenantDeprovisioned> {
    let mut steps = Vec::new();
    let mut warnings = Vec::new();

    // 1. LiteLLM virtual key — fetch first, then call delete.
    let litellm_path = format!("{}/{}/litellm", secret_prefix, name);
    match deps.bao.kv_get_field(&litellm_path, "api_key").await {
        Ok(key) => {
            if let Err(e) = deps.litellm.delete_key(&key).await {
                warnings.push(format!("litellm delete_key: {e}"));
            } else {
                steps.push("litellm:deleted".into());
            }
        }
        Err(e) => warnings.push(format!("could not read litellm key for delete: {e}")),
    }

    // 2. Authentik OAuth provider + application.
    if let Err(e) = deps.authentik.delete_oauth(name).await {
        warnings.push(format!("authentik delete_oauth: {e}"));
    } else {
        steps.push("authentik:deleted".into());
    }

    // 3. bao secrets — litellm + papehouse.
    for sub in &["litellm", "papehouse"] {
        let p = format!("{}/{}/{}", secret_prefix, name, sub);
        match deps.bao.kv_delete(&p).await {
            Ok(()) => steps.push(format!("bao:deleted:{p}")),
            Err(e) => warnings.push(format!("bao kv_delete {p}: {e}")),
        }
    }

    // 4. bao JWT roles per scope.
    for scope in mcp_scopes {
        let role = format!("mcp-{scope}-{name}");
        match deps.bao.jwt_role_delete(&role).await {
            Ok(()) => steps.push(format!("jwt:deleted:{role}")),
            Err(e) => warnings.push(format!("jwt_role_delete {role}: {e}")),
        }
    }

    // 5. Wipe state dir for the tenant.
    let dir = deps.state_dir.join("tenants").join(name);
    if dir.exists() {
        match std::fs::remove_dir_all(&dir) {
            Ok(()) => steps.push("state:wiped".into()),
            Err(e) => warnings.push(format!("remove state dir {}: {e}", dir.display())),
        }
    } else {
        steps.push("state:already_absent".into());
    }

    Ok(TenantDeprovisioned {
        name: name.to_string(),
        hub_policy_removal_snippet: hub_policy::build_removal_snippet(name),
        fleet_manifest_removal_snippet: format!(
            "# Remove `- {name}` from fleet.yaml `claws:` list, and delete claws/{name}.toml.\n# Then commit + push.\n"
        ),
        steps_completed: steps,
        warnings,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn tenant_state_round_trips_through_disk() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().to_path_buf();
        let mut s = TenantState::default();
        s.litellm_key_minted = true;
        s.jwt_roles_created.push("mcp-heb-alpha".into());
        s.save(&dir, "alpha").unwrap();
        let loaded = TenantState::load(&dir, "alpha");
        assert!(loaded.litellm_key_minted);
        assert_eq!(loaded.jwt_roles_created, vec!["mcp-heb-alpha".to_string()]);
    }

    #[test]
    fn tenant_state_missing_file_yields_default() {
        let tmp = TempDir::new().unwrap();
        let s = TenantState::load(&tmp.path().to_path_buf(), "ghost");
        assert!(!s.litellm_key_minted);
        assert!(s.jwt_roles_created.is_empty());
    }
}
