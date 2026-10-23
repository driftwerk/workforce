//! Datadog HTTP client wrapper.
//!
//! Thin wrapper around `reqwest::Client` with `DD-API-KEY` and
//! `DD-APPLICATION-KEY` header authentication, site→base-URL mapping, and
//! Datadog-shaped error parsing.

use std::collections::VecDeque;
use std::sync::LazyLock;
use std::time::Duration;

use eyre::{eyre, Result};
use reqwest::header::{HeaderMap, HeaderValue, ACCEPT, USER_AGENT};
use serde_json::Value;
use tokio::time::Instant;
use workforce_agent::tools::RateLimitedError;

use crate::adapters::auth::{field_paths, resolve_credential, resolve_host};
use crate::adapters::utils::retry_after_secs;
use workforce_common::settings::SecretStore;

/// Map a Datadog site identifier to its API base URL.
///
/// Accepts short keys (`us1`, `eu1`, …), Datadog's canonical hostnames
/// (`datadoghq.com`, `datadoghq.eu`, `us3.datadoghq.com`, …), the full
/// API hostnames (`api.datadoghq.eu`), and even pasted URLs
/// (`https://api.datadoghq.eu/`). Case-insensitive. Errors on unknown
/// values instead of silently defaulting.
pub fn site_to_base_url(site: &str) -> std::result::Result<String, String> {
    let lower = site.trim().to_ascii_lowercase();
    let sans_scheme = lower
        .strip_prefix("https://")
        .or_else(|| lower.strip_prefix("http://"))
        .unwrap_or(&lower);
    let normalised = sans_scheme
        .strip_prefix("api.")
        .unwrap_or(sans_scheme)
        .trim_end_matches('/');

    const SITES: &[(&[&str], &str)] = &[
        (&["us1", "datadoghq.com"], "https://api.datadoghq.com"),
        (
            &["us3", "us3.datadoghq.com"],
            "https://api.us3.datadoghq.com",
        ),
        (
            &["us5", "us5.datadoghq.com"],
            "https://api.us5.datadoghq.com",
        ),
        (&["eu", "eu1", "datadoghq.eu"], "https://api.datadoghq.eu"),
        (
            &["ap1", "ap1.datadoghq.com"],
            "https://api.ap1.datadoghq.com",
        ),
    ];

    for (aliases, url) in SITES {
        if aliases.contains(&normalised) {
            return Ok(url.to_string());
        }
    }

    Err(format!(
        "Unknown Datadog site: '{}'. Expected one of: us1, us3, us5, eu1, ap1 \
         (or hostname form: datadoghq.com, datadoghq.eu, us3.datadoghq.com, etc.). \
         For FedRAMP or custom domains, use the base_url field instead.",
        site.trim()
    ))
}

/// Static credentials captured by [`DatadogClient::new`].
struct StaticAuth {
    api_key: HeaderValue,
    app_key: HeaderValue,
}

/// Thin wrapper around `reqwest::Client` with Datadog authentication.
///
/// Construct via [`DatadogClient::new`] for static keys, or
/// [`DatadogClient::with_store`] to resolve
/// `skills/datadog/accounts/{workspace}/{api_key,app_key}` per request.
pub struct DatadogClient {
    client: reqwest::Client,
    base_url: String,
    static_auth: Option<StaticAuth>,
    store: Option<std::sync::Arc<dyn workforce_common::settings::SecretStore>>,
    workspace: String,
}

