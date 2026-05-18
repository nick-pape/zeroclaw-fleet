//! Authentik admin API client — creates a per-tenant OAuth2/OIDC
//! provider + application by cloning a template provider's signing key
//! and authorization flow.

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};

/// Holds the admin API token (from bao) + base URL.
#[derive(Debug, Clone)]
pub struct AuthentikClient {
    http: reqwest::Client,
    base_url: String,
    token: String,
}

/// Provider attributes the orchestrator copies from the template provider.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ProviderTemplate {
    pub authorization_flow: Option<String>,
    pub property_mappings: Option<Vec<String>>,
    pub signing_key: Option<String>,
}

/// Output of a successful provider+application creation.
#[derive(Debug, Clone)]
pub struct OAuthProviderCreated {
    pub provider_pk: i64,
    pub application_slug: String,
    pub client_id: String,
    pub client_secret: String,
}

impl AuthentikClient {
    pub fn new(http: reqwest::Client, base_url: impl Into<String>, token: impl Into<String>) -> Self {
        Self {
            http,
            base_url: base_url.into().trim_end_matches('/').to_string(),
            token: token.into(),
        }
    }

    fn auth_header(&self) -> String {
        format!("Bearer {}", self.token)
    }

    /// Fetch the OAuth2 provider with the given slug/name so we can clone
    /// its signing_key + authorization_flow.
    pub async fn fetch_provider_template(&self, name: &str) -> Result<ProviderTemplate> {
        let url = format!(
            "{}/api/v3/providers/oauth2/?name={}",
            self.base_url,
            urlencoding(name)
        );
        let resp = self
            .http
            .get(&url)
            .header("Authorization", self.auth_header())
            .send()
            .await
            .context("authentik GET provider")?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!("authentik GET provider {url} -> {status}: {body}"));
        }
        let page: AuthentikPaginated<ProviderTemplate> =
            resp.json().await.context("authentik parse provider")?;
        page.results
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("no authentik provider named {name}"))
    }

    /// Delete a per-tenant OAuth2 provider + its bound application.
    /// Idempotent — silently succeeds on 404. Deletes the application
    /// first (parent of the binding) then the provider.
    pub async fn delete_oauth(&self, tenant: &str) -> Result<()> {
        let name = format!("mcp-{tenant}");
        // Application by slug (slug = name in our convention).
        let app_url = format!("{}/api/v3/core/applications/{name}/", self.base_url);
        let app_resp = self
            .http
            .delete(&app_url)
            .header("Authorization", self.auth_header())
            .send()
            .await
            .context("authentik DELETE application")?;
        let app_status = app_resp.status();
        if !app_status.is_success() && app_status.as_u16() != 404 {
            let body = app_resp.text().await.unwrap_or_default();
            return Err(anyhow!("authentik DELETE application {app_url} -> {app_status}: {body}"));
        }

        // Provider — fetched by name to get its pk, then deleted.
        let lookup = format!("{}/api/v3/providers/oauth2/?name={}", self.base_url, urlencoding(&name));
        let resp = self
            .http
            .get(&lookup)
            .header("Authorization", self.auth_header())
            .send()
            .await
            .context("authentik GET provider for delete")?;
        if resp.status().is_success() {
            let page: AuthentikPaginated<AuthentikOAuthProvider> = resp.json().await?;
            if let Some(p) = page.results.into_iter().next() {
                let url = format!("{}/api/v3/providers/oauth2/{}/", self.base_url, p.pk);
                let dr = self
                    .http
                    .delete(&url)
                    .header("Authorization", self.auth_header())
                    .send()
                    .await
                    .context("authentik DELETE provider")?;
                let dr_status = dr.status();
                if !dr_status.is_success() && dr_status.as_u16() != 404 {
                    let body = dr.text().await.unwrap_or_default();
                    return Err(anyhow!("authentik DELETE provider {url} -> {dr_status}: {body}"));
                }
            }
        }
        Ok(())
    }

    /// Create a per-tenant OAuth2/OIDC provider and bind a new application
    /// to it. Returns the created provider's pk + the client_secret
    /// Authentik auto-generated.
    pub async fn create_oauth(
        &self,
        tenant: &str,
        template: &ProviderTemplate,
    ) -> Result<OAuthProviderCreated> {
        let name = format!("mcp-{tenant}");

        // Provider.
        let url = format!("{}/api/v3/providers/oauth2/", self.base_url);
        let payload = serde_json::json!({
            "name": name,
            "client_type": "confidential",
            "client_id": name,
            "authorization_flow": template.authorization_flow,
            "property_mappings": template.property_mappings.as_deref().unwrap_or(&[]),
            "signing_key": template.signing_key,
            "issuer_mode": "per_provider",
            "redirect_uris": [],
        });
        let resp = self
            .http
            .post(&url)
            .header("Authorization", self.auth_header())
            .json(&payload)
            .send()
            .await
            .context("authentik POST provider")?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!("authentik POST provider -> {status}: {body}"));
        }
        let provider: AuthentikOAuthProvider =
            resp.json().await.context("authentik parse created provider")?;

        // Application binding.
        let app_url = format!("{}/api/v3/core/applications/", self.base_url);
        let app_payload = serde_json::json!({
            "name": name,
            "slug": name,
            "provider": provider.pk,
        });
        let app_resp = self
            .http
            .post(&app_url)
            .header("Authorization", self.auth_header())
            .json(&app_payload)
            .send()
            .await
            .context("authentik POST application")?;
        let app_status = app_resp.status();
        if !app_status.is_success() {
            let body = app_resp.text().await.unwrap_or_default();
            return Err(anyhow!("authentik POST application -> {app_status}: {body}"));
        }
        Ok(OAuthProviderCreated {
            provider_pk: provider.pk,
            application_slug: name.clone(),
            client_id: name,
            client_secret: provider.client_secret,
        })
    }
}

#[derive(Debug, Deserialize)]
struct AuthentikPaginated<T> {
    results: Vec<T>,
}

#[derive(Debug, Deserialize)]
struct AuthentikOAuthProvider {
    pk: i64,
    #[serde(default)]
    client_secret: String,
}

/// Minimal URL-encoder for query strings (avoids pulling in `urlencoding`
/// as a separate dep — only used for provider name lookups).
fn urlencoding(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urlencoding_round_trips_unreserved_chars() {
        assert_eq!(urlencoding("mcp-interactive"), "mcp-interactive");
        assert_eq!(urlencoding("foo bar"), "foo%20bar");
        assert_eq!(urlencoding("name+slash/"), "name%2Bslash%2F");
    }

    #[test]
    fn provider_template_deserializes_partial_payload() {
        let json = serde_json::json!({
            "authorization_flow": "abc-def",
            "signing_key": "sk-1",
        });
        let t: ProviderTemplate = serde_json::from_value(json).unwrap();
        assert_eq!(t.authorization_flow.as_deref(), Some("abc-def"));
        assert_eq!(t.signing_key.as_deref(), Some("sk-1"));
        assert!(t.property_mappings.is_none());
    }
}
