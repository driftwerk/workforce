//! FreeAgent v2 REST client.
//!
//! Auth is delegated to the generic [`GenericTokenStore`]: every request
//! attaches a `Bearer` access token minted from the per-workspace refresh
//! token, and a `401` forces one refresh-and-retry. List endpoints ride
//! through FreeAgent's per-minute rate window by honouring `Retry-After` on
//! a `429`. The shared `reqwest::Client` carries the mandatory `User-Agent`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use eyre::{eyre, Result, WrapErr};
use reqwest::header::ACCEPT;
use reqwest::{Method, StatusCode};
use serde_json::Value;
use workforce_agent::tools::RateLimitedError;
use workforce_common::settings::SecretStore;
use workforce_oauth_client::{OAuthClientCreds, TokenStore as GenericTokenStore};

use super::token_store::build_token_store;

/// FreeAgent's default page size and its hard upper bound.
pub const DEFAULT_PER_PAGE: u64 = 25;
pub const MAX_PER_PAGE: u64 = 100;

/// Bounded 429 handling for paged reads.
const MAX_RATE_LIMIT_RETRIES: u32 = 5;
const BACKOFF_BASE: Duration = Duration::from_millis(500);
const BACKOFF_MAX: Duration = Duration::from_secs(30);

/// Clamp a caller-supplied `per_page` into `[1, MAX_PER_PAGE]`; `None`/`0`
/// means "FreeAgent default".
pub fn clamp_per_page(requested: Option<u64>) -> u64 {
    match requested {
        None | Some(0) => DEFAULT_PER_PAGE,
        Some(n) => n.clamp(1, MAX_PER_PAGE),
    }
}

/// Clamp a caller-supplied `page` to a minimum of 1.
pub fn clamp_page(requested: Option<u64>) -> u64 {
    requested.unwrap_or(1).max(1)
}

/// Build the shared HTTP client. FreeAgent rejects requests without a
/// `User-Agent`, so it is set here and reused for both the token endpoint
/// (via the OAuth client) and data calls.
pub fn build_http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .user_agent(super::provider::USER_AGENT)
        .build()
        .wrap_err("building FreeAgent HTTP client")
}

/// One FreeAgent company's REST client.
pub struct FreeAgentClient {
    http: reqwest::Client,
    tokens: GenericTokenStore,
    api_base: String,
}

impl FreeAgentClient {
    pub fn new(
        store: Arc<dyn SecretStore>,
        workspace: &str,
        creds: OAuthClientCreds,
        http: reqwest::Client,
        api_base: String,
        sandbox: bool,
    ) -> Self {
        let tokens = build_token_store(store, workspace, creds, http.clone(), sandbox);
        Self {
            http,
            tokens,
            api_base,
        }
    }

    /// Test constructor: data calls hit `api_base` and token refreshes hit
    /// `token_endpoint`, both pointed at a mock server.
    #[cfg(test)]
    pub(crate) fn for_test(
        store: Arc<dyn SecretStore>,
        workspace: &str,
        creds: OAuthClientCreds,
        http: reqwest::Client,
        api_base: &str,
        token_endpoint: &str,
    ) -> Self {
        let provider = Arc::new(workforce_oauth_client::DiscoveredProvider::new(
            "freeagent",
            format!("{token_endpoint}/approve_app"),
            token_endpoint.to_string(),
        ));
        let tokens = super::token_store::token_store_with_provider(
            store,
            workspace,
            creds,
            http.clone(),
            provider,
        );
        Self {
            http,
            tokens,
            api_base: api_base.to_string(),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}/{}", self.api_base, path.trim_start_matches('/'))
    }

    pub async fn get(&self, path: &str, query: &[(&str, String)]) -> Result<Value> {
        self.dispatch(Method::GET, path, query, None, false).await
    }

    /// GET a list endpoint, riding through `429`s within the rate window.
    pub async fn get_paged(&self, path: &str, query: &[(&str, String)]) -> Result<Value> {
        self.dispatch(Method::GET, path, query, None, true).await
    }

    pub async fn post(&self, path: &str, body: Value) -> Result<Value> {
        self.dispatch(Method::POST, path, &[], Some(body), false)
            .await
    }

    pub async fn put(&self, path: &str, body: Value) -> Result<Value> {
        self.dispatch(Method::PUT, path, &[], Some(body), false)
            .await
    }