impl DatadogClient {
    /// Create a new Datadog client. `site` accepts short keys (`us1`, `eu1`,
    /// …) or hostname forms (`datadoghq.eu`, `us3.datadoghq.com`, …).
    /// `base_url_override` (if `Some`) bypasses site resolution entirely.
    pub fn new(
        api_key: &str,
        app_key: &str,
        site: &str,
        base_url_override: Option<&str>,
    ) -> Result<Self> {
        let base_url = match base_url_override {
            Some(url) => url.to_string(),
            None => site_to_base_url(site).map_err(|e| eyre!(e))?,
        };

        let auth = StaticAuth {
            api_key: HeaderValue::from_str(api_key)
                .map_err(|e| eyre!("Invalid Datadog api_key: {}", e))?,
            app_key: HeaderValue::from_str(app_key)
                .map_err(|e| eyre!("Invalid Datadog app_key: {}", e))?,
        };

        let client = reqwest::Client::builder().build()?;

        Ok(Self {
            client,
            base_url: base_url.trim_end_matches('/').to_string(),
            static_auth: Some(auth),
            store: None,
            workspace: String::new(),
        })
    }

    /// Build a client that resolves credentials from `store` per request.
    pub fn with_store(
        store: std::sync::Arc<dyn workforce_common::settings::SecretStore>,
        workspace: &str,
        site: &str,
        base_url_override: Option<&str>,
    ) -> Result<Self> {
        let base_url = match base_url_override {
            Some(url) => url.to_string(),
            None => site_to_base_url(site).map_err(|e| eyre!(e))?,
        };
        let client = reqwest::Client::builder().build()?;
        Ok(Self {
            client,
            base_url: base_url.trim_end_matches('/').to_string(),
            static_auth: None,
            store: Some(store),
            workspace: workspace.to_string(),
        })
    }

    fn paths(&self, field: &str) -> (String, String) {
        field_paths("datadog", "accounts", &self.workspace, field)
    }

    /// Both keys are credentials in their own right (Datadog's application
    /// key carries a person's permissions), so the host follows the caller
    /// only when both came from the caller's tiers.
    fn build_request(&self) -> Result<(HeaderMap, String)> {
        let (api_key, app_key, base_url) = match (&self.static_auth, &self.store) {
            (Some(s), _) => (s.api_key.clone(), s.app_key.clone(), self.base_url.clone()),
            (None, Some(store)) => {
                let credential = |field: &str| {
                    let (ws, flat) = self.paths(field);
                    resolve_credential(store.as_ref(), &ws, &flat)
                        .ok_or_else(|| eyre!("Datadog {field} not configured at {ws}"))
                };
                let api_key = credential("api_key")?;
                let app_key = credential("app_key")?;
                let base_url =
                    self.host_for(store.as_ref(), api_key.per_user && app_key.per_user)?;
                (
                    HeaderValue::from_str(&api_key.value)
                        .map_err(|e| eyre!("Invalid Datadog api_key: {}", e))?,
                    HeaderValue::from_str(&app_key.value)
                        .map_err(|e| eyre!("Invalid Datadog app_key: {}", e))?,
                    base_url,
                )
            }
            (None, None) => return Err(eyre!("Datadog client has no credential store")),
        };
        let mut headers = HeaderMap::new();
        headers.insert("DD-API-KEY", api_key);
        headers.insert("DD-APPLICATION-KEY", app_key);
        headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
        headers.insert(USER_AGENT, HeaderValue::from_static("workforce"));
        Ok((headers, base_url))
    }

    fn host_for(&self, store: &dyn SecretStore, per_user: bool) -> Result<String> {
        let host = |field: &str| {
            let (ws, flat) = self.paths(field);
            resolve_host(store, per_user, &ws, &flat)
        };
        match (host("base_url"), host("site")) {
            (Some(url), _) => Ok(url),
            (None, Some(site)) => site_to_base_url(&site).map_err(|e| eyre!(e)),
            (None, None) => Ok(self.base_url.clone()),
        }
    }

    /// The configured API base URL (no trailing slash).
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// GET request.
    pub async fn get(&self, path: &str) -> Result<Value> {
        let (headers, base_url) = self.build_request()?;
        let url = format!("{base_url}{path}");
        let response = self.client.get(&url).headers(headers).send().await?;
        Self::handle_response(response).await
    }

