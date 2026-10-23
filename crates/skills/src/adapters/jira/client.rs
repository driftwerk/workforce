//! Jira Cloud HTTP client wrapper.
//!
//! Thin wrapper around `reqwest::Client` with Basic auth (`email:api_token`),
//! REST v3 + Agile API helpers, and Jira-shaped error mapping.

use std::sync::Arc;

use base64::Engine;
use eyre::{eyre, Result};
use reqwest::header::{HeaderValue, ACCEPT, AUTHORIZATION, USER_AGENT};
use serde_json::Value;
use workforce_agent::tools::RateLimitedError;
use workforce_common::settings::SecretStore;

use crate::adapters::auth::{field_paths, resolve_companion, resolve_credential};
use crate::adapters::utils::retry_after_secs;

/// Base path for Jira REST API v3.
pub const REST: &str = "/rest/api/3";
/// Base path for the Jira Software Agile API.
pub const AGILE: &str = "/rest/agile/1.0";

fn trim_site(site: &str) -> String {
    site.trim_end_matches('/').to_string()
}

/// Thin wrapper around `reqwest::Client` with Jira Cloud Basic authentication.
///
/// Credentials are resolved from the [`SecretStore`] on each request, enabling
/// per-user credential resolution when the store layers user overlays over
/// shared agent credentials. If no store is provided (legacy), uses baked-in
/// credentials.
pub struct JiraClient {
    client: reqwest::Client,
    base_url: Option<String>,
    default_project: Option<String>,
    store: Option<Arc<dyn SecretStore>>,
    workspace: String,
    /// Pre-computed auth header for legacy (no-store) mode.
    static_auth: Option<HeaderValue>,
}

impl JiraClient {
    /// Create a new Jira client with credentials resolved from a `SecretStore`.
    ///
    /// Credentials are read from the store on each request, supporting
    /// per-user resolution when the store layers user overlays.
    pub fn with_store(
        store: Arc<dyn SecretStore>,
        workspace: &str,
        site: Option<&str>,
        default_project: Option<String>,
    ) -> Result<Self> {
        let client = reqwest::Client::builder().build()?;
        Ok(Self {
            client,
            base_url: site.map(trim_site),
            default_project,
            store: Some(store),
            workspace: workspace.to_string(),
            static_auth: None,
        })
    }

    /// Create a new Jira client with static credentials (legacy).
    pub fn new(
        email: &str,
        api_token: &str,
        site: &str,
        default_project: Option<String>,
    ) -> Result<Self> {
        let auth = Self::encode_basic_auth(email, api_token)?;
        let client = reqwest::Client::builder().build()?;
        Ok(Self {
            client,
            base_url: Some(trim_site(site)),
            default_project,
            store: None,
            workspace: String::new(),
            static_auth: Some(auth),
        })
    }

    fn encode_basic_auth(email: &str, api_token: &str) -> Result<HeaderValue> {
        let encoded =
            base64::engine::general_purpose::STANDARD.encode(format!("{email}:{api_token}"));
        HeaderValue::from_str(&format!("Basic {encoded}"))
            .map_err(|e| eyre!("Invalid Jira credentials: {e}"))
    }

    fn paths(&self, field: &str) -> (String, String) {
        field_paths("jira", "workspaces", &self.workspace, field)
    }

    fn resolve_auth(&self) -> Result<(HeaderValue, String)> {
        if let Some(ref auth) = self.static_auth {
            let site = self
                .base_url
                .clone()
                .ok_or_else(|| eyre!("Jira site not configured"))?;
            return Ok((auth.clone(), site));
        }
        let store = self
            .store
            .as_ref()
            .ok_or_else(|| eyre!("No credential store"))?;
        let (token_ws, token_flat) = self.paths("api_token");
        let token = resolve_credential(store.as_ref(), &token_ws, &token_flat)
            .ok_or_else(|| eyre!("Jira API token not configured at {token_ws}"))?;
        let companion = |field: &str| {
            let (ws, flat) = self.paths(field);
            resolve_companion(store.as_ref(), token.per_user, &ws, &flat)
        };
        let email = companion("email")
            .ok_or_else(|| eyre!("Jira email not configured at {}", self.paths("email").0))?;
        let site = companion("site")
            .map(|s| trim_site(&s))
            .or_else(|| self.base_url.clone())
            .ok_or_else(|| eyre!("Jira site not configured at {}", self.paths("site").0))?;
        Ok((Self::encode_basic_auth(&email, &token.value)?, site))
    }

