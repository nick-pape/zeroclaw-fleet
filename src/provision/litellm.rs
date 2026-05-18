//! Minimal LiteLLM admin API client — just enough to mint a virtual key
//! per tenant with a monthly budget.

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};

/// LiteLLM admin client. Holds the master key (from bao) and the base URL.
#[derive(Debug, Clone)]
pub struct LiteLlmClient {
    http: reqwest::Client,
    base_url: String,
    master_key: String,
}

impl LiteLlmClient {
    pub fn new(http: reqwest::Client, base_url: impl Into<String>, master_key: impl Into<String>) -> Self {
        Self {
            http,
            base_url: base_url.into().trim_end_matches('/').to_string(),
            master_key: master_key.into(),
        }
    }

    /// Generate a virtual key scoped to a tenant. `models` is the allowed
    /// model list (empty = all models on the server). `monthly_budget_usd`
    /// enforces a hard cap.
    pub async fn generate_key(
        &self,
        tenant: &str,
        models: &[&str],
        monthly_budget_usd: f64,
    ) -> Result<String> {
        let url = format!("{}/key/generate", self.base_url);
        let body = serde_json::json!({
            "user_id": tenant,
            "key_alias": format!("claw-{tenant}"),
            "models": models,
            "max_budget": monthly_budget_usd,
            "budget_duration": "30d",
        });
        let resp = self
            .http
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.master_key))
            .json(&body)
            .send()
            .await
            .context("litellm POST /key/generate")?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!("litellm /key/generate -> {status}: {body}"));
        }
        let payload: GenerateKeyResponse = resp.json().await.context("litellm parse")?;
        Ok(payload.key)
    }
}

#[derive(Debug, Deserialize, Serialize)]
struct GenerateKeyResponse {
    key: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_key_response_parses_minimal_payload() {
        let payload: GenerateKeyResponse =
            serde_json::from_value(serde_json::json!({"key": "sk-abc123"})).unwrap();
        assert_eq!(payload.key, "sk-abc123");
    }

    #[test]
    fn base_url_trailing_slash_normalized() {
        let c = LiteLlmClient::new(reqwest::Client::new(), "https://chat/", "sk-master");
        assert_eq!(c.base_url, "https://chat");
    }
}
