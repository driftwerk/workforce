//! HubSpot REST API HTTP client wrapper.
//!
//! Auth: `Authorization: Bearer <token>` header (Private App access tokens).
//! All requests target `https://api.hubapi.com{path}`. Construct via
//! [`HubspotClient::new`] for static credentials, or
//! [`HubspotClient::with_store`] to resolve
//! `skills/hubspot/portals/{portal}/access_token` per request — pass a
//! `LayeredUserVault` and per-user overrides at
//! `users/{uid}/skills/hubspot/...` apply transparently.

use std::sync::Arc;

use eyre::{eyre, Result};
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, CONTENT_TYPE};
use serde_json::Value;
use workforce_agent::tools::RateLimitedError;
use workforce_common::settings::SecretStore;

pub(super) const BASE_URL: &str = "https://api.hubapi.com";

/// Thin wrapper around `reqwest::Client` for the HubSpot REST API.
pub struct HubspotClient {
    client: reqwest::Client,
    pub max_results: u64,
    static_auth: Option<HeaderValue>,
    store: Option<Arc<dyn SecretStore>>,
    portal: String,
    base_url: String,
}

impl HubspotClient {
    /// Build a new client with a static access token. HubSpot is SaaS-only —
    /// the access token determines which portal (account) is targeted.
    pub fn new(access_token: &str, max_results: u64) -> Result<Self> {
        let auth = HeaderValue::from_str(&format!("Bearer {access_token}"))
            .map_err(|e| eyre!("Invalid HubSpot access token: {e}"))?;
        Ok(Self {
            client: reqwest::Client::builder().build()?,
            max_results,
            static_auth: Some(auth),
            store: None,
            portal: String::new(),
            base_url: BASE_URL.to_string(),
        })
    }

    /// The same client against another origin — a test server.
    #[cfg(test)]
    pub fn with_base_url(mut self, base_url: &str) -> Self {
        self.base_url = base_url.trim_end_matches('/').to_string();
        self
    }

