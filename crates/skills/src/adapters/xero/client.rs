//! Xero Accounting API HTTP client.
//!
//! Auth is the OAuth 2.0 client-credentials grant used by Xero "Custom
//! Connections": POST `client_id`/`client_secret` to the identity token
//! endpoint for a 30-minute bearer token, then call the Accounting API with
//! that token plus the `Xero-tenant-id` header. Tokens and the tenant id are
//! cached; the token is refreshed lazily on expiry and once on a 401.
//!
//! Construct via [`XeroClient::new`] for static credentials, or
//! [`XeroClient::with_store`] to resolve them per request from a layered
//! [`SecretStore`].

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use eyre::{eyre, Result};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, ACCEPT, AUTHORIZATION, CONTENT_TYPE};
use reqwest::{Method, StatusCode};
use serde_json::Value;
use tokio::sync::Mutex as AsyncMutex;
use workforce_agent::tools::RateLimitedError;
use workforce_common::settings::SecretStore;

const PROD_TOKEN_URL: &str = "https://identity.xero.com/connect/token";
const PROD_CONNECTIONS_URL: &str = "https://api.xero.com/connections";
const PROD_API_BASE: &str = "https://api.xero.com/api.xro/2.0";

/// Refreshed this long before nominal expiry to avoid using a token that
/// dies mid-flight.
const EXPIRY_SLACK: Duration = Duration::from_secs(60);
/// Fallback lifetime when the token response omits `expires_in`.
const DEFAULT_TOKEN_TTL: Duration = Duration::from_secs(1800);
const TENANT_HEADER: &str = "xero-tenant-id";

/// Bulk-export 429 handling (see [`XeroClient::get_for_export`]). Confined to
/// the export path so the interactive `xero.*_list` tools keep their current
/// behaviour (a 429 surfaces as an error there).
const EXPORT_MAX_RATE_LIMIT_RETRIES: u32 = 5;
const EXPORT_BACKOFF_BASE: Duration = Duration::from_millis(500);
const EXPORT_BACKOFF_MAX: Duration = Duration::from_secs(30);

/// Where a client reads its `client_id`/`client_secret`.
enum Creds {
    Static {
        client_id: String,
        client_secret: String,
    },
    Store {
        store: Arc<dyn SecretStore>,
        entity: String,
    },
}

/// Overridable API endpoints (production by default; tests point these at a
/// mock server).
#[derive(Clone)]
struct Endpoints {
    token_url: String,
    connections_url: String,
    api_base: String,
}

impl Endpoints {
    fn production() -> Self {
        Self {
            token_url: PROD_TOKEN_URL.to_string(),
            connections_url: PROD_CONNECTIONS_URL.to_string(),
            api_base: PROD_API_BASE.to_string(),
        }
    }
}

struct CachedToken {
    value: String,
    expires_at: Instant,
}

/// Cached bearer token plus the tenant discovered for one credential set.
#[derive(Default)]
struct TokenCacheEntry {
    token: Option<CachedToken>,
    tenant_id: Option<String>,
}

/// SHA-256 over `client_id ‖ \0 ‖ client_secret` — the token-cache key. See the
/// security invariant on [`XeroClient::cache`].
type CredFingerprint = [u8; 32];

/// Thin wrapper around `reqwest::Client` for the Xero Accounting API.
pub struct XeroClient {
    http: reqwest::Client,
    creds: Creds,
    configured_tenant: Option<String>,
    endpoints: Endpoints,
    /// SECURITY INVARIANT: a cached token (and the tenant discovered with it) is
    /// only ever served to a caller whose currently-resolved credentials
    /// fingerprint to the same key. One `XeroClient` is shared across every user
    /// of an agent, so a per-instance cache would hand one user's token to the
    /// next caller regardless of identity — the cross-user leak this keying
    /// closes. Keying by `SHA-256(client_id‖secret)` is causally correct: the
    /// token is a pure function of the credentials, so shared-tier creds dedupe
    /// to one token, per-user creds get isolated tokens, and a rotated secret
    /// re-mints. The map is bounded by the number of distinct credential sets
    /// (≈ active users); entries are tiny and self-stale via the token TTL, so
    /// no eviction is required.
    ///
    /// Two locks: the outer `std::sync::Mutex` guards only the map and is never
    /// held across an `.await`; each entry's inner async mutex serialises
    /// token fetches for that one credential set (single-flight) without
    /// blocking unrelated credential sets.
    cache: Mutex<HashMap<CredFingerprint, Arc<AsyncMutex<TokenCacheEntry>>>>,
}

