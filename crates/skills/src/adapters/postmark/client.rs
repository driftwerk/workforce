//! Postmark HTTP client wrapper.
//!
//! Thin wrapper around `reqwest::Client` with Postmark's two-token auth model:
//! `X-Postmark-Server-Token` for per-server operations (send, messages, stats,
//! bounces, templates, streams, suppressions) and `X-Postmark-Account-Token`
//! for account-wide resources (servers list, domains, sender signatures, data
//! removal). Tools call `server_*` or `account_*` methods by intent; the client
//! fails cleanly when an account-scoped call is made without an account token.

use eyre::{eyre, Result};
use reqwest::header::{HeaderValue, ACCEPT, CONTENT_TYPE, USER_AGENT};
use reqwest::{Method, RequestBuilder};
use serde_json::Value;
use workforce_agent::tools::RateLimitedError;

use crate::adapters::auth::{field_paths, resolve_credential, resolve_host};
use crate::adapters::utils::retry_after_secs;

/// Default Postmark API base URL. No region variants.
pub const DEFAULT_BASE_URL: &str = "https://api.postmarkapp.com";

const SERVER_TOKEN_HEADER: &str = "X-Postmark-Server-Token";
const ACCOUNT_TOKEN_HEADER: &str = "X-Postmark-Account-Token";

/// Which token to attach to the outgoing request.
#[derive(Debug, Clone, Copy)]
enum TokenKind {
    Server,
    Account,
}

/// Thin wrapper around `reqwest::Client` with Postmark authentication.
///
/// Construct via [`PostmarkClient::new`] for static tokens, or
/// [`PostmarkClient::with_store`] to resolve
/// `skills/postmark/workspaces/{workspace}/{server_token,account_token}`
/// per request.
pub struct PostmarkClient {
    client: reqwest::Client,
    base_url: String,
    static_creds: Option<StaticCreds>,
    store: Option<std::sync::Arc<dyn workforce_common::settings::SecretStore>>,
    workspace: String,
}

struct StaticCreds {
    server_token: String,
    account_token: Option<String>,
}

impl PostmarkClient {
    /// Create a new Postmark client with static tokens. `account_token`
    /// is optional — if `None`, any call to an `account_*` method returns
    /// an error with a clear remedy hint.
    pub fn new(
        server_token: &str,
        account_token: Option<&str>,
        base_url_override: Option<&str>,
    ) -> Result<Self> {
        if server_token.is_empty() {
            return Err(eyre!("Postmark server_token cannot be empty"));
        }
        // Fail at init if a token is not a valid header value, not on the
        // first request under load.
        HeaderValue::from_str(server_token)
            .map_err(|e| eyre!("Invalid Postmark server_token: {}", e))?;
        if let Some(tok) = account_token {
            HeaderValue::from_str(tok)
                .map_err(|e| eyre!("Invalid Postmark account_token: {}", e))?;
        }

        let base_url = base_url_override
            .map(|s| s.trim_end_matches('/').to_string())
            .unwrap_or_else(|| DEFAULT_BASE_URL.to_string());

        Ok(Self {
            client: reqwest::Client::builder().build()?,
            base_url,
            static_creds: Some(StaticCreds {
                server_token: server_token.to_string(),
                account_token: account_token.map(str::to_string),
            }),
            store: None,
            workspace: String::new(),
        })
    }

    /// Build a Postmark client that resolves credentials from `store` per
    /// request. The store is expected to be a `LayeredUserVault` so
    /// per-user overrides apply transparently.
    pub fn with_store(
        store: std::sync::Arc<dyn workforce_common::settings::SecretStore>,
        workspace: &str,
        base_url_override: Option<&str>,
    ) -> Result<Self> {
        let base_url = base_url_override
            .map(|s| s.trim_end_matches('/').to_string())
            .unwrap_or_else(|| DEFAULT_BASE_URL.to_string());
        Ok(Self {
            client: reqwest::Client::builder().build()?,
            base_url,
            static_creds: None,
            store: Some(store),
            workspace: workspace.to_string(),
        })
    }

    /// The configured API base URL (no trailing slash).
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Whether an account token is available — advisory only, for doc text.
    ///
    /// In `with_store` mode this returns `true` optimistically (we can't
    /// know without an active user identity); the per-request resolution
    /// surfaces a clean error if the account token isn't actually set.
    pub fn has_account_token(&self) -> bool {
        match &self.static_creds {
            Some(c) => c.account_token.is_some(),
            None => true,
        }
    }