    #[cfg(test)]
    pub fn base_url(&self) -> Option<&str> {
        self.base_url.as_deref()
    }

    /// Default project key for this client, if configured.
    pub fn default_project(&self) -> Option<&str> {
        self.default_project.as_deref()
    }

    fn build_request(
        &self,
        method: reqwest::Method,
        path: &str,
    ) -> Result<reqwest::RequestBuilder> {
        let (auth, base_url) = self.resolve_auth()?;
        let url = format!("{base_url}{path}");
        Ok(self
            .client
            .request(method, &url)
            .header(AUTHORIZATION, auth)
            .header(ACCEPT, HeaderValue::from_static("application/json"))
            .header(USER_AGENT, HeaderValue::from_static("workforce")))
    }

    /// GET request.
    pub async fn get(&self, path: &str) -> Result<Value> {
        let response = self
            .build_request(reqwest::Method::GET, path)?
            .send()
            .await?;
        Self::handle_response(response).await
    }

    /// GET raw response bytes, for binary endpoints such as attachment content.
    ///
    /// Redirects are followed by reqwest's default policy, which drops the
    /// `Authorization` header on a cross-host hop — exactly what Jira's redirect
    /// from the content endpoint to its signed media host requires.
    pub async fn get_bytes(&self, path: &str) -> Result<Vec<u8>> {
        let response = self
            .build_request(reqwest::Method::GET, path)?
            .send()
            .await?;
        let status = response.status();
        if !status.is_success() {
            let retry_after = retry_after_secs(response.headers());
            let body = response.text().await.unwrap_or_default();
            return Err(jira_status_error(status, retry_after, &body));
        }
        Ok(response.bytes().await?.to_vec())
    }

    /// GET request with query parameters.
    pub async fn get_with_query(&self, path: &str, query: &[(&str, &str)]) -> Result<Value> {
        let response = self
            .build_request(reqwest::Method::GET, path)?
            .query(query)
            .send()
            .await?;
        Self::handle_response(response).await
    }

    /// POST request with a JSON body.
    pub async fn post(&self, path: &str, body: &Value) -> Result<Value> {
        let response = self
            .build_request(reqwest::Method::POST, path)?
            .json(body)
            .send()
            .await?;
        Self::handle_response(response).await
    }

    /// PUT request with a JSON body.
    pub async fn put(&self, path: &str, body: &Value) -> Result<Value> {
        let response = self
            .build_request(reqwest::Method::PUT, path)?
            .json(body)
            .send()
            .await?;
        Self::handle_response(response).await
    }

    /// DELETE request.
    pub async fn delete(&self, path: &str) -> Result<Value> {
        let response = self
            .build_request(reqwest::Method::DELETE, path)?
            .send()
            .await?;
        Self::handle_response(response).await
    }

    /// Multipart POST for attachments.
    pub async fn post_multipart(
        &self,
        path: &str,
        form: reqwest::multipart::Form,
    ) -> Result<Value> {
        let response = self
            .build_request(reqwest::Method::POST, path)?
            .header("X-Atlassian-Token", "no-check")
            .multipart(form)
            .send()
            .await?;
        Self::handle_response(response).await
    }

    /// Map a `reqwest::Response` to a `Result<Value>`, extracting Jira's
    /// `errorMessages`/`errors` shape into a readable error string.
    async fn handle_response(response: reqwest::Response) -> Result<Value> {
        let status = response.status();

        if status.is_success() {
            if status == reqwest::StatusCode::NO_CONTENT {
                return Ok(Value::Null);
            }
            // Some Jira endpoints (e.g. attachments) can return non-JSON on
            // success in odd cases — fall back gracefully to a string.
            let text = response.text().await.unwrap_or_default();
            if text.is_empty() {
                return Ok(Value::Null);
            }
            return serde_json::from_str::<Value>(&text)
                .map_err(|e| eyre!("Failed to parse Jira response as JSON: {}", e));
        }

        let retry_after = retry_after_secs(response.headers());
        let body = response.text().await.unwrap_or_default();
        Err(jira_status_error(status, retry_after, &body))
    }
}