impl XeroClient {
    /// Build a client with static credentials.
    pub fn new(client_id: &str, client_secret: &str, tenant_id: Option<String>) -> Result<Self> {
        Self::build(
            Creds::Static {
                client_id: client_id.to_string(),
                client_secret: client_secret.to_string(),
            },
            tenant_id,
            Endpoints::production(),
        )
    }

    /// Build a client that resolves its credentials per request from `store`,
    /// reading `skills/xero/instances/{entity}/{client_id,client_secret}` with
    /// a fallback to the legacy flat `skills/xero/{client_id,client_secret}`.
    pub fn with_store(
        store: Arc<dyn SecretStore>,
        entity: &str,
        tenant_id: Option<String>,
    ) -> Result<Self> {
        Self::build(
            Creds::Store {
                store,
                entity: entity.to_string(),
            },
            tenant_id,
            Endpoints::production(),
        )
    }

    /// Construct a client pointed at a mock server (all three endpoints under
    /// one base) with static credentials. Test-only.
    #[cfg(test)]
    pub(crate) fn with_endpoints_for_test(
        client_id: &str,
        client_secret: &str,
        tenant_id: Option<String>,
        base: &str,
    ) -> Self {
        Self::build(
            Creds::Static {
                client_id: client_id.to_string(),
                client_secret: client_secret.to_string(),
            },
            tenant_id,
            Endpoints {
                token_url: format!("{base}/connect/token"),
                connections_url: format!("{base}/connections"),
                api_base: format!("{base}/api.xro/2.0"),
            },
        )
        .expect("test client builds")
    }

    /// Construct a client pointed at a mock server that resolves credentials
    /// per request from `store` (the production path). Test-only; lets tests
    /// vary the resolved credentials between calls to exercise cache keying.
    #[cfg(test)]
    pub(crate) fn with_store_and_endpoints_for_test(
        store: Arc<dyn SecretStore>,
        entity: &str,
        tenant_id: Option<String>,
        base: &str,
    ) -> Self {
        Self::build(
            Creds::Store {
                store,
                entity: entity.to_string(),
            },
            tenant_id,
            Endpoints {
                token_url: format!("{base}/connect/token"),
                connections_url: format!("{base}/connections"),
                api_base: format!("{base}/api.xro/2.0"),
            },
        )
        .expect("test client builds")
    }

    fn build(
        creds: Creds,
        configured_tenant: Option<String>,
        endpoints: Endpoints,
    ) -> Result<Self> {
        Ok(Self {
            http: reqwest::Client::builder().build()?,
            creds,
            configured_tenant,
            endpoints,
            cache: Mutex::new(HashMap::new()),
        })
    }

    fn resolve_creds(&self) -> Result<(String, String)> {
        match &self.creds {
            Creds::Static {
                client_id,
                client_secret,
            } => Ok((client_id.clone(), client_secret.clone())),
            Creds::Store { store, entity } => {
                let field = |f: &str| {
                    crate::adapters::auth::resolve_workspace_or_flat(
                        store.as_ref(),
                        &format!("skills/xero/instances/{entity}/{f}"),
                        &format!("skills/xero/{f}"),
                    )
                };
                let client_id = field("client_id")
                    .ok_or_else(|| eyre!("Xero client_id not configured for entity '{entity}'"))?;
                let client_secret = field("client_secret").ok_or_else(|| {
                    eyre!("Xero client_secret not configured for entity '{entity}'")
                })?;
                Ok((client_id, client_secret))
            }
        }
    }