    fn paths(&self, field: &str) -> (String, String) {
        field_paths("postmark", "workspaces", &self.workspace, field)
    }

    fn resolve_token(&self, kind: TokenKind) -> Result<(String, String)> {
        if let Some(ref creds) = self.static_creds {
            let token = match kind {
                TokenKind::Server => creds.server_token.clone(),
                TokenKind::Account => creds.account_token.clone().ok_or_else(|| {
                    eyre!(
                        "Account token not configured for this Postmark workspace. \
                         This tool requires X-Postmark-Account-Token. \
                         Add skills/postmark/workspaces/<name>/account_token (vault) or \
                         run `workforce-local-mcp config postmark` and set the account token."
                    )
                })?,
            };
            return Ok((token, self.base_url.clone()));
        }
        let store = self
            .store
            .as_ref()
            .ok_or_else(|| eyre!("Postmark client has no credential store"))?;
        let (workspace, flat) = self.paths(match kind {
            TokenKind::Server => "server_token",
            TokenKind::Account => "account_token",
        });
        let token =
            resolve_credential(store.as_ref(), &workspace, &flat).ok_or_else(|| match kind {
                TokenKind::Server => eyre!("Postmark server_token not configured at {workspace}"),
                TokenKind::Account => eyre!(
                    "Account token not configured for this Postmark workspace. \
                 Add {workspace} to the vault."
                ),
            })?;
        let (url_ws, url_flat) = self.paths("base_url");
        let host = resolve_host(store.as_ref(), token.per_user, &url_ws, &url_flat)
            .unwrap_or_else(|| self.base_url.clone());
        Ok((token.value, host))
    }

    fn build_request(&self, method: Method, path: &str, kind: TokenKind) -> Result<RequestBuilder> {
        let header_name = match kind {
            TokenKind::Server => SERVER_TOKEN_HEADER,
            TokenKind::Account => ACCOUNT_TOKEN_HEADER,
        };
        let (token, base_url) = self.resolve_token(kind)?;
        let header_value = HeaderValue::from_str(&token)
            .map_err(|e| eyre!("Invalid Postmark {header_name} value: {e}"))?;
        Ok(self
            .client
            .request(method, format!("{base_url}{path}"))
            .header(header_name, header_value)
            .header(ACCEPT, HeaderValue::from_static("application/json"))
            .header(CONTENT_TYPE, HeaderValue::from_static("application/json"))
            .header(USER_AGENT, HeaderValue::from_static("workforce")))
    }

    async fn send(&self, builder: RequestBuilder) -> Result<Value> {
        Self::handle_response(builder.send().await?).await
    }

    // ── Server-token methods ────────────────────────────────────────────────

    pub async fn server_get(&self, path: &str) -> Result<Value> {
        self.send(self.build_request(Method::GET, path, TokenKind::Server)?)
            .await
    }

    pub async fn server_get_with_query(&self, path: &str, query: &[(&str, &str)]) -> Result<Value> {
        let req = self.build_request(Method::GET, path, TokenKind::Server)?;
        self.send(req.query(query)).await
    }

    pub async fn server_post(&self, path: &str, body: &Value) -> Result<Value> {
        let req = self.build_request(Method::POST, path, TokenKind::Server)?;
        self.send(req.json(body)).await
    }

    pub async fn server_put(&self, path: &str, body: &Value) -> Result<Value> {
        let req = self.build_request(Method::PUT, path, TokenKind::Server)?;
        self.send(req.json(body)).await
    }

    pub async fn server_delete(&self, path: &str) -> Result<Value> {
        self.send(self.build_request(Method::DELETE, path, TokenKind::Server)?)
            .await
    }

    // ── Account-token methods ───────────────────────────────────────────────

    pub async fn account_get(&self, path: &str) -> Result<Value> {
        self.send(self.build_request(Method::GET, path, TokenKind::Account)?)
            .await
    }

    pub async fn account_get_with_query(
        &self,
        path: &str,
        query: &[(&str, &str)],
    ) -> Result<Value> {
        let req = self.build_request(Method::GET, path, TokenKind::Account)?;
        self.send(req.query(query)).await
    }