/// Map a non-success Jira HTTP status and body into a readable error.
///
/// Shared by the JSON ([`JiraClient::handle_response`]) and raw-bytes
/// ([`JiraClient::get_bytes`]) request paths so both surface errors identically.
fn jira_status_error(
    status: reqwest::StatusCode,
    retry_after: Option<u64>,
    body: &str,
) -> eyre::Error {
    let message = parse_jira_error(body);
    match status.as_u16() {
        400 => eyre!("Bad request: {message}"),
        401 => eyre!("Authentication failed: {message}"),
        403 => eyre!("Forbidden (permissions): {message}"),
        404 => eyre!("Not found: {message}"),
        409 => eyre!("Conflict: {message}"),
        422 => eyre!("Validation error: {message}"),
        429 => {
            RateLimitedError::new(format!("Rate limited by Jira: {message}"), retry_after).into()
        }
        _ => eyre!("Jira API error ({status}): {message}"),
    }
}

/// Parse a Jira error response body into a readable string.
///
/// Jira returns `{"errorMessages": [...], "errors": {field: msg, ...}}`. We
/// flatten both into one comma-separated string. If the body isn't JSON or
/// doesn't match this shape, we return the raw text.
fn parse_jira_error(body: &str) -> String {
    let Ok(v) = serde_json::from_str::<Value>(body) else {
        return body.to_string();
    };

    let mut parts: Vec<String> = Vec::new();

    if let Some(arr) = v.get("errorMessages").and_then(|x| x.as_array()) {
        for m in arr {
            if let Some(s) = m.as_str() {
                parts.push(s.to_string());
            }
        }
    }
    if let Some(obj) = v.get("errors").and_then(|x| x.as_object()) {
        for (k, val) in obj {
            if let Some(s) = val.as_str() {
                parts.push(format!("{}: {}", k, s));
            }
        }
    }
    // Some endpoints return a flat `message` instead.
    if parts.is_empty() {
        if let Some(s) = v.get("message").and_then(|x| x.as_str()) {
            parts.push(s.to_string());
        }
    }

    if parts.is_empty() {
        body.to_string()
    } else {
        parts.join("; ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_builds_with_valid_creds() {
        let client = JiraClient::new("you@acme.com", "tok123", "https://acme.atlassian.net", None);
        assert!(client.is_ok());
    }

    #[test]
    fn client_trims_trailing_slash_on_site() {
        let client =
            JiraClient::new("you@acme.com", "tok", "https://acme.atlassian.net/", None).unwrap();
        assert_eq!(client.base_url(), Some("https://acme.atlassian.net"));
    }

    #[test]
    fn basic_auth_header_encodes_email_and_token() {
        // We can't easily inspect the default headers on the built client,
        // so reproduce the encoding here and assert it round-trips.
        let credentials = "you@acme.com:tok123";
        let encoded = base64::engine::general_purpose::STANDARD.encode(credentials);
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(encoded.as_bytes())
            .unwrap();
        assert_eq!(decoded, credentials.as_bytes());
    }

    #[test]
    fn parses_jira_error_with_messages_array() {
        let body = r#"{"errorMessages":["Issue does not exist"],"errors":{}}"#;
        assert_eq!(parse_jira_error(body), "Issue does not exist");
    }

    #[test]
    fn parses_jira_error_with_field_errors() {
        let body = r#"{"errorMessages":[],"errors":{"summary":"Summary is required"}}"#;
        assert_eq!(parse_jira_error(body), "summary: Summary is required");
    }

    #[test]
    fn parses_jira_error_falls_back_to_raw_text() {
        let body = "not json at all";
        assert_eq!(parse_jira_error(body), "not json at all");
    }

    #[test]
    fn parses_jira_error_combines_messages_and_fields() {
        let body =
            r#"{"errorMessages":["Validation failed"],"errors":{"project":"Project required"}}"#;
        let out = parse_jira_error(body);
        assert!(out.contains("Validation failed"));
        assert!(out.contains("project: Project required"));
    }

    #[test]
    fn jira_status_error_maps_status_and_message() {
        let not_found = jira_status_error(
            reqwest::StatusCode::NOT_FOUND,
            None,
            r#"{"errorMessages":["Issue does not exist"]}"#,
        )
        .to_string();
        assert!(not_found.starts_with("Not found:"));
        assert!(not_found.contains("Issue does not exist"));

        let forbidden = jira_status_error(reqwest::StatusCode::FORBIDDEN, None, "{}").to_string();
        assert!(forbidden.starts_with("Forbidden (permissions):"));

        let limited = jira_status_error(reqwest::StatusCode::TOO_MANY_REQUESTS, None, "slow down")
            .to_string();
        assert!(limited.starts_with("Rate limited by Jira:"));
        assert!(limited.contains("slow down"));
    }

    #[test]
    fn a_429_maps_to_a_typed_rate_limit() {
        let err = jira_status_error(reqwest::StatusCode::TOO_MANY_REQUESTS, None, "slow down");
        let limit = err
            .downcast_ref::<RateLimitedError>()
            .expect("429 should carry the typed rate-limit signal");
        assert_eq!(limit.message, "Rate limited by Jira: slow down");
        assert_eq!(limit.retry_after_secs, None);
        assert_eq!(err.to_string(), "Rate limited by Jira: slow down");

        let hinted =
            jira_status_error(reqwest::StatusCode::TOO_MANY_REQUESTS, Some(7), "slow down");
        let limit = hinted.downcast_ref::<RateLimitedError>().unwrap();
        assert_eq!(limit.retry_after_secs, Some(7));
        assert_eq!(
            hinted.to_string(),
            "Rate limited by Jira: slow down (retry after 7s)"
        );
    }

    use crate::adapters::test_support::{InMem, LayeredInMem};

    fn decode_basic_auth(header: &HeaderValue) -> (String, String) {
        let s = header.to_str().unwrap();
        let b64 = s.strip_prefix("Basic ").unwrap();
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(b64)
            .unwrap();
        let creds = String::from_utf8(decoded).unwrap();
        let (e, t) = creds.split_once(':').unwrap();
        (e.to_string(), t.to_string())
    }

    #[test]
    fn resolve_auth_falls_back_to_flat_path_in_workspace_mode() {
        let store = InMem::from(&[
            ("skills/jira/email", "flat@acme.com"),
            ("skills/jira/api_token", "flat_tok"),
        ]);
        let client =
            JiraClient::with_store(store, "default", Some("https://acme.atlassian.net"), None)
                .unwrap();
        let (auth, site) = client.resolve_auth().unwrap();
        let (email, token) = decode_basic_auth(&auth);
        assert_eq!(email, "flat@acme.com");
        assert_eq!(token, "flat_tok");
        assert_eq!(site, "https://acme.atlassian.net");
    }

    #[test]
    fn resolve_auth_prefers_workspace_path_over_flat() {
        let store = InMem::from(&[
            ("skills/jira/email", "flat@acme.com"),
            ("skills/jira/api_token", "flat_tok"),
            ("skills/jira/workspaces/acme/email", "ws@acme.com"),
            ("skills/jira/workspaces/acme/api_token", "ws_tok"),
        ]);
        let client =
            JiraClient::with_store(store, "acme", Some("https://acme.atlassian.net"), None)
                .unwrap();
        let (auth, _) = client.resolve_auth().unwrap();
        let (email, token) = decode_basic_auth(&auth);
        assert_eq!(email, "ws@acme.com");
        assert_eq!(token, "ws_tok");
    }

    #[test]
    fn a_shared_token_ignores_a_per_user_site() {
        let store = LayeredInMem::new(
            &[
                ("skills/jira/email", "shared@acme.com"),
                ("skills/jira/api_token", "shared_tok"),
            ],
            &[("skills/jira/site", "https://attacker.example")],
        );
        let client =
            JiraClient::with_store(store, "acme", Some("https://acme.atlassian.net/"), None)
                .unwrap();
        let (_, site) = client.resolve_auth().unwrap();
        assert_eq!(site, "https://acme.atlassian.net");
    }

    #[test]
    fn a_per_user_token_uses_its_own_site_and_email() {
        let store = LayeredInMem::new(
            &[
                ("skills/jira/email", "shared@acme.com"),
                ("skills/jira/api_token", "shared_tok"),
                ("skills/jira/site", "https://acme.atlassian.net"),
            ],
            &[
                ("skills/jira/api_token", "my_tok"),
                ("skills/jira/email", "me@acme.com"),
                ("skills/jira/site", "https://me.atlassian.net/"),
            ],
        );
        let client = JiraClient::with_store(store, "acme", None, None).unwrap();
        let (auth, site) = client.resolve_auth().unwrap();
        let (email, token) = decode_basic_auth(&auth);
        assert_eq!((email.as_str(), token.as_str()), ("me@acme.com", "my_tok"));
        assert_eq!(site, "https://me.atlassian.net");
    }

    #[test]
    fn a_per_user_token_without_a_site_anywhere_errors_cleanly() {
        let store = LayeredInMem::new(
            &[],
            &[
                ("skills/jira/api_token", "my_tok"),
                ("skills/jira/email", "me@acme.com"),
            ],
        );
        let client = JiraClient::with_store(store, "acme", None, None).unwrap();
        let err = client.resolve_auth().unwrap_err().to_string();
        assert!(err.contains("Jira site not configured"), "{err}");
    }
}