    /// GET request with query parameters.
    pub async fn get_with_query(&self, path: &str, query: &[(&str, &str)]) -> Result<Value> {
        let (headers, base_url) = self.build_request()?;
        let url = format!("{base_url}{path}");
        let response = self
            .client
            .get(&url)
            .headers(headers)
            .query(query)
            .send()
            .await?;
        Self::handle_response(response).await
    }

    /// POST request with a JSON body. A log query waits its turn in
    /// [`LOG_QUERIES`] first.
    pub async fn post(&self, path: &str, body: &Value) -> Result<Value> {
        if path.starts_with("/api/v2/logs/") {
            LOG_QUERIES.admit().await;
        }
        let (headers, base_url) = self.build_request()?;
        let url = format!("{base_url}{path}");
        let response = self
            .client
            .post(&url)
            .headers(headers)
            .json(body)
            .send()
            .await?;
        Self::handle_response(response).await
    }

    /// PUT request with a JSON body.
    pub async fn put(&self, path: &str, body: &Value) -> Result<Value> {
        let (headers, base_url) = self.build_request()?;
        let url = format!("{base_url}{path}");
        let response = self
            .client
            .put(&url)
            .headers(headers)
            .json(body)
            .send()
            .await?;
        Self::handle_response(response).await
    }

    /// DELETE request.
    pub async fn delete(&self, path: &str) -> Result<Value> {
        let (headers, base_url) = self.build_request()?;
        let url = format!("{base_url}{path}");
        let response = self.client.delete(&url).headers(headers).send().await?;
        Self::handle_response(response).await
    }

    /// Map a `reqwest::Response` to a `Result<Value>`, extracting Datadog's
    /// `{errors: [...]}` shape into a readable error string.
    async fn handle_response(response: reqwest::Response) -> Result<Value> {
        let status = response.status();
        let retry_after = retry_after_secs(response.headers())
            .or_else(|| header_secs(response.headers(), "x-ratelimit-reset"));
        let quota = rate_limit_quota(response.headers());

        if status.is_success() {
            if status == reqwest::StatusCode::NO_CONTENT {
                return Ok(Value::Null);
            }
            let text = response.text().await.unwrap_or_default();
            if text.is_empty() {
                return Ok(Value::Null);
            }
            return serde_json::from_str::<Value>(&text)
                .map_err(|e| eyre!("Failed to parse Datadog response as JSON: {}", e));
        }

        let body = response.text().await.unwrap_or_default();
        let message = parse_datadog_error(&body);

        match status.as_u16() {
            400 => Err(eyre!("Bad request: {}", message)),
            401 => Err(eyre!("Authentication failed: {}", message)),
            403 => Err(eyre!(
                "Forbidden (check API/Application key scopes): {}",
                message
            )),
            404 => Err(eyre!("Not found: {}", message)),
            409 => Err(eyre!("Conflict: {}", message)),
            422 => Err(eyre!("Validation error: {}", message)),
            429 => Err(RateLimitedError::new(
                format!("Rate limited by Datadog: {message}{quota}"),
                retry_after,
            )
            .into()),
            _ => Err(eyre!("Datadog API error ({}): {}", status, message)),
        }
    }
}

/// Datadog admits three log queries per ten seconds (its `X-RateLimit-*`
/// headers on a 429), so a skill that pages through a day of logs one call
/// after another is refused from the fourth call on. Log queries queue here
/// and go out at that rate instead.
static LOG_QUERIES: LazyLock<Pacer> = LazyLock::new(|| Pacer::new(3, Duration::from_secs(10)));

/// Admits at most `burst` calls per sliding `window`, process-wide; a caller
/// over the limit sleeps until the oldest call in the window ages out.
struct Pacer {
    burst: usize,
    window: Duration,
    sent: tokio::sync::Mutex<VecDeque<Instant>>,
}

impl Pacer {
    fn new(burst: usize, window: Duration) -> Self {
        Self {
            burst,
            window,
            sent: tokio::sync::Mutex::new(VecDeque::with_capacity(burst)),
        }
    }