    /// Return a valid bearer token and the resolved tenant id, refreshing the
    /// token and discovering the tenant as needed.
    ///
    /// Credentials are resolved on **every** call, before any cache access —
    /// half the cross-user leak fix: a caller whose identity resolves no
    /// credentials errors here and never reaches a cached token. The other half
    /// is keying the cache by the credential fingerprint (see
    /// [`XeroClient::cache`]), so a caller cannot ride a token minted from a
    /// different credential set. Per-fingerprint serialisation fetches the token
    /// at most once per expiry window without blocking unrelated credentials.
    async fn auth(&self) -> Result<(String, String)> {
        let (client_id, client_secret) = self.resolve_creds()?;
        let entry = self.cache_entry(&client_id, &client_secret);

        // Single-flight per credential set; the inner lock is the only one held
        // across network I/O.
        let mut cached = entry.lock().await;

        let bearer = match &cached.token {
            Some(t) if Instant::now() + EXPIRY_SLACK < t.expires_at => t.value.clone(),
            _ => {
                let (value, ttl) = self.fetch_token(&client_id, &client_secret).await?;
                cached.token = Some(CachedToken {
                    value: value.clone(),
                    expires_at: Instant::now() + ttl,
                });
                value
            }
        };

        let tenant = match &cached.tenant_id {
            Some(t) => t.clone(),
            None => {
                let tenant = match &self.configured_tenant {
                    Some(t) => t.clone(),
                    None => self.discover_tenant(&bearer).await?,
                };
                cached.tenant_id = Some(tenant.clone());
                tenant
            }
        };

        Ok((bearer, tenant))
    }

    /// Get-or-create the cache entry for one credential set, releasing the map
    /// lock before returning so unrelated credential sets never serialise
    /// against each other during a token fetch.
    fn cache_entry(
        &self,
        client_id: &str,
        client_secret: &str,
    ) -> Arc<AsyncMutex<TokenCacheEntry>> {
        let fingerprint = credential_fingerprint(client_id, client_secret);
        let mut map = self.cache.lock().expect("xero token cache mutex poisoned");
        Arc::clone(
            map.entry(fingerprint)
                .or_insert_with(|| Arc::new(AsyncMutex::new(TokenCacheEntry::default()))),
        )
    }

    /// Drop the cached token for the caller's current credentials so the next
    /// request re-mints it. Used after a 401 (token revoked or raced past
    /// expiry). The tenant is left intact — only the bearer is suspect. Resolves
    /// creds again so the same fingerprint (and thus the same entry) is cleared.
    async fn invalidate_token(&self) -> Result<()> {
        let (client_id, client_secret) = self.resolve_creds()?;
        let fingerprint = credential_fingerprint(&client_id, &client_secret);
        let entry = self
            .cache
            .lock()
            .expect("xero token cache mutex poisoned")
            .get(&fingerprint)
            .cloned();
        if let Some(entry) = entry {
            entry.lock().await.token = None;
        }
        Ok(())
    }