    pub async fn delete(&self, path: &str) -> Result<Value> {
        self.dispatch(Method::DELETE, path, &[], None, false).await
    }

    /// Send a request with a fresh access token, refreshing once on a `401`
    /// and (when `ride_rate_limit`) retrying a bounded number of times on a
    /// `429`.
    async fn dispatch(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, String)],
        body: Option<Value>,
        ride_rate_limit: bool,
    ) -> Result<Value> {
        let url = self.url(path);
        let mut refreshed = false;
        let mut rate_limit_retries = 0u32;
        loop {
            let token = self.tokens.access_token().await?;
            let mut req = self
                .http
                .request(method.clone(), &url)
                .bearer_auth(token)
                .header(ACCEPT, "application/json");
            if !query.is_empty() {
                req = req.query(query);
            }
            if let Some(ref json) = body {
                req = req.json(json);
            }
            let resp = req
                .send()
                .await
                .wrap_err_with(|| format!("{method} {url}"))?;
            let status = resp.status();
            if status == StatusCode::UNAUTHORIZED && !refreshed {
                refreshed = true;
                self.tokens.force_refresh().await.wrap_err(
                    "token refresh after 401 failed — the refresh token is likely revoked; \
                     reconnect via freeagent.setup(action='login')",
                )?;
                continue;
            }
            if ride_rate_limit
                && status == StatusCode::TOO_MANY_REQUESTS
                && rate_limit_retries < MAX_RATE_LIMIT_RETRIES
            {
                rate_limit_retries += 1;
                let wait = retry_after(&resp).unwrap_or_else(|| backoff_for(rate_limit_retries));
                tokio::time::sleep(wait).await;
                continue;
            }
            return Self::handle_response(resp).await;
        }
    }

    async fn handle_response(resp: reqwest::Response) -> Result<Value> {
        let status = resp.status();
        // The raw header value: the clamped `retry_after` paces only the
        // internal loop; the reported hint must not under-state the window.
        let retry_after_secs = crate::adapters::utils::retry_after_secs(resp.headers());
        let text = resp.text().await.unwrap_or_default();
        if status.is_success() {
            if text.is_empty() {
                return Ok(Value::Null);
            }
            return serde_json::from_str(&text)
                .map_err(|e| eyre!("FreeAgent returned non-JSON success body: {e}"));
        }
        let message = extract_error_message(&text).unwrap_or_else(|| {
            if text.is_empty() {
                format!("HTTP {}", status.as_u16())
            } else {
                text.clone()
            }
        });
        Err(match status.as_u16() {
            400 | 422 => eyre!("FreeAgent rejected the request (validation): {message}"),
            401 => {
                eyre!("FreeAgent authentication failed — reconnect via freeagent.setup: {message}")
            }
            403 => {
                eyre!("FreeAgent forbade the request — the connection lacks permission: {message}")
            }
            404 => eyre!("Not found: {message}"),
            429 => RateLimitedError::new(
                format!("FreeAgent rate limit hit — retry shortly: {message}"),
                retry_after_secs,
            )
            .into(),
            s if (500..600).contains(&s) => eyre!("FreeAgent server error ({s}): {message}"),
            s => eyre!("FreeAgent API error ({s}): {message}"),
        })
    }
}

/// Best-effort message extraction across FreeAgent's error shapes: the OAuth
/// `{"error_description"}`, the validation `{"errors": {"error": {"message"}}}`
/// and `{"errors": [{"message"}]}` envelopes, and a bare `{"error": "…"}`.
fn extract_error_message(text: &str) -> Option<String> {
    let body: Value = serde_json::from_str(text).ok()?;
    if let Some(m) = body["error_description"].as_str() {
        return Some(m.to_string());
    }
    if let Some(m) = body["errors"]["error"]["message"].as_str() {
        return Some(m.to_string());
    }
    if let Some(m) = body["errors"]
        .as_array()
        .and_then(|a| a.first())
        .and_then(|e| e["message"].as_str())
    {
        return Some(m.to_string());
    }
    body["error"]
        .as_str()
        .or_else(|| body["errors"]["message"].as_str())
        .map(str::to_string)
}

/// Parse a `Retry-After` header (integer seconds), clamped to the backoff
/// ceiling so a hostile value can't stall a list indefinitely.
fn retry_after(resp: &reqwest::Response) -> Option<Duration> {
    let secs = resp
        .headers()
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()?;
    Some(Duration::from_secs(secs).min(BACKOFF_MAX))
}