    async fn admit(&self) {
        let mut sent = self.sent.lock().await;
        let now = Instant::now();
        while sent
            .front()
            .is_some_and(|t| now.duration_since(*t) >= self.window)
        {
            sent.pop_front();
        }
        if sent.len() >= self.burst {
            if let Some(oldest) = sent.pop_front() {
                tokio::time::sleep_until(oldest + self.window).await;
            }
        }
        sent.push_back(Instant::now());
    }
}

/// Datadog's quota headers as ` (quota <name> N per Ps, M left, resets in Rs)`,
/// so a rate-limited call says which limit it hit and when it clears; empty
/// when the response carries none of them.
fn rate_limit_quota(headers: &HeaderMap) -> String {
    let field = |name: &str| header_str(headers, name);
    let parts: Vec<String> = [
        field("x-ratelimit-limit").map(|limit| {
            let name = field("x-ratelimit-name").map_or(String::new(), |n| format!("{n} "));
            match field("x-ratelimit-period") {
                Some(period) => format!("quota {name}{limit} per {period}s"),
                None => format!("quota {name}{limit}"),
            }
        }),
        field("x-ratelimit-remaining").map(|left| format!("{left} left")),
        field("x-ratelimit-reset").map(|reset| format!("resets in {reset}s")),
    ]
    .into_iter()
    .flatten()
    .collect();
    if parts.is_empty() {
        String::new()
    } else {
        format!(" ({})", parts.join(", "))
    }
}

fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|v| !v.is_empty())
}

/// A whole-seconds header, as Datadog's relative `X-RateLimit-Reset`.
fn header_secs(headers: &HeaderMap, name: &str) -> Option<u64> {
    header_str(headers, name).and_then(|v| v.parse().ok())
}