    async fn fetch_token(
        &self,
        client_id: &str,
        client_secret: &str,
    ) -> Result<(String, Duration)> {
        let resp = self
            .http
            .post(&self.endpoints.token_url)
            .basic_auth(client_id, Some(client_secret))
            .form(&[("grant_type", "client_credentials")])
            .send()
            .await?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(eyre!(
                "Xero token request failed (HTTP {}): {}",
                status.as_u16(),
                redact(&body)
            ));
        }
        let body: Value = resp.json().await?;
        let access_token = body["access_token"]
            .as_str()
            .ok_or_else(|| eyre!("Xero token response missing access_token"))?
            .to_string();
        let ttl = body["expires_in"]
            .as_u64()
            .map(Duration::from_secs)
            .unwrap_or(DEFAULT_TOKEN_TTL);
        Ok((access_token, ttl))
    }

    async fn discover_tenant(&self, bearer: &str) -> Result<String> {
        let resp = self
            .http
            .get(&self.endpoints.connections_url)
            .bearer_auth(bearer)
            .header(ACCEPT, "application/json")
            .send()
            .await?;
        if !resp.status().is_success() {
            let status = resp.status();
            return Err(eyre!("Xero /connections failed (HTTP {})", status.as_u16()));
        }
        let connections: Value = resp.json().await?;
        let tenant_ids: Vec<String> = connections
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|c| c["tenantId"].as_str().map(str::to_string))
            .collect();
        match tenant_ids.as_slice() {
            [single] => Ok(single.clone()),
            [] => Err(eyre!(
                "Xero connection has no tenants — check the custom connection is authorised"
            )),
            many => Err(eyre!(
                "Xero connection spans multiple tenants ({}); set tenant_id explicitly",
                many.join(", ")
            )),
        }
    }

    fn url(&self, path: &str) -> String {
        format!(
            "{}/{}",
            self.endpoints.api_base,
            path.trim_start_matches('/')
        )
    }

    fn headers(&self, bearer: &str, tenant: &str) -> Result<HeaderMap> {
        let mut headers = HeaderMap::new();
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {bearer}"))
                .map_err(|_| eyre!("invalid bearer token"))?,
        );
        headers.insert(
            HeaderName::from_static(TENANT_HEADER),
            HeaderValue::from_str(tenant).map_err(|_| eyre!("invalid tenant id"))?,
        );
        headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        Ok(headers)
    }

    pub async fn get(&self, path: &str, query: &[(&str, String)]) -> Result<Value> {
        self.send(Method::GET, path, query, None).await
    }

    pub async fn post(&self, path: &str, body: Value) -> Result<Value> {
        self.send(Method::POST, path, &[], Some(body)).await
    }

    pub async fn put(&self, path: &str, body: Value) -> Result<Value> {
        self.send(Method::PUT, path, &[], Some(body)).await
    }

    /// GET one page for the bulk-export path. Behaves like [`get`], but on a
    /// 429 it sleeps for the `Retry-After` interval (or a bounded exponential
    /// backoff when the header is absent) and retries, up to
    /// [`EXPORT_MAX_RATE_LIMIT_RETRIES`] times, so a large export rides through
    /// Xero's per-minute rate window instead of failing. 401 still triggers the
    /// one-shot token refresh. Callers fan these out with bounded concurrency
    /// (`buffer_unordered`) so credentials still resolve per request under the
    /// caller's identity — this method does no spawning of its own.
    pub async fn get_for_export(&self, path: &str, query: &[(&str, String)]) -> Result<Value> {
        let url = self.url(path);
        let mut attempted_refresh = false;
        let mut rate_limit_retries = 0u32;
        loop {
            let (bearer, tenant) = self.auth().await?;
            let resp = self
                .http
                .request(Method::GET, &url)
                .headers(self.headers(&bearer, &tenant)?)
                .query(query)
                .send()
                .await?;
            let status = resp.status();
            if status == StatusCode::UNAUTHORIZED && !attempted_refresh {
                attempted_refresh = true;
                self.invalidate_token().await?;
                continue;
            }
            if status == StatusCode::TOO_MANY_REQUESTS
                && rate_limit_retries < EXPORT_MAX_RATE_LIMIT_RETRIES
            {
                rate_limit_retries += 1;
                let wait = retry_after(&resp).unwrap_or_else(|| backoff_for(rate_limit_retries));
                tokio::time::sleep(wait).await;
                continue;
            }
            return Self::handle_response(resp).await;
        }
    }

    /// Send a request, refreshing the token and retrying exactly once if the
    /// API rejects it with a 401 (token revoked or raced past expiry).
    async fn send(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, String)],
        body: Option<Value>,
    ) -> Result<Value> {
        let url = self.url(path);
        let mut attempted_refresh = false;
        loop {
            let (bearer, tenant) = self.auth().await?;
            let mut req = self
                .http
                .request(method.clone(), &url)
                .headers(self.headers(&bearer, &tenant)?);
            if !query.is_empty() {
                req = req.query(query);
            }
            if let Some(ref json) = body {
                req = req.json(json);
            }
            let resp = req.send().await?;
            if resp.status() == StatusCode::UNAUTHORIZED && !attempted_refresh {
                attempted_refresh = true;
                self.invalidate_token().await?;
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
                .map_err(|e| eyre!("Xero returned non-JSON success body: {e}"));
        }
        let message = extract_error_message(&text).unwrap_or_else(|| {
            if text.is_empty() {
                format!("HTTP {}", status.as_u16())
            } else {
                text.clone()
            }
        });
        Err(match status.as_u16() {
            400 => eyre!("Xero rejected the request (validation): {message}"),
            401 => eyre!("Xero authentication failed — check client credentials: {message}"),
            403 => eyre!(
                "Xero forbade the request — the connection lacks the required scope: {message}"
            ),
            404 => eyre!("Not found: {message}"),
            429 => RateLimitedError::new(
                format!("Xero rate limit hit — retry shortly: {message}"),
                retry_after_secs,
            )
            .into(),
            s if (500..600).contains(&s) => eyre!("Xero server error ({s}): {message}"),
            s => eyre!("Xero API error ({s}): {message}"),
        })
    }
}

