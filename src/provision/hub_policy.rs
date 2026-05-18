//! Generate the YAML snippet a human (or future automation) appends to the
//! MCP hub's `policy.yaml` to register a new tenant's identity block.
//!
//! Deliberately does NOT auto-commit to a git repo. Editing a deployment
//! policy file from a daemon container is a meaningful blast-radius
//! decision and we want that to stay explicit until we trust the
//! provisioning pipeline end-to-end. The snippet is returned for the UI
//! to surface; the operator runs the git commit themselves.

use serde::Serialize;

/// Shape of a single identity block in the hub's policy.yaml. Only the
/// fields the orchestrator generates — operators can hand-edit additional
/// fields (hitl overrides, tool_arg_overrides) after pasting.
#[derive(Debug, Serialize)]
pub struct HubIdentityBlock {
    pub name: String,
    pub aud: String,
    pub uid_base: u32,
    pub allowed_tools: Vec<String>,
    pub secrets: Vec<(String, HubSecretRef)>,
}

#[derive(Debug, Serialize)]
pub struct HubSecretRef {
    pub path: String,
    pub field: String,
}

impl HubIdentityBlock {
    /// Render as a YAML fragment ready to paste under `identities:` in
    /// the hub's `policy.yaml`. Indented to match the existing convention
    /// (2 spaces).
    pub fn to_yaml(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!("  {}:\n", self.name));
        out.push_str(&format!("    aud: {}\n", self.aud));
        out.push_str(&format!("    uid_base: {}\n", self.uid_base));
        out.push_str("    allowed_tools:\n");
        for t in &self.allowed_tools {
            out.push_str(&format!("      - {t:?}\n"));
        }
        if !self.secrets.is_empty() {
            out.push_str("    secrets:\n");
            for (scope, sref) in &self.secrets {
                out.push_str(&format!("      {scope}:\n"));
                out.push_str(&format!("        path: {}\n", sref.path));
                out.push_str(&format!("        field: {}\n", sref.field));
            }
        }
        out
    }
}

/// Build a fresh identity block for a tenant from the manifest data.
pub fn build_block(
    tenant: &str,
    uid_base: u32,
    mcp_scopes: &[String],
    secret_prefix: &str,
) -> HubIdentityBlock {
    HubIdentityBlock {
        name: tenant.to_string(),
        aud: format!("mcp-{tenant}"),
        uid_base,
        allowed_tools: mcp_scopes
            .iter()
            .map(|s| format!("{s}_*"))
            .collect(),
        secrets: mcp_scopes
            .iter()
            .map(|s| {
                (
                    s.clone(),
                    HubSecretRef {
                        path: format!("{secret_prefix}/{s}/{tenant}"),
                        field: "api_token".into(),
                    },
                )
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn yaml_indentation_matches_two_space_convention() {
        let block = build_block(
            "alpha",
            1100,
            &["heb".to_string(), "kitchenowl".to_string()],
            "services",
        );
        let yaml = block.to_yaml();
        // Top-level identity name is at 2 spaces, fields at 4 spaces.
        assert!(yaml.starts_with("  alpha:\n"));
        assert!(yaml.contains("    aud: mcp-alpha\n"));
        assert!(yaml.contains("    uid_base: 1100\n"));
        assert!(yaml.contains("      - \"heb_*\""));
        assert!(yaml.contains("      - \"kitchenowl_*\""));
        assert!(yaml.contains("      heb:\n        path: services/heb/alpha\n        field: api_token\n"));
    }

    #[test]
    fn empty_scopes_omits_secrets_section() {
        let block = build_block("alpha", 1100, &[], "services");
        let yaml = block.to_yaml();
        assert!(!yaml.contains("secrets:"));
        // allowed_tools is still emitted (empty list is valid YAML).
        assert!(yaml.contains("allowed_tools:\n"));
    }
}