    pub async fn account_post(&self, path: &str, body: &Value) -> Result<Value> {
        let req = self.build_request(Method::POST, path, TokenKind::Account)?;
        self.send(req.json(body)).await
    }

    pub async fn account_put(&self, path: &str, body: &Value) -> Result<Value> {
        let req = self.build_request(Method::PUT, path, TokenKind::Account)?;
        self.send(req.json(body)).await
    }

    pub async fn account_delete(&self, path: &str) -> Result<Value> {
        self.send(self.build_request(Method::DELETE, path, TokenKind::Account)?)
            .await
    }

    /// Map a `reqwest::Response` to a `Result<Value>`, extracting Postmark's
    /// `{ErrorCode, Message}` shape into a readable error string. HTTP status
    /// is the source of truth for success/failure.
    async fn handle_response(response: reqwest::Response) -> Result<Value> {
        let status = response.status();
        let retry_after = retry_after_secs(response.headers());

        if status.is_success() {
            if status == reqwest::StatusCode::NO_CONTENT {
                return Ok(Value::Null);
            }
            let text = response.text().await.unwrap_or_default();
            if text.is_empty() {
                return Ok(Value::Null);
            }
            return serde_json::from_str::<Value>(&text)
                .map_err(|e| eyre!("Failed to parse Postmark response as JSON: {}", e));
        }

        let body = response.text().await.unwrap_or_default();
        let message = parse_postmark_error(&body);

        match status.as_u16() {
            400 => Err(eyre!("Bad request: {}", message)),
            401 => Err(eyre!("Authentication failed: {}", message)),
            403 => Err(eyre!("Forbidden (check token scope): {}", message)),
            404 => Err(eyre!("Not found: {}", message)),
            422 => Err(eyre!("Validation error: {}", message)),
            429 => Err(RateLimitedError::new(
                format!("Rate limited by Postmark: {message}"),
                retry_after,
            )
            .into()),
            s if (500..600).contains(&s) => {
                Err(eyre!("Postmark server error ({}): {}", s, message))
            }
            _ => Err(eyre!("Postmark API error ({}): {}", status, message)),
        }
    }
}