/// Best-effort message extraction across Xero's error body shapes
/// (`problem+json`, validation envelopes, and the legacy `Message` field).
fn extract_error_message(text: &str) -> Option<String> {
    let body: Value = serde_json::from_str(text).ok()?;
    let direct = ["Detail", "Message", "Title", "message"]
        .iter()
        .find_map(|k| body.get(k).and_then(Value::as_str));
    if let Some(m) = direct {
        return Some(m.to_string());
    }
    body["Elements"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|e| e["ValidationErrors"].as_array())
        .flatten()
        .filter_map(|v| v["Message"].as_str())
        .next()
        .map(str::to_string)
}

fn redact(input: &str) -> String {
    workforce_common::redact::redact_secrets(input)
}

/// Parse a 429 `Retry-After` header (integer seconds), clamped to the backoff
/// ceiling so a hostile or buggy value can't stall an export indefinitely.
fn retry_after(resp: &reqwest::Response) -> Option<Duration> {
    let secs = resp
        .headers()
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()?;
    Some(Duration::from_secs(secs).min(EXPORT_BACKOFF_MAX))
}

/// Exponential backoff for a 429 with no `Retry-After`: 0.5s, 1s, 2s, … capped
/// at [`EXPORT_BACKOFF_MAX`]. `attempt` is 1-based.
fn backoff_for(attempt: u32) -> Duration {
    let shift = attempt.saturating_sub(1).min(6);
    (EXPORT_BACKOFF_BASE * (1u32 << shift)).min(EXPORT_BACKOFF_MAX)
}

