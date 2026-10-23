//! Notion REST API HTTP client wrapper.
//!
//! Auth: `Authorization: Bearer <token>` header (internal integration tokens).
//! All requests target `https://api.notion.com/v1{path}` and include the
//! required `Notion-Version` header. Construct via [`NotionClient::new`] for
//! static credentials, or [`NotionClient::with_store`] to resolve
//! `skills/notion/workspaces/{workspace}/token` per request — pass a
//! `LayeredUserVault` and per-user overrides at
//! `users/{uid}/skills/notion/...` apply transparently.

use std::sync::Arc;

use eyre::{eyre, Result};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, AUTHORIZATION, CONTENT_TYPE};
use serde_json::Value;
use workforce_agent::tools::RateLimitedError;
use workforce_common::settings::SecretStore;

use super::config::NOTION_VERSION;
use crate::adapters::utils::retry_after_secs;

const BASE_URL: &str = "https://api.notion.com/v1";

/// Thin wrapper around `reqwest::Client` for the Notion REST API.
pub struct NotionClient {
    client: reqwest::Client,
    pub max_results: u64,
    static_auth: Option<HeaderValue>,
    store: Option<Arc<dyn SecretStore>>,
    workspace: String,
}

impl NotionClient {
    /// Build a client with a static integration token.
    pub fn new(token: &str, max_results: u64) -> Result<Self> {
        let auth = HeaderValue::from_str(&format!("Bearer {token}"))
            .map_err(|e| eyre!("Invalid Notion token: {e}"))?;
        Ok(Self {
            client: reqwest::Client::builder().build()?,
            max_results,
            static_auth: Some(auth),
            store: None,
            workspace: String::new(),
        })
    }

    /// Build a client that resolves its token per request from `store`.
    pub fn with_store(
        store: Arc<dyn SecretStore>,
        workspace: &str,
        max_results: u64,
    ) -> Result<Self> {
        Ok(Self {
            client: reqwest::Client::builder().build()?,
            max_results,
            static_auth: None,
            store: Some(store),
            workspace: workspace.to_string(),
        })
    }

    fn build_headers(&self) -> Result<HeaderMap> {
        let mut headers = HeaderMap::new();
        let auth = if let Some(ref h) = self.static_auth {
            h.clone()
        } else {
            let store = self
                .store
                .as_ref()
                .ok_or_else(|| eyre!("Notion client has no credential store"))?;
            let workspace_path = format!("skills/notion/workspaces/{}/token", self.workspace);
            let token = crate::adapters::auth::resolve_workspace_or_flat(
                store.as_ref(),
                &workspace_path,
                "skills/notion/token",
            )
            .ok_or_else(|| {
                eyre!("Notion token not configured at {workspace_path} or skills/notion/token")
            })?;
            HeaderValue::from_str(&format!("Bearer {token}"))
                .map_err(|e| eyre!("Invalid Notion token: {e}"))?
        };
        headers.insert(AUTHORIZATION, auth);
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        headers.insert(
            HeaderName::from_static("notion-version"),
            HeaderValue::from_static(NOTION_VERSION),
        );
        Ok(headers)
    }

    fn url(&self, path: &str) -> String {
        if path.starts_with('/') {
            format!("{}{}", BASE_URL, path)
        } else {
            format!("{}/{}", BASE_URL, path)
        }
    }

    pub async fn get(&self, path: &str, query: &[(&str, String)]) -> Result<Value> {
        let mut req = self
            .client
            .get(self.url(path))
            .headers(self.build_headers()?);
        if !query.is_empty() {
            req = req.query(query);
        }
        Self::handle_response(req.send().await?).await
    }

    pub async fn post(&self, path: &str, body: Value) -> Result<Value> {
        Self::handle_response(
            self.client
                .post(self.url(path))
                .headers(self.build_headers()?)
                .json(&body)
                .send()
                .await?,
        )
        .await
    }

    pub async fn patch(&self, path: &str, body: Value) -> Result<Value> {
        Self::handle_response(
            self.client
                .patch(self.url(path))
                .headers(self.build_headers()?)
                .json(&body)
                .send()
                .await?,
        )
        .await
    }

