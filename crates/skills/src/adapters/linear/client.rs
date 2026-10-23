//! Linear GraphQL API HTTP client wrapper.

use std::sync::Arc;

use eyre::{eyre, Result};
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, CONTENT_TYPE};
use serde_json::Value;
use workforce_agent::tools::RateLimitedError;
use workforce_common::settings::SecretStore;

use crate::adapters::utils::retry_after_secs;

/// Thin wrapper around `reqwest::Client` for Linear GraphQL API.
///
/// Construct via [`LinearClient::new`] for static credentials, or
/// [`LinearClient::with_store`] to resolve
/// `skills/linear/workspaces/{workspace}/api_key` per request — pass a
/// `LayeredUserVault` and per-user overrides at
/// `users/{uid}/skills/linear/workspaces/{ws}/api_key` take effect
/// transparently.
pub struct LinearClient {
    client: reqwest::Client,
    base_url: String,
    static_auth: Option<HeaderValue>,
    store: Option<Arc<dyn SecretStore>>,
    workspace: String,
}

impl LinearClient {
    /// Create a Linear client with a static API key. Linear uses
    /// `Authorization: {api_key}` (no `Bearer` prefix).
    pub fn new(api_key: &str, base_url: &str) -> Result<Self> {
        let auth =
            HeaderValue::from_str(api_key).map_err(|e| eyre!("Invalid Linear API key: {}", e))?;
        Ok(Self {
            client: reqwest::Client::builder().build()?,
            base_url: base_url.trim_end_matches('/').to_string(),
            static_auth: Some(auth),
            store: None,
            workspace: String::new(),
        })
    }

    /// Create a Linear client that resolves its API key from `store` on
    /// every request.
    pub fn with_store(
        store: Arc<dyn SecretStore>,
        workspace: &str,
        base_url: &str,
    ) -> Result<Self> {
        Ok(Self {
            client: reqwest::Client::builder().build()?,
            base_url: base_url.trim_end_matches('/').to_string(),
            static_auth: None,
            store: Some(store),
            workspace: workspace.to_string(),
        })
    }

    fn resolve_auth(&self) -> Result<HeaderValue> {
        if let Some(ref auth) = self.static_auth {
            return Ok(auth.clone());
        }
        let store = self
            .store
            .as_ref()
            .ok_or_else(|| eyre!("Linear client has no credential store"))?;
        let path = format!("skills/linear/workspaces/{}/api_key", self.workspace);
        let api_key = crate::adapters::auth::resolve_workspace_or_flat(
            store.as_ref(),
            &path,
            "skills/linear/api_key",
        )
        .ok_or_else(|| eyre!("Linear API key not configured at {}", path))?;
        HeaderValue::from_str(&api_key).map_err(|e| eyre!("Invalid Linear API key: {}", e))
    }