/// Exponential backoff for a `429` with no `Retry-After`: 0.5s, 1s, 2s, …
/// capped at [`BACKOFF_MAX`]. `attempt` is 1-based.
fn backoff_for(attempt: u32) -> Duration {
    let shift = attempt.saturating_sub(1).min(6);
    (BACKOFF_BASE * (1u32 << shift)).min(BACKOFF_MAX)
}

/// Holds one `FreeAgentClient` per workspace, built lazily and shared so a
/// workspace's token-refresh single-flight lock is reused across calls.
pub struct FreeAgentContext {
    store: Arc<dyn SecretStore>,
    http: reqwest::Client,
    creds: OAuthClientCreds,
    api_base: String,
    sandbox: bool,
    clients: Mutex<HashMap<String, Arc<FreeAgentClient>>>,
}

impl FreeAgentContext {
    pub fn new(
        store: Arc<dyn SecretStore>,
        http: reqwest::Client,
        creds: OAuthClientCreds,
        sandbox: bool,
    ) -> Self {
        let api_base = super::provider::FreeAgentProvider::new(sandbox)
            .api_base()
            .to_string();
        Self {
            store,
            http,
            creds,
            api_base,
            sandbox,
            clients: Mutex::new(HashMap::new()),
        }
    }

    /// Resolve (get-or-create) the client for a workspace. Per-user token
    /// resolution happens inside the client on each call, so one client per
    /// workspace name serves every user correctly.
    pub fn client(&self, workspace: &str) -> Arc<FreeAgentClient> {
        let mut map = self
            .clients
            .lock()
            .expect("freeagent client cache poisoned");
        Arc::clone(map.entry(workspace.to_string()).or_insert_with(|| {
            Arc::new(FreeAgentClient::new(
                Arc::clone(&self.store),
                workspace,
                self.creds.clone(),
                self.http.clone(),
                self.api_base.clone(),
                self.sandbox,
            ))
        }))
    }

    /// Test constructor pointing every client's data calls at `api_base`.
    #[cfg(test)]
    pub(crate) fn with_base_for_test(
        store: Arc<dyn SecretStore>,
        http: reqwest::Client,
        creds: OAuthClientCreds,
        api_base: String,
    ) -> Self {
        Self {
            store,
            http,
            creds,
            api_base,
            sandbox: false,
            clients: Mutex::new(HashMap::new()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use crate::adapters::test_support::InMem;

    fn creds() -> OAuthClientCreds {
        OAuthClientCreds {
            client_id: "cid".into(),
            client_secret: Some("secret".into()),
            redirect_uri: None,
        }
    }

    /// A store with a fresh (far-future) access token so the data path runs
    /// without contacting the token endpoint.
    fn seeded_store() -> Arc<InMem> {
        let far = (std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 100_000)
            .to_string();
        let store = InMem::from(&[
            ("skills/freeagent/workspaces/default/refresh_token", "rt"),
            (
                "skills/freeagent/workspaces/default/access_token",
                "at-fresh",
            ),
        ]);
        store
            .set("skills/freeagent/workspaces/default/expires_at", &far)
            .unwrap();
        store
    }

    fn client_with_fresh_token(server: &MockServer) -> FreeAgentClient {
        FreeAgentClient::new(
            seeded_store() as Arc<dyn SecretStore>,
            "default",
            creds(),
            build_http_client().unwrap(),
            server.uri(),
            false,
        )
    }

    #[tokio::test]
    async fn get_attaches_bearer_and_user_agent_and_parses() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/company"))
            .and(header("authorization", "Bearer at-fresh"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({ "company": { "name": "Acme" } })),
            )
            .mount(&server)
            .await;

        let client = client_with_fresh_token(&server);
        let out = client.get("/company", &[]).await.unwrap();
        assert_eq!(out["company"]["name"], "Acme");

        let reqs = server.received_requests().await.unwrap();
        assert!(reqs[0]
            .headers
            .get("user-agent")
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("workforce-freeagent/"));
    }

    #[tokio::test]
    async fn paged_get_sends_paging_and_rides_through_429() {
        let server = MockServer::start().await;
        // Retry-After: 0 keeps the test instant. The capped 429 answers the
        // first call; the 200 answers the retry.
        Mock::given(method("GET"))
            .and(path("/invoices"))
            .and(query_param("page", "2"))
            .and(query_param("per_page", "100"))
            .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "0"))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/invoices"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "invoices": [] })))
            .mount(&server)
            .await;

        let client = client_with_fresh_token(&server);
        let out = client
            .get_paged(
                "/invoices",
                &[("page", "2".to_string()), ("per_page", "100".to_string())],
            )
            .await
            .unwrap();
        assert!(out["invoices"].is_array());
    }