/// Fingerprint a credential set for use as the token-cache key.
///
/// `SHA-256(client_id ‖ 0x00 ‖ client_secret)`. The NUL separator is a domain
/// boundary so that, e.g., `(id="ab", secret="c")` and `(id="a", secret="bc")`
/// never collide. Including the secret means a rotated secret yields a fresh
/// key and re-mints the token; the digest (not the raw secret) is what lives in
/// the map and it is never logged.
fn credential_fingerprint(client_id: &str, client_secret: &str) -> CredFingerprint {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(client_id.as_bytes());
    hasher.update([0u8]);
    hasher.update(client_secret.as_bytes());
    hasher.finalize().into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn client_for(server: &MockServer, tenant_id: Option<String>) -> XeroClient {
        XeroClient::with_endpoints_for_test("cid", "secret", tenant_id, &server.uri())
    }

    fn token_mock(expires_in: u64) -> Mock {
        Mock::given(method("POST"))
            .and(path("/connect/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token": "tok-abc",
                "token_type": "Bearer",
                "expires_in": expires_in,
            })))
    }

    fn connections_mock(tenant_ids: &[&str]) -> Mock {
        let body: Vec<Value> = tenant_ids
            .iter()
            .map(|t| json!({ "tenantId": t, "tenantType": "ORGANISATION" }))
            .collect();
        Mock::given(method("GET"))
            .and(path("/connections"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
    }

    #[tokio::test]
    async fn fetches_token_and_tenant_then_sends_with_headers() {
        let server = MockServer::start().await;
        token_mock(1800).mount(&server).await;
        connections_mock(&["TENANT-1"]).mount(&server).await;
        Mock::given(method("GET"))
            .and(path("/api.xro/2.0/Organisation"))
            .and(header("authorization", "Bearer tok-abc"))
            .and(header(TENANT_HEADER, "TENANT-1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "Organisations": [] })))
            .mount(&server)
            .await;

        let client = client_for(&server, None);
        let out = client.get("/Organisation", &[]).await.unwrap();
        assert!(out["Organisations"].is_array());
    }

    #[tokio::test]
    async fn reuses_cached_token_across_calls() {
        let server = MockServer::start().await;
        token_mock(1800).expect(1).mount(&server).await;
        connections_mock(&["T"]).mount(&server).await;
        Mock::given(method("GET"))
            .and(path("/api.xro/2.0/Organisation"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
            .mount(&server)
            .await;

        let client = client_for(&server, None);
        client.get("/Organisation", &[]).await.unwrap();
        client.get("/Organisation", &[]).await.unwrap();
        // `token_mock.expect(1)` is asserted on drop: a second fetch would fail.
    }

    #[tokio::test]
    async fn expired_token_triggers_refetch() {
        let server = MockServer::start().await;
        // expires_in below the slack window → always considered expired.
        token_mock(0).expect(2).mount(&server).await;
        connections_mock(&["T"]).mount(&server).await;
        Mock::given(method("GET"))
            .and(path("/api.xro/2.0/Organisation"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
            .mount(&server)
            .await;

        let client = client_for(&server, None);
        client.get("/Organisation", &[]).await.unwrap();
        client.get("/Organisation", &[]).await.unwrap();
    }

    #[tokio::test]
    async fn unauthorised_invalidates_and_retries_once() {
        let server = MockServer::start().await;
        token_mock(1800).expect(2).mount(&server).await;
        connections_mock(&["T"]).mount(&server).await;
        // wiremock matches in mount order: the 401 (mounted first, capped at one
        // hit) answers the first call, then the 200 answers the retry.
        Mock::given(method("GET"))
            .and(path("/api.xro/2.0/Invoices"))
            .respond_with(ResponseTemplate::new(401))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api.xro/2.0/Invoices"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "Invoices": [] })))
            .mount(&server)
            .await;

        let client = client_for(&server, None);
        let out = client.get("/Invoices", &[]).await.unwrap();
        assert!(out["Invoices"].is_array());
    }

    #[tokio::test]
    async fn concurrent_calls_fetch_token_once() {
        let server = MockServer::start().await;
        token_mock(1800).expect(1).mount(&server).await;
        connections_mock(&["T"]).mount(&server).await;
        Mock::given(method("GET"))
            .and(path("/api.xro/2.0/Organisation"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
            .mount(&server)
            .await;

        let client = Arc::new(client_for(&server, None));
        let a = {
            let c = Arc::clone(&client);
            tokio::spawn(async move { c.get("/Organisation", &[]).await })
        };
        let b = {
            let c = Arc::clone(&client);
            tokio::spawn(async move { c.get("/Organisation", &[]).await })
        };
        a.await.unwrap().unwrap();
        b.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn export_get_retries_after_429() {
        let server = MockServer::start().await;
        token_mock(1800).mount(&server).await;
        connections_mock(&["T"]).mount(&server).await;
        // Retry-After: 0 keeps the test instant. The capped 429 (mounted first)
        // answers the first call; the 200 answers the retry.
        Mock::given(method("GET"))
            .and(path("/api.xro/2.0/Invoices"))
            .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "0"))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api.xro/2.0/Invoices"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "Invoices": [] })))
            .mount(&server)
            .await;

        let client = client_for(&server, None);
        let out = client
            .get_for_export("/Invoices", &[("page", "1".to_string())])
            .await
            .unwrap();
        assert!(out["Invoices"].is_array());
    }

    #[tokio::test]
    async fn export_get_gives_up_after_max_429() {
        let server = MockServer::start().await;
        token_mock(1800).mount(&server).await;
        connections_mock(&["T"]).mount(&server).await;
        Mock::given(method("GET"))
            .and(path("/api.xro/2.0/Invoices"))
            .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "0"))
            .mount(&server)
            .await;

        let client = client_for(&server, None);
        let err = client
            .get_for_export("/Invoices", &[("page", "1".to_string())])
            .await
            .unwrap_err();
        assert!(err.to_string().contains("rate limit"), "{err}");
        let limit = err
            .downcast_ref::<RateLimitedError>()
            .expect("exhausted retries should surface the typed rate limit");
        assert_eq!(limit.retry_after_secs, Some(0));
    }

    #[tokio::test]
    async fn a_429_maps_to_a_typed_rate_limit_with_the_retry_after_hint() {
        let resp = reqwest::Response::from(
            http::Response::builder()
                .status(429)
                .header("retry-after", "30")
                .body(r#"{"Detail":"Rate limit exceeded"}"#)
                .unwrap(),
        );
        let err = XeroClient::handle_response(resp).await.unwrap_err();
        let limit = err
            .downcast_ref::<RateLimitedError>()
            .expect("429 should carry the typed rate-limit signal");
        assert_eq!(limit.retry_after_secs, Some(30));
        assert_eq!(
            limit.message,
            "Xero rate limit hit — retry shortly: Rate limit exceeded"
        );
    }

    #[test]
    fn backoff_grows_and_caps() {
        assert_eq!(backoff_for(1), EXPORT_BACKOFF_BASE);
        assert!(backoff_for(3) > backoff_for(1));
        assert!(backoff_for(50) <= EXPORT_BACKOFF_MAX);
    }

    #[tokio::test]
    async fn configured_tenant_skips_connections_lookup() {
        let server = MockServer::start().await;
        token_mock(1800).mount(&server).await;
        // No /connections mock — must not be called.
        Mock::given(method("GET"))
            .and(path("/api.xro/2.0/Organisation"))
            .and(header(TENANT_HEADER, "FIXED"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
            .mount(&server)
            .await;

        let client = client_for(&server, Some("FIXED".into()));
        client.get("/Organisation", &[]).await.unwrap();
    }

    #[tokio::test]
    async fn multiple_tenants_without_pin_errors() {
        let server = MockServer::start().await;
        token_mock(1800).mount(&server).await;
        connections_mock(&["T1", "T2"]).mount(&server).await;

        let client = client_for(&server, None);
        let err = client.get("/Organisation", &[]).await.unwrap_err();
        assert!(err.to_string().contains("multiple tenants"), "{err}");
    }

    #[tokio::test]
    async fn forbidden_maps_to_scope_message() {
        let server = MockServer::start().await;
        token_mock(1800).mount(&server).await;
        connections_mock(&["T"]).mount(&server).await;
        Mock::given(method("GET"))
            .and(path("/api.xro/2.0/Reports/TrialBalance"))
            .respond_with(ResponseTemplate::new(403).set_body_json(json!({
                "Detail": "AuthorizationUnsuccessful"
            })))
            .mount(&server)
            .await;

        let client = client_for(&server, None);
        let err = client.get("/Reports/TrialBalance", &[]).await.unwrap_err();
        assert!(err.to_string().contains("scope"), "{err}");
    }

    #[test]
    fn extract_error_message_handles_shapes() {
        assert_eq!(
            extract_error_message(r#"{"Detail":"bad"}"#).as_deref(),
            Some("bad")
        );
        assert_eq!(
            extract_error_message(r#"{"Message":"legacy"}"#).as_deref(),
            Some("legacy")
        );
        assert_eq!(
            extract_error_message(
                r#"{"Elements":[{"ValidationErrors":[{"Message":"line invalid"}]}]}"#
            )
            .as_deref(),
            Some("line invalid")
        );
        assert!(extract_error_message("not json").is_none());
    }

    // ── Cross-user token-cache isolation (the 2026-06-02 leak regression) ──

    /// A `SecretStore` whose resolved credentials can be swapped between calls,
    /// simulating different callers (per-user vault tiers) hitting one shared
    /// `XeroClient`. Only `get` is load-bearing for the Xero client; the rest
    /// satisfy the trait.
    #[derive(Default)]
    struct SwappableStore {
        data: Mutex<HashMap<String, String>>,
    }

    impl SwappableStore {
        fn set_creds(&self, entity: &str, client_id: &str, client_secret: &str) {
            let mut d = self.data.lock().unwrap();
            d.insert(
                format!("skills/xero/instances/{entity}/client_id"),
                client_id.to_string(),
            );
            d.insert(
                format!("skills/xero/instances/{entity}/client_secret"),
                client_secret.to_string(),
            );
        }

        fn clear(&self) {
            self.data.lock().unwrap().clear();
        }
    }

    impl SecretStore for SwappableStore {
        fn get(&self, path: &str) -> Option<String> {
            self.data.lock().unwrap().get(path).cloned()
        }
        fn set(&self, path: &str, value: &str) -> eyre::Result<()> {
            self.data
                .lock()
                .unwrap()
                .insert(path.to_string(), value.to_string());
            Ok(())
        }
        fn delete(&self, path: &str) -> eyre::Result<bool> {
            Ok(self.data.lock().unwrap().remove(path).is_some())
        }
        fn list(&self, prefix: Option<&str>) -> Vec<String> {
            let p = prefix.unwrap_or("");
            self.data
                .lock()
                .unwrap()
                .keys()
                .filter(|k| k.starts_with(p))
                .cloned()
                .collect()
        }
    }

    fn store_backed_client(server: &MockServer, store: Arc<SwappableStore>) -> XeroClient {
        XeroClient::with_store_and_endpoints_for_test(
            store as Arc<dyn SecretStore>,
            "harbor",
            None,
            &server.uri(),
        )
    }

    #[test]
    fn credential_fingerprint_is_stable_and_distinct() {
        let a = credential_fingerprint("id-a", "secret-a");
        assert_eq!(a, credential_fingerprint("id-a", "secret-a"), "stable");
        assert_ne!(a, credential_fingerprint("id-b", "secret-a"), "id differs");
        assert_ne!(
            a,
            credential_fingerprint("id-a", "secret-b"),
            "secret differs"
        );
        // The NUL domain boundary means splitting id/secret cannot collide.
        assert_ne!(
            credential_fingerprint("ab", "c"),
            credential_fingerprint("a", "bc"),
        );
    }

    #[tokio::test]
    async fn distinct_credentials_get_distinct_tokens_and_reuse_per_set() {
        let server = MockServer::start().await;
        // Two distinct credential sets ⇒ two token fetches; repeating the first
        // set reuses its cached token (no third fetch). Against the pre-fix code
        // the shared cache would fetch once and `expect(2)` would fail.
        token_mock(1800).expect(2).mount(&server).await;
        connections_mock(&["T"]).mount(&server).await;
        Mock::given(method("GET"))
            .and(path("/api.xro/2.0/Organisation"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
            .mount(&server)
            .await;

        let store = Arc::new(SwappableStore::default());
        let client = store_backed_client(&server, Arc::clone(&store));

        store.set_creds("harbor", "id-a", "secret-a");
        client.get("/Organisation", &[]).await.unwrap();

        store.set_creds("harbor", "id-b", "secret-b");
        client.get("/Organisation", &[]).await.unwrap();

        store.set_creds("harbor", "id-a", "secret-a");
        client.get("/Organisation", &[]).await.unwrap();
    }

    #[tokio::test]
    async fn caller_without_credentials_denied_even_after_warm_cache() {
        let server = MockServer::start().await;
        // The token is minted at most once (by the credentialed caller) and the
        // Organisation endpoint hit at most once. If a credential-less caller
        // could ride the warmed bearer, the org endpoint would be hit a second
        // time and `expect(1)` would fail — this is the leak detector. Against
        // the pre-fix code this test fails (second call returns A's data).
        token_mock(1800).expect(1).mount(&server).await;
        connections_mock(&["T"]).mount(&server).await;
        Mock::given(method("GET"))
            .and(path("/api.xro/2.0/Organisation"))
            .and(header("authorization", "Bearer tok-abc"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
            .expect(1)
            .mount(&server)
            .await;

        let store = Arc::new(SwappableStore::default());
        let client = store_backed_client(&server, Arc::clone(&store));

        // Credentialed caller warms the shared client's token cache.
        store.set_creds("harbor", "id-a", "secret-a");
        client.get("/Organisation", &[]).await.unwrap();

        // A different caller resolves no credentials: denied at resolution,
        // never reaching Xero with the warmed bearer.
        store.clear();
        let err = client.get("/Organisation", &[]).await.unwrap_err();
        assert!(
            err.to_string().contains("not configured"),
            "expected a not-configured error, got: {err}"
        );
    }
}