    /// Execute a GraphQL query against the Linear API.
    pub async fn query(&self, query: &str, variables: Value) -> Result<Value> {
        let body = serde_json::json!({
            "query": query,
            "variables": variables
        });

        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, self.resolve_auth()?);
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));

        let response = self
            .client
            .post(&self.base_url)
            .headers(headers)
            .json(&body)
            .send()
            .await?;

        Self::handle_response(response).await
    }

    /// Execute a GraphQL mutation against the Linear API.
    pub async fn mutate(&self, mutation: &str, variables: Value) -> Result<Value> {
        // GraphQL mutations use the same endpoint as queries
        self.query(mutation, variables).await
    }

    /// Handle the HTTP response, mapping errors appropriately.
    async fn handle_response(response: reqwest::Response) -> Result<Value> {
        let status = response.status();
        let retry_after = retry_after_secs(response.headers());

        if status.is_success() {
            let body: Value = response.json().await?;

            // Linear signals throttling as a GraphQL error on a 200, marked
            // by `extensions.code == "RATELIMITED"`.
            if let Some(errors_array) = body.get("errors").and_then(Value::as_array) {
                if !errors_array.is_empty() {
                    let error_messages: Vec<String> = errors_array
                        .iter()
                        .filter_map(|e| e["message"].as_str().map(String::from))
                        .collect();
                    let message = format!("GraphQL errors: {}", error_messages.join("; "));
                    let throttled = errors_array
                        .iter()
                        .any(|e| e["extensions"]["code"].as_str() == Some("RATELIMITED"));
                    return Err(if throttled {
                        RateLimitedError::new(message, None).into()
                    } else {
                        eyre!(message)
                    });
                }
            }

            // Return the data portion if present, otherwise the full body
            Ok(body.get("data").cloned().unwrap_or(body))
        } else {
            let body = response.text().await.unwrap_or_default();
            let message = serde_json::from_str::<Value>(&body)
                .ok()
                .and_then(|v| {
                    v.get("errors")
                        .and_then(|e| e.as_array())
                        .and_then(|arr| arr.first())
                        .and_then(|e| e["message"].as_str().map(String::from))
                })
                .unwrap_or(body);

            match status.as_u16() {
                401 => Err(eyre!("Authentication failed: {}", message)),
                403 => Err(eyre!("Forbidden (rate limit or permissions): {}", message)),
                404 => Err(eyre!("Not found: {}", message)),
                429 => Err(
                    RateLimitedError::new(format!("Rate limited: {message}"), retry_after).into(),
                ),
                _ => Err(eyre!("Linear API error ({}): {}", status, message)),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_builds_with_valid_api_key() {
        let client = LinearClient::new("lin_api_test123", "https://api.linear.app/graphql");
        assert!(client.is_ok());
    }

    #[test]
    fn client_trims_trailing_slash() {
        let client = LinearClient::new("lin_api_test", "https://api.linear.app/graphql/").unwrap();
        assert_eq!(client.base_url, "https://api.linear.app/graphql");
    }

    use crate::adapters::test_support::InMem;

    #[test]
    fn resolve_auth_falls_back_to_flat_path() {
        let store = InMem::from(&[("skills/linear/api_key", "lin_flat_key")]);
        let client =
            LinearClient::with_store(store, "default", "https://api.linear.app/graphql").unwrap();
        let auth = client.resolve_auth().unwrap();
        assert_eq!(auth.to_str().unwrap(), "lin_flat_key");
    }

    #[test]
    fn resolve_auth_prefers_workspace_path_over_flat() {
        let store = InMem::from(&[
            ("skills/linear/api_key", "flat_key"),
            ("skills/linear/workspaces/acme/api_key", "workspace_key"),
        ]);
        let client =
            LinearClient::with_store(store, "acme", "https://api.linear.app/graphql").unwrap();
        let auth = client.resolve_auth().unwrap();
        assert_eq!(auth.to_str().unwrap(), "workspace_key");
    }

    #[tokio::test]
    async fn a_429_maps_to_a_typed_rate_limit_with_the_retry_after_hint() {
        let resp = reqwest::Response::from(
            http::Response::builder()
                .status(429)
                .header("retry-after", "45")
                .body(r#"{"errors":[{"message":"Too many requests"}]}"#)
                .unwrap(),
        );
        let err = LinearClient::handle_response(resp).await.unwrap_err();
        let limit = err
            .downcast_ref::<RateLimitedError>()
            .expect("429 should carry the typed rate-limit signal");
        assert_eq!(limit.message, "Rate limited: Too many requests");
        assert_eq!(limit.retry_after_secs, Some(45));
        assert_eq!(
            err.to_string(),
            "Rate limited: Too many requests (retry after 45s)"
        );
    }

    #[tokio::test]
    async fn a_ratelimited_graphql_error_on_200_maps_to_a_typed_rate_limit() {
        let resp = reqwest::Response::from(
            http::Response::builder()
                .status(200)
                .body(
                    r#"{"errors":[{"message":"Rate limit exceeded","extensions":{"code":"RATELIMITED"}}]}"#,
                )
                .unwrap(),
        );
        let err = LinearClient::handle_response(resp).await.unwrap_err();
        let limit = err
            .downcast_ref::<RateLimitedError>()
            .expect("RATELIMITED GraphQL error should carry the typed signal");
        assert_eq!(limit.message, "GraphQL errors: Rate limit exceeded");
        assert_eq!(limit.retry_after_secs, None);
    }

    #[tokio::test]
    async fn a_plain_graphql_error_on_200_stays_untyped() {
        let resp = reqwest::Response::from(
            http::Response::builder()
                .status(200)
                .body(
                    r#"{"errors":[{"message":"Field not found","extensions":{"code":"GRAPHQL_VALIDATION_FAILED"}}]}"#,
                )
                .unwrap(),
        );
        let err = LinearClient::handle_response(resp).await.unwrap_err();
        assert!(err.downcast_ref::<RateLimitedError>().is_none());
        assert_eq!(err.to_string(), "GraphQL errors: Field not found");
    }
}