    #[tokio::test]
    async fn unauthorised_refreshes_and_retries_once() {
        let server = MockServer::start().await;
        // No pre-seeded access token → first call mints one from the mock
        // token endpoint, the data 401 forces a refresh, then the retry 200s.
        let store = InMem::from(&[("skills/freeagent/workspaces/default/refresh_token", "rt")]);
        Mock::given(method("POST"))
            .and(path("/token_endpoint"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token": "at-minted", "expires_in": 3600
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/company"))
            .respond_with(ResponseTemplate::new(401))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/company"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "company": {} })))
            .mount(&server)
            .await;

        let client = FreeAgentClient::for_test(
            store as Arc<dyn SecretStore>,
            "default",
            creds(),
            build_http_client().unwrap(),
            &server.uri(),
            &format!("{}/token_endpoint", server.uri()),
        );
        let out = client.get("/company", &[]).await.unwrap();
        assert!(out["company"].is_object());
    }

    #[tokio::test]
    async fn error_status_maps_to_friendly_message() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/contacts/999"))
            .respond_with(ResponseTemplate::new(404).set_body_json(json!({
                "errors": { "error": { "message": "Contact not found" } }
            })))
            .mount(&server)
            .await;
        let client = client_with_fresh_token(&server);
        let err = client.get("/contacts/999", &[]).await.unwrap_err();
        assert!(err.to_string().contains("Not found"), "{err}");
        assert!(err.to_string().contains("Contact not found"), "{err}");
    }

    #[tokio::test]
    async fn a_429_maps_to_a_typed_rate_limit_with_the_retry_after_hint() {
        let resp = reqwest::Response::from(
            http::Response::builder()
                .status(429)
                .header("retry-after", "30")
                .body(r#"{"error":"too many requests"}"#)
                .unwrap(),
        );
        let err = FreeAgentClient::handle_response(resp).await.unwrap_err();
        let limit = err
            .downcast_ref::<RateLimitedError>()
            .expect("429 should carry the typed rate-limit signal");
        assert_eq!(limit.retry_after_secs, Some(30));
        assert_eq!(
            limit.message,
            "FreeAgent rate limit hit — retry shortly: too many requests"
        );
    }

    #[test]
    fn clamp_per_page_bounds() {
        assert_eq!(clamp_per_page(None), DEFAULT_PER_PAGE);
        assert_eq!(clamp_per_page(Some(0)), DEFAULT_PER_PAGE);
        assert_eq!(clamp_per_page(Some(50)), 50);
        assert_eq!(clamp_per_page(Some(10_000)), MAX_PER_PAGE);
    }

    #[test]
    fn clamp_page_floors_at_one() {
        assert_eq!(clamp_page(None), 1);
        assert_eq!(clamp_page(Some(0)), 1);
        assert_eq!(clamp_page(Some(7)), 7);
    }

    #[test]
    fn backoff_grows_and_caps() {
        assert_eq!(backoff_for(1), BACKOFF_BASE);
        assert!(backoff_for(3) > backoff_for(1));
        assert!(backoff_for(50) <= BACKOFF_MAX);
    }

    #[test]
    fn extract_error_message_handles_freeagent_shapes() {
        assert_eq!(
            extract_error_message(r#"{"error_description":"bad token"}"#).as_deref(),
            Some("bad token")
        );
        assert_eq!(
            extract_error_message(r#"{"errors":{"error":{"message":"Date is invalid"}}}"#)
                .as_deref(),
            Some("Date is invalid")
        );
        assert_eq!(
            extract_error_message(r#"{"errors":[{"message":"line bad"}]}"#).as_deref(),
            Some("line bad")
        );
        assert_eq!(
            extract_error_message(r#"{"error":"unauthorized"}"#).as_deref(),
            Some("unauthorized")
        );
        assert!(extract_error_message("not json").is_none());
    }
}