    pub async fn delete(&self, path: &str) -> Result<Value> {
        Self::handle_response(
            self.client
                .delete(self.url(path))
                .headers(self.build_headers()?)
                .send()
                .await?,
        )
        .await
    }

    async fn handle_response(response: reqwest::Response) -> Result<Value> {
        let status = response.status();
        let retry_after = retry_after_secs(response.headers());
        let text = response.text().await.unwrap_or_default();

        if status.is_success() {
            if text.is_empty() {
                return Ok(Value::Null);
            }
            serde_json::from_str(&text)
                .map_err(|e| eyre!("Notion returned non-JSON success body: {e}"))
        } else {
            let message = serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|v| v.get("message").and_then(|m| m.as_str().map(String::from)))
                .unwrap_or_else(|| {
                    if text.is_empty() {
                        format!("HTTP {}", status.as_u16())
                    } else {
                        text.clone()
                    }
                });

            match status.as_u16() {
                401 => Err(eyre!("Authentication failed — check Notion token: {message}")),
                403 => Err(eyre!("Forbidden (check integration capabilities and page sharing): {message}")),
                404 => Err(eyre!("Not found (ensure the page/database is shared with the integration): {message}")),
                409 => Err(eyre!("Conflict (transaction could not be completed): {message}")),
                429 => Err(RateLimitedError::new(
                    format!("Rate limited (Notion allows 3 req/sec): {message}"),
                    retry_after,
                )
                .into()),
                s if (500..600).contains(&s) => Err(eyre!("Notion server error ({s}): {message}")),
                _ => Err(eyre!("Notion API error ({status}): {message}")),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::test_support::InMem;

    #[test]
    fn url_handles_leading_slash_and_bare() {
        let c = NotionClient::new("ntn_test123", 100).unwrap();
        assert_eq!(c.url("/pages/abc"), "https://api.notion.com/v1/pages/abc");
        assert_eq!(c.url("pages/abc"), "https://api.notion.com/v1/pages/abc");
    }

    #[test]
    fn build_headers_uses_static_auth_with_notion_version() {
        let c = NotionClient::new("ntn_static_token", 100).unwrap();
        let headers = c.build_headers().unwrap();
        assert_eq!(
            headers.get(AUTHORIZATION).unwrap(),
            "Bearer ntn_static_token"
        );
        assert_eq!(headers.get("notion-version").unwrap(), NOTION_VERSION);
    }

    #[test]
    fn build_headers_falls_back_to_flat_path_when_workspace_path_missing() {
        let store = InMem::from(&[("skills/notion/token", "ntn_flat")]);
        let c = NotionClient::with_store(store, "default", 100).unwrap();
        let headers = c.build_headers().unwrap();
        assert_eq!(headers.get(AUTHORIZATION).unwrap(), "Bearer ntn_flat");
    }

    #[test]
    fn build_headers_prefers_workspace_path_over_flat() {
        let store = InMem::from(&[
            ("skills/notion/token", "ntn_flat"),
            ("skills/notion/workspaces/acme/token", "ntn_acme"),
        ]);
        let c = NotionClient::with_store(store, "acme", 100).unwrap();
        let headers = c.build_headers().unwrap();
        assert_eq!(headers.get(AUTHORIZATION).unwrap(), "Bearer ntn_acme");
    }

    #[tokio::test]
    async fn a_429_maps_to_a_typed_rate_limit_with_the_retry_after_hint() {
        let resp = reqwest::Response::from(
            http::Response::builder()
                .status(429)
                .header("retry-after", "30")
                .body(r#"{"message":"rate limited"}"#)
                .unwrap(),
        );
        let err = NotionClient::handle_response(resp).await.unwrap_err();
        let limit = err
            .downcast_ref::<RateLimitedError>()
            .expect("429 should carry the typed rate-limit signal");
        assert_eq!(limit.retry_after_secs, Some(30));
        assert_eq!(
            limit.message,
            "Rate limited (Notion allows 3 req/sec): rate limited"
        );
    }
}