/// Parse a Datadog error response body into a readable string.
///
/// Datadog returns `{"errors": ["msg", ...]}` (most APIs) or
/// `{"errors": [{"detail": "...", "title": "..."}, ...]}` (v2 JSON:API).
/// Falls back to the raw text when neither shape matches.
fn parse_datadog_error(body: &str) -> String {
    let Ok(v) = serde_json::from_str::<Value>(body) else {
        return body.to_string();
    };

    let parts: Vec<String> = v
        .get("errors")
        .and_then(|x| x.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|item| {
                    item.as_str().map(String::from).or_else(|| {
                        let detail = item.get("detail").and_then(|d| d.as_str());
                        let title = item.get("title").and_then(|t| t.as_str());
                        match (title, detail) {
                            (Some(t), Some(d)) => Some(format!("{t}: {d}")),
                            (Some(t), None) => Some(t.to_string()),
                            (None, Some(d)) => Some(d.to_string()),
                            (None, None) => None,
                        }
                    })
                })
                .collect()
        })
        .unwrap_or_default();

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
        assert!(DatadogClient::new("api", "app", "us1", None).is_ok());
    }

    #[test]
    fn client_trims_trailing_slash_on_base_url() {
        let client = DatadogClient::new("api", "app", "us1", Some("https://example.com/")).unwrap();
        assert_eq!(client.base_url(), "https://example.com");
    }

    #[test]
    fn client_errors_on_unknown_site() {
        assert!(DatadogClient::new("api", "app", "bogus", None).is_err());
    }

    #[test]
    fn site_to_base_url_maps_all_known_short_keys() {
        assert_eq!(
            site_to_base_url("us1").unwrap(),
            "https://api.datadoghq.com"
        );
        assert_eq!(
            site_to_base_url("us3").unwrap(),
            "https://api.us3.datadoghq.com"
        );
        assert_eq!(
            site_to_base_url("us5").unwrap(),
            "https://api.us5.datadoghq.com"
        );
        assert_eq!(site_to_base_url("eu1").unwrap(), "https://api.datadoghq.eu");
        assert_eq!(site_to_base_url("eu").unwrap(), "https://api.datadoghq.eu");
        assert_eq!(
            site_to_base_url("ap1").unwrap(),
            "https://api.ap1.datadoghq.com"
        );
    }

    #[test]
    fn site_to_base_url_accepts_hostname_form() {
        assert_eq!(
            site_to_base_url("datadoghq.com").unwrap(),
            "https://api.datadoghq.com"
        );
        assert_eq!(
            site_to_base_url("datadoghq.eu").unwrap(),
            "https://api.datadoghq.eu"
        );
        assert_eq!(
            site_to_base_url("us3.datadoghq.com").unwrap(),
            "https://api.us3.datadoghq.com"
        );
        assert_eq!(
            site_to_base_url("us5.datadoghq.com").unwrap(),
            "https://api.us5.datadoghq.com"
        );
        assert_eq!(
            site_to_base_url("ap1.datadoghq.com").unwrap(),
            "https://api.ap1.datadoghq.com"
        );
    }

    #[test]
    fn site_to_base_url_accepts_api_hostname() {
        assert_eq!(
            site_to_base_url("api.datadoghq.eu").unwrap(),
            "https://api.datadoghq.eu"
        );
        assert_eq!(
            site_to_base_url("api.datadoghq.com").unwrap(),
            "https://api.datadoghq.com"
        );
    }

    #[test]
    fn site_to_base_url_strips_https_prefix() {
        assert_eq!(
            site_to_base_url("https://datadoghq.eu").unwrap(),
            "https://api.datadoghq.eu"
        );
        assert_eq!(
            site_to_base_url("https://api.datadoghq.eu").unwrap(),
            "https://api.datadoghq.eu"
        );
    }

    #[test]
    fn site_to_base_url_strips_trailing_slash() {
        assert_eq!(
            site_to_base_url("datadoghq.eu/").unwrap(),
            "https://api.datadoghq.eu"
        );
    }

    #[test]
    fn site_to_base_url_is_case_insensitive() {
        assert_eq!(site_to_base_url("EU1").unwrap(), "https://api.datadoghq.eu");
        assert_eq!(
            site_to_base_url("DATADOGHQ.EU").unwrap(),
            "https://api.datadoghq.eu"
        );
    }

    #[test]
    fn site_to_base_url_errors_on_unknown_site() {
        assert!(site_to_base_url("unknown").is_err());
        assert!(site_to_base_url("").is_err());
        assert!(site_to_base_url("datadoghq.com.evil").is_err());
        let err = site_to_base_url("bogus").unwrap_err();
        assert!(err.contains("Unknown Datadog site"));
        assert!(err.contains("us1"));
        assert!(err.contains("datadoghq.com"));
    }

    #[test]
    fn parses_datadog_error_with_string_array() {
        let body = r#"{"errors":["Invalid query"]}"#;
        assert_eq!(parse_datadog_error(body), "Invalid query");
    }

    #[test]
    fn parses_datadog_error_with_jsonapi_object_array() {
        let body = r#"{"errors":[{"title":"Bad Request","detail":"missing field 'query'"}]}"#;
        assert_eq!(
            parse_datadog_error(body),
            "Bad Request: missing field 'query'"
        );
    }

    #[test]
    fn parses_datadog_error_falls_back_to_raw_text() {
        let body = "not json at all";
        assert_eq!(parse_datadog_error(body), "not json at all");
    }

    #[test]
    fn parses_datadog_error_combines_multiple_messages() {
        let body = r#"{"errors":["one","two","three"]}"#;
        assert_eq!(parse_datadog_error(body), "one; two; three");
    }

    #[test]
    fn parses_datadog_error_with_only_detail() {
        let body = r#"{"errors":[{"detail":"something broke"}]}"#;
        assert_eq!(parse_datadog_error(body), "something broke");
    }

    use crate::adapters::test_support::{InMem, LayeredInMem};

    #[test]
    fn build_headers_falls_back_to_flat_path_when_account_path_missing() {
        let store = InMem::from(&[
            ("skills/datadog/api_key", "flat-api-key-12345"),
            ("skills/datadog/app_key", "flat-app-key-67890"),
        ]);
        let client = DatadogClient::with_store(store, "default", "us1", None).unwrap();
        let (headers, base_url) = client.build_request().unwrap();
        assert_eq!(headers.get("DD-API-KEY").unwrap(), "flat-api-key-12345");
        assert_eq!(
            headers.get("DD-APPLICATION-KEY").unwrap(),
            "flat-app-key-67890"
        );
        assert_eq!(base_url, "https://api.datadoghq.com");
    }

    #[test]
    fn build_headers_prefers_account_path_over_flat() {
        let store = InMem::from(&[
            ("skills/datadog/api_key", "flat-api-key"),
            ("skills/datadog/app_key", "flat-app-key"),
            ("skills/datadog/accounts/prod/api_key", "prod-api-key"),
            ("skills/datadog/accounts/prod/app_key", "prod-app-key"),
        ]);
        let client = DatadogClient::with_store(store, "prod", "us1", None).unwrap();
        let (headers, _) = client.build_request().unwrap();
        assert_eq!(headers.get("DD-API-KEY").unwrap(), "prod-api-key");
        assert_eq!(headers.get("DD-APPLICATION-KEY").unwrap(), "prod-app-key");
    }

    #[test]
    fn shared_keys_ignore_a_per_user_host_and_site() {
        let store = LayeredInMem::new(
            &[
                ("skills/datadog/api_key", "shared-api"),
                ("skills/datadog/app_key", "shared-app"),
            ],
            &[
                ("skills/datadog/base_url", "https://attacker.example"),
                ("skills/datadog/site", "eu1"),
            ],
        );
        let client = DatadogClient::with_store(store, "default", "us1", None).unwrap();
        let (_, base_url) = client.build_request().unwrap();
        assert_eq!(base_url, "https://api.datadoghq.com");
    }

    #[test]
    fn a_per_user_app_key_is_honoured_and_the_host_stays_shared_unless_both_keys_are_per_user() {
        let store = LayeredInMem::new(
            &[
                ("skills/datadog/api_key", "shared-api"),
                ("skills/datadog/app_key", "shared-app"),
            ],
            &[
                ("skills/datadog/app_key", "my-app"),
                ("skills/datadog/site", "eu1"),
            ],
        );
        let client = DatadogClient::with_store(store, "default", "us1", None).unwrap();
        let (headers, base_url) = client.build_request().unwrap();
        assert_eq!(headers.get("DD-API-KEY").unwrap(), "shared-api");
        assert_eq!(headers.get("DD-APPLICATION-KEY").unwrap(), "my-app");
        assert_eq!(base_url, "https://api.datadoghq.com");

        let store = LayeredInMem::new(
            &[("skills/datadog/app_key", "shared-app")],
            &[
                ("skills/datadog/api_key", "my-api"),
                ("skills/datadog/site", "eu1"),
            ],
        );
        let client = DatadogClient::with_store(store, "default", "us1", None).unwrap();
        let (_, base_url) = client.build_request().unwrap();
        assert_eq!(base_url, "https://api.datadoghq.com");
    }

    #[test]
    fn per_user_keys_use_their_own_site_then_base_url() {
        let store = LayeredInMem::new(
            &[("skills/datadog/app_key", "shared-app")],
            &[
                ("skills/datadog/api_key", "my-api"),
                ("skills/datadog/app_key", "my-app"),
                ("skills/datadog/site", "eu1"),
            ],
        );
        let client = DatadogClient::with_store(store, "default", "us1", None).unwrap();
        let (headers, base_url) = client.build_request().unwrap();
        assert_eq!(headers.get("DD-APPLICATION-KEY").unwrap(), "my-app");
        assert_eq!(base_url, "https://api.datadoghq.eu");

        let store = LayeredInMem::new(
            &[],
            &[
                ("skills/datadog/api_key", "my-api"),
                ("skills/datadog/app_key", "my-app"),
                ("skills/datadog/base_url", "https://dd.gov.example/"),
                ("skills/datadog/site", "eu1"),
            ],
        );
        let client = DatadogClient::with_store(store, "default", "us1", None).unwrap();
        let (_, base_url) = client.build_request().unwrap();
        assert_eq!(base_url, "https://dd.gov.example");
    }

    #[tokio::test]
    async fn a_429_maps_to_a_typed_rate_limit_with_the_retry_after_hint() {
        let resp = reqwest::Response::from(
            http::Response::builder()
                .status(429)
                .header("retry-after", "30")
                .body(r#"{"errors":["rate limit exceeded"]}"#)
                .unwrap(),
        );
        let err = DatadogClient::handle_response(resp).await.unwrap_err();
        let limit = err
            .downcast_ref::<RateLimitedError>()
            .expect("429 should carry the typed rate-limit signal");
        assert_eq!(
            limit.message,
            "Rate limited by Datadog: rate limit exceeded"
        );
        assert_eq!(limit.retry_after_secs, Some(30));
        assert_eq!(
            err.to_string(),
            "Rate limited by Datadog: rate limit exceeded (retry after 30s)"
        );
    }

    #[tokio::test]
    async fn a_429_names_the_quota_it_hit_when_datadog_says_which() {
        let resp = reqwest::Response::from(
            http::Response::builder()
                .status(429)
                .header("x-ratelimit-name", "logs_api_query")
                .header("x-ratelimit-limit", "300")
                .header("x-ratelimit-period", "3600")
                .header("x-ratelimit-remaining", "0")
                .header("x-ratelimit-reset", "1200")
                .body(r#"{"errors":["Too many requests"]}"#)
                .unwrap(),
        );
        let err = DatadogClient::handle_response(resp).await.unwrap_err();
        let limit = err.downcast_ref::<RateLimitedError>().unwrap();
        assert_eq!(limit.retry_after_secs, Some(1200));
        assert_eq!(
            limit.message,
            "Rate limited by Datadog: Too many requests (quota logs_api_query 300 per 3600s, 0 left, resets in 1200s)"
        );
    }

    #[test]
    fn a_partial_set_of_quota_headers_names_what_it_has() {
        let mut headers = HeaderMap::new();
        headers.insert("x-ratelimit-limit", HeaderValue::from_static("300"));
        assert_eq!(rate_limit_quota(&headers), " (quota 300)");
        headers.insert("x-ratelimit-remaining", HeaderValue::from_static(" 7 "));
        assert_eq!(rate_limit_quota(&headers), " (quota 300, 7 left)");
        assert_eq!(rate_limit_quota(&HeaderMap::new()), "");
    }

    #[tokio::test(start_paused = true)]
    async fn a_pacer_spreads_a_burst_over_its_window() {
        let pacer = Pacer::new(3, Duration::from_secs(10));
        let start = Instant::now();
        for _ in 0..3 {
            pacer.admit().await;
        }
        assert_eq!(start.elapsed(), Duration::ZERO);
        pacer.admit().await;
        assert_eq!(start.elapsed(), Duration::from_secs(10));
        pacer.admit().await;
        assert_eq!(start.elapsed(), Duration::from_secs(10));
        tokio::time::advance(Duration::from_secs(4)).await;
        pacer.admit().await;
        assert_eq!(start.elapsed(), Duration::from_secs(14));
        pacer.admit().await;
        assert_eq!(start.elapsed(), Duration::from_secs(20));
    }

    #[tokio::test]
    async fn a_429_with_a_non_numeric_retry_after_drops_the_hint() {
        let resp = reqwest::Response::from(
            http::Response::builder()
                .status(429)
                .header("retry-after", "Wed, 21 Oct 2026 07:28:00 GMT")
                .body(r#"{"errors":["rate limit exceeded"]}"#)
                .unwrap(),
        );
        let err = DatadogClient::handle_response(resp).await.unwrap_err();
        let limit = err.downcast_ref::<RateLimitedError>().unwrap();
        assert_eq!(limit.retry_after_secs, None);
    }
}