    /// Build a client that resolves its access token per request from `store`.
    pub fn with_store(store: Arc<dyn SecretStore>, portal: &str, max_results: u64) -> Result<Self> {
        Ok(Self {
            client: reqwest::Client::builder().build()?,
            max_results,
            static_auth: None,
            store: Some(store),
            portal: portal.to_string(),
            base_url: BASE_URL.to_string(),
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
                .ok_or_else(|| eyre!("HubSpot client has no credential store"))?;
            let workspace_path = format!("skills/hubspot/portals/{}/access_token", self.portal);
            let token = crate::adapters::auth::resolve_workspace_or_flat(
                store.as_ref(),
                &workspace_path,
                "skills/hubspot/access_token",
            )
            .ok_or_else(|| {
                eyre!(
                    "HubSpot access_token not configured at {workspace_path} or skills/hubspot/access_token"
                )
            })?;
            HeaderValue::from_str(&format!("Bearer {token}"))
                .map_err(|e| eyre!("Invalid HubSpot access token: {e}"))?
        };
        headers.insert(AUTHORIZATION, auth);
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        Ok(headers)
    }

    fn url(&self, path: &str) -> String {
        format!("{}/{}", self.base_url, path.trim_start_matches('/'))
    }

    /// One request: `method` on `path` with `query`, and `body` as JSON
    /// when there is one. An empty success body (a delete's 204) is `Null`.
    pub async fn send(
        &self,
        method: reqwest::Method,
        path: &str,
        query: &[(&str, String)],
        body: Option<Value>,
    ) -> Result<Value> {
        let mut req = self
            .client
            .request(method, self.url(path))
            .headers(self.build_headers()?);
        if !query.is_empty() {
            req = req.query(query);
        }
        if let Some(body) = body {
            req = req.json(&body);
        }
        Self::handle_response(req.send().await?).await
    }

    pub async fn get(&self, path: &str, query: &[(&str, String)]) -> Result<Value> {
        self.send(reqwest::Method::GET, path, query, None).await
    }

    pub async fn post(&self, path: &str, body: Value) -> Result<Value> {
        self.send(reqwest::Method::POST, path, &[], Some(body))
            .await
    }

    pub async fn patch(&self, path: &str, body: Value) -> Result<Value> {
        self.send(reqwest::Method::PATCH, path, &[], Some(body))
            .await
    }

    pub async fn put(&self, path: &str, body: Value) -> Result<Value> {
        self.send(reqwest::Method::PUT, path, &[], Some(body)).await
    }

    /// HubSpot answers a delete (an archive) with an empty 204, which is `Null`.
    pub async fn delete(&self, path: &str, query: &[(&str, String)]) -> Result<Value> {
        self.send(reqwest::Method::DELETE, path, query, None).await
    }

    async fn handle_response(response: reqwest::Response) -> Result<Value> {
        let status = response.status();
        let retry_after = crate::adapters::utils::retry_after_secs(response.headers());
        let text = response.text().await.unwrap_or_default();

        if status.is_success() {
            if text.is_empty() {
                return Ok(Value::Null);
            }
            serde_json::from_str(&text)
                .map_err(|e| eyre!("HubSpot returned non-JSON success body: {e}"))
        } else {
            let message = serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|v| {
                    v.get("message")
                        .and_then(|m| m.as_str().map(String::from))
                        .or_else(|| v.get("error").and_then(|e| e.as_str().map(String::from)))
                })
                .unwrap_or_else(|| {
                    if text.is_empty() {
                        format!("HTTP {}", status.as_u16())
                    } else {
                        text.clone()
                    }
                });

            match status.as_u16() {
                401 => Err(eyre!(
                    "Authentication failed — check HubSpot access_token: {message}"
                )),
                403 => Err(eyre!("Forbidden (check Private App scopes): {message}")),
                404 => Err(eyre!("Not found: {message}")),
                429 => Err(
                    RateLimitedError::new(format!("Rate limited: {message}"), retry_after).into(),
                ),
                s if (500..600).contains(&s) => Err(eyre!("HubSpot server error ({s}): {message}")),
                _ => Err(eyre!("HubSpot API error ({status}): {message}")),
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
        let c = HubspotClient::new("pat-test-123", 100).unwrap();
        assert_eq!(
            c.url("/crm/v3/objects/contacts"),
            "https://api.hubapi.com/crm/v3/objects/contacts"
        );
        assert_eq!(
            c.url("crm/v3/objects/contacts"),
            "https://api.hubapi.com/crm/v3/objects/contacts"
        );
        assert_eq!(
            c.with_base_url("http://127.0.0.1:1/").url("/x"),
            "http://127.0.0.1:1/x"
        );
    }

    #[test]
    fn build_headers_uses_static_auth() {
        let c = HubspotClient::new("pat-static-token", 100).unwrap();
        let headers = c.build_headers().unwrap();
        assert_eq!(
            headers.get(AUTHORIZATION).unwrap(),
            "Bearer pat-static-token"
        );
    }

    #[test]
    fn build_headers_falls_back_to_flat_path_when_portal_path_missing() {
        let store = InMem::from(&[("skills/hubspot/access_token", "pat-flat")]);
        let c = HubspotClient::with_store(store, "default", 100).unwrap();
        let headers = c.build_headers().unwrap();
        assert_eq!(headers.get(AUTHORIZATION).unwrap(), "Bearer pat-flat");
    }

    #[test]
    fn build_headers_prefers_portal_path_over_flat() {
        let store = InMem::from(&[
            ("skills/hubspot/access_token", "pat-flat"),
            ("skills/hubspot/portals/acme/access_token", "pat-acme"),
        ]);
        let c = HubspotClient::with_store(store, "acme", 100).unwrap();
        let headers = c.build_headers().unwrap();
        assert_eq!(headers.get(AUTHORIZATION).unwrap(), "Bearer pat-acme");
    }

    #[tokio::test]
    async fn a_429_maps_to_a_typed_rate_limit() {
        let resp = reqwest::Response::from(
            http::Response::builder()
                .status(429)
                .body(r#"{"message":"You have reached your limit"}"#)
                .unwrap(),
        );
        let err = HubspotClient::handle_response(resp).await.unwrap_err();
        let limit = err
            .downcast_ref::<RateLimitedError>()
            .expect("429 should carry the typed rate-limit signal");
        assert_eq!(limit.retry_after_secs, None);
        assert_eq!(limit.message, "Rate limited: You have reached your limit");
    }
}