/// Parse a Postmark error body (`{"ErrorCode": <int>, "Message": "..."}`)
/// into a readable string. Falls back to the raw text for other shapes.
fn parse_postmark_error(body: &str) -> String {
    let Ok(v) = serde_json::from_str::<Value>(body) else {
        return body.to_string();
    };

    let code = v.get("ErrorCode").and_then(|c| c.as_i64());
    let message = v.get("Message").and_then(|m| m.as_str()).map(String::from);

    match (code, message) {
        (Some(c), Some(m)) => format!("[ErrorCode {c}] {m}"),
        (None, Some(m)) => m,
        (Some(c), None) => format!("[ErrorCode {c}]"),
        (None, None) => body.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_builds_with_server_token_only() {
        let c = PostmarkClient::new("pm_server_token", None, None).unwrap();
        assert_eq!(c.base_url(), DEFAULT_BASE_URL);
        assert!(!c.has_account_token());
    }

    #[test]
    fn client_builds_with_both_tokens() {
        let c = PostmarkClient::new("pm_server", Some("pm_account"), None).unwrap();
        assert!(c.has_account_token());
    }

    #[test]
    fn client_trims_trailing_slash_on_base_url() {
        let c = PostmarkClient::new("pm_server", None, Some("https://api-staging.example.com/"))
            .unwrap();
        assert_eq!(c.base_url(), "https://api-staging.example.com");
    }

    #[test]
    fn client_errors_on_empty_server_token() {
        assert!(PostmarkClient::new("", None, None).is_err());
    }

    #[test]
    fn client_errors_on_header_invalid_server_token() {
        // Characters outside ASCII-visible range are rejected by reqwest.
        assert!(PostmarkClient::new("bad\ntoken", None, None).is_err());
    }

    #[test]
    fn account_call_errors_when_token_missing() {
        let c = PostmarkClient::new("pm_server", None, None).unwrap();
        let err = c
            .build_request(Method::GET, "/servers", TokenKind::Account)
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("Account token not configured"), "{msg}");
        assert!(msg.contains("account_token"), "{msg}");
    }

    #[test]
    fn account_call_succeeds_when_token_present() {
        let c = PostmarkClient::new("pm_server", Some("pm_account"), None).unwrap();
        assert!(c
            .build_request(Method::GET, "/servers", TokenKind::Account)
            .is_ok());
    }

    #[test]
    fn parses_postmark_error_with_code_and_message() {
        let body = r#"{"ErrorCode":10,"Message":"Bad or missing API token"}"#;
        assert_eq!(
            parse_postmark_error(body),
            "[ErrorCode 10] Bad or missing API token"
        );
    }

    #[test]
    fn parses_postmark_error_with_message_only() {
        let body = r#"{"Message":"Something failed"}"#;
        assert_eq!(parse_postmark_error(body), "Something failed");
    }

    #[test]
    fn parses_postmark_error_falls_back_to_raw_text() {
        assert_eq!(parse_postmark_error("not json at all"), "not json at all");
    }

    #[test]
    fn parses_postmark_error_empty_object_falls_back() {
        assert_eq!(parse_postmark_error("{}"), "{}");
    }

    use crate::adapters::test_support::{InMem, LayeredInMem};

    #[test]
    fn resolve_token_falls_back_to_flat_paths() {
        let store = InMem::from(&[
            ("skills/postmark/server_token", "flat_server"),
            ("skills/postmark/account_token", "flat_account"),
        ]);
        let client = PostmarkClient::with_store(store, "default", None).unwrap();
        assert_eq!(
            client.resolve_token(TokenKind::Server).unwrap(),
            ("flat_server".to_string(), DEFAULT_BASE_URL.to_string())
        );
        assert_eq!(
            client.resolve_token(TokenKind::Account).unwrap().0,
            "flat_account"
        );
    }

    #[test]
    fn resolve_token_prefers_workspace_paths_over_flat() {
        let store = InMem::from(&[
            ("skills/postmark/server_token", "flat_server"),
            ("skills/postmark/account_token", "flat_account"),
            ("skills/postmark/workspaces/acme/server_token", "ws_server"),
            (
                "skills/postmark/workspaces/acme/account_token",
                "ws_account",
            ),
        ]);
        let client = PostmarkClient::with_store(store, "acme", None).unwrap();
        assert_eq!(
            client.resolve_token(TokenKind::Server).unwrap().0,
            "ws_server"
        );
        assert_eq!(
            client.resolve_token(TokenKind::Account).unwrap().0,
            "ws_account"
        );
    }

    #[test]
    fn a_shared_token_ignores_a_per_user_host() {
        let store = LayeredInMem::new(
            &[("skills/postmark/server_token", "shared_server")],
            &[("skills/postmark/base_url", "https://attacker.example")],
        );
        let client =
            PostmarkClient::with_store(store, "default", Some("https://pm.org.example")).unwrap();
        assert_eq!(
            client.resolve_token(TokenKind::Server).unwrap(),
            (
                "shared_server".to_string(),
                "https://pm.org.example".to_string()
            )
        );
    }

    #[test]
    fn a_per_user_token_may_use_its_own_host() {
        let store = LayeredInMem::new(
            &[("skills/postmark/server_token", "shared_server")],
            &[
                ("skills/postmark/server_token", "my_server"),
                ("skills/postmark/base_url", "https://pm.me.example/"),
            ],
        );
        let client = PostmarkClient::with_store(store, "default", None).unwrap();
        assert_eq!(
            client.resolve_token(TokenKind::Server).unwrap(),
            ("my_server".to_string(), "https://pm.me.example".to_string())
        );
    }

    #[tokio::test]
    async fn a_429_maps_to_a_typed_rate_limit_with_the_retry_after_hint() {
        let resp = reqwest::Response::from(
            http::Response::builder()
                .status(429)
                .header("retry-after", "10")
                .body(r#"{"ErrorCode":429,"Message":"Rate limit exceeded"}"#)
                .unwrap(),
        );
        let err = PostmarkClient::handle_response(resp).await.unwrap_err();
        let limit = err
            .downcast_ref::<RateLimitedError>()
            .expect("429 should carry the typed rate-limit signal");
        assert_eq!(
            limit.message,
            "Rate limited by Postmark: [ErrorCode 429] Rate limit exceeded"
        );
        assert_eq!(limit.retry_after_secs, Some(10));
        assert_eq!(
            err.to_string(),
            "Rate limited by Postmark: [ErrorCode 429] Rate limit exceeded (retry after 10s)"
        );
    }
}
