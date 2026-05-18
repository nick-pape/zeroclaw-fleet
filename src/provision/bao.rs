//! Minimal OpenBao / HashiCorp Vault HTTP client.
//!
//! The orchestrator authenticates with a long-lived service token mounted
//! in via env / docker secret. We only need:
//!   * `kv_get(path)` — read a KV-v2 secret.
//!   * `kv_put(path, fields)` — write a KV-v2 secret (idempotent).
//!   * `jwt_role_create(role, audience, policies, ttl)` — create or update
//!     a JWT auth role for a tenant's MCP scope.
//!
//! Mount paths assumed: `secret/data/<path>` (KV-v2). Override `kv_mount`
//! in [`BaoClient::with_mount`] if your deployment uses a different mount.

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Bao HTTP client. Cheap to clone (just an Arc-backed reqwest client + a
/// few strings).
#[derive(Debug, Clone)]
pub struct BaoClient {
    http: reqwest::Client,
    base_url: String,
    token: String,
    kv_mount: String,
}

impl BaoClient {
    /// Build a client for `base_url` (e.g. `https://bao.example.com`).
    /// `token` is the X-Vault-Token / X-Bao-Token header value.
    pub fn new(http: reqwest::Client, base_url: impl Into<String>, token: impl Into<String>) -> Self {
        Self {
            http,
            base_url: base_url.into().trim_end_matches('/').to_string(),
            token: token.into(),
            kv_mount: "secret".into(),
        }
    }

    /// Override the KV-v2 mount path. Defaults to `"secret"`.
    pub fn with_mount(mut self, mount: impl Into<String>) -> Self {
        self.kv_mount = mount.into();
        self
    }

    fn kv_url(&self, path: &str) -> String {
        let p = path.trim_start_matches('/');
        format!("{}/v1/{}/data/{p}", self.base_url, self.kv_mount)
    }

    /// Read a KV-v2 secret. Returns the `data.data` map (the secret fields).
    pub async fn kv_get(&self, path: &str) -> Result<BTreeMap<String, String>> {
        let url = self.kv_url(path);
        let resp = self
            .http
            .get(&url)
            .header("X-Vault-Token", &self.token)
            .send()
            .await
            .context("bao GET")?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!("bao GET {url} -> {status}: {body}"));
        }
        let env: KvV2Envelope = resp.json().await.context("bao GET parse")?;
        Ok(env.data.data)
    }

    /// Read a single field. Convenience wrapper around [`Self::kv_get`].
    pub async fn kv_get_field(&self, path: &str, field: &str) -> Result<String> {
        let map = self.kv_get(path).await?;
        map.get(field)
            .cloned()
            .ok_or_else(|| anyhow!("bao secret {path} missing field {field}"))
    }

    /// Write a KV-v2 secret. Idempotent — overwrites any existing value at
    /// the same path with a new version.
    pub async fn kv_put(&self, path: &str, fields: &BTreeMap<String, String>) -> Result<()> {
        let url = self.kv_url(path);
        let body = serde_json::json!({ "data": fields });
        let resp = self
            .http
            .post(&url)
            .header("X-Vault-Token", &self.token)
            .json(&body)
            .send()
            .await
            .context("bao POST")?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!("bao POST {url} -> {status}: {body}"));
        }
        Ok(())
    }

    /// Permanently delete a KV-v2 secret (all versions). Use the
    /// `metadata` path so the deletion is hard, not just soft.
    pub async fn kv_delete(&self, path: &str) -> Result<()> {
        let p = path.trim_start_matches('/');
        let url = format!("{}/v1/{}/metadata/{p}", self.base_url, self.kv_mount);
        let resp = self
            .http
            .delete(&url)
            .header("X-Vault-Token", &self.token)
            .send()
            .await
            .context("bao DELETE metadata")?;
        let status = resp.status();
        // 204 (no content) and 404 (already gone) both count as success.
        if status.is_success() || status.as_u16() == 404 {
            return Ok(());
        }
        let body = resp.text().await.unwrap_or_default();
        Err(anyhow!("bao DELETE {url} -> {status}: {body}"))
    }

    /// Delete a JWT auth role.
    pub async fn jwt_role_delete(&self, role: &str) -> Result<()> {
        let url = format!("{}/v1/auth/jwt/role/{role}", self.base_url);
        let resp = self
            .http
            .delete(&url)
            .header("X-Vault-Token", &self.token)
            .send()
            .await
            .context("bao DELETE jwt role")?;
        let status = resp.status();
        if status.is_success() || status.as_u16() == 404 {
            return Ok(());
        }
        let body = resp.text().await.unwrap_or_default();
        Err(anyhow!("bao DELETE {url} -> {status}: {body}"))
    }

    /// Create or update a JWT auth role for a tenant's MCP scope.
    ///
    /// Maps to `bao write auth/jwt/role/<role>`. Idempotent.
    pub async fn jwt_role_upsert(
        &self,
        role: &str,
        bound_audiences: &[&str],
        policies: &[&str],
        user_claim: &str,
        ttl_seconds: u64,
    ) -> Result<()> {
        let url = format!("{}/v1/auth/jwt/role/{role}", self.base_url);
        let body = serde_json::json!({
            "role_type": "jwt",
            "bound_audiences": bound_audiences,
            "user_claim": user_claim,
            "policies": policies,
            "ttl": format!("{}s", ttl_seconds),
        });
        let resp = self
            .http
            .post(&url)
            .header("X-Vault-Token", &self.token)
            .json(&body)
            .send()
            .await
            .context("bao jwt role POST")?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!("bao jwt role POST {url} -> {status}: {body}"));
        }
        Ok(())
    }
}

#[derive(Debug, Deserialize, Serialize)]
struct KvV2Envelope {
    data: KvV2Data,
}

#[derive(Debug, Deserialize, Serialize)]
struct KvV2Data {
    data: BTreeMap<String, String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kv_url_strips_leading_slash_and_uses_mount() {
        let c = BaoClient::new(reqwest::Client::new(), "https://b/", "tok");
        assert_eq!(c.kv_url("services/litellm/master"), "https://b/v1/secret/data/services/litellm/master");
        assert_eq!(c.kv_url("/services/foo"), "https://b/v1/secret/data/services/foo");
        let c2 = BaoClient::new(reqwest::Client::new(), "https://b", "tok").with_mount("kv");
        assert_eq!(c2.kv_url("services/x"), "https://b/v1/kv/data/services/x");
    }
}
