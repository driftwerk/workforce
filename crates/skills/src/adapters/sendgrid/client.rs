//! SendGrid v3 HTTP client.
//!
//! One API key per workspace, sent as `Authorization: Bearer`. In
//! [`SendGridClient::with_store`] mode the key and the default sender are
//! resolved from the layered vault on every request, so per-user overlays
//! apply without any shared token state. A `429` is surfaced as the typed
//! [`RateLimitedError`], with the retry hint taken from `Retry-After` or
//! SendGrid's `X-RateLimit-Reset` epoch.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use eyre::{eyre, Result};
use reqwest::header::{HeaderMap, HeaderValue, ACCEPT, AUTHORIZATION, CONTENT_TYPE, USER_AGENT};
use reqwest::{Method, RequestBuilder, Response, StatusCode};
use serde_json::{json, Value};
use workforce_agent::tools::RateLimitedError;
use workforce_common::settings::SecretStore;

use crate::adapters::auth::{field_paths, resolve_credential, resolve_host};
use crate::adapters::utils::retry_after_secs;

pub const DEFAULT_BASE_URL: &str = "https://api.sendgrid.com";
const MESSAGE_ID_HEADER: &str = "x-message-id";
const RATE_LIMIT_RESET_HEADER: &str = "x-ratelimit-reset";

/// A From identity: the address must be a verified sender in the account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sender {
    pub email: String,
    pub name: Option<String>,
}

enum Credentials {
    Static {
        api_key: String,
        sender: Option<Sender>,
    },
    Store(Arc<dyn SecretStore>),
}

pub struct SendGridClient {
    http: reqwest::Client,
    base_url: String,
    credentials: Credentials,
    workspace: String,
}

impl SendGridClient {
    /// Static credentials, validated up front so a malformed key fails at
    /// load rather than on the first send.
    pub fn new(api_key: &str, sender: Option<Sender>, base_url: Option<&str>) -> Result<Self> {
        bearer_value(api_key)?;
        Ok(Self {
            http: build_http()?,
            base_url: normalise_base_url(base_url),
            credentials: Credentials::Static {
                api_key: api_key.trim().to_string(),
                sender,
            },
            workspace: String::new(),
        })
    }

    /// Credentials resolved from `store` per request, honouring per-user
    /// overlays when the store is a `LayeredUserVault`.
    pub fn with_store(
        store: Arc<dyn SecretStore>,
        workspace: &str,
        base_url: Option<&str>,
    ) -> Result<Self> {
        Ok(Self {
            http: build_http()?,
            base_url: normalise_base_url(base_url),
            credentials: Credentials::Store(store),
            workspace: workspace.to_string(),
        })
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    fn paths(&self, field: &str) -> (String, String) {
        field_paths("sendgrid", "workspaces", &self.workspace, field)
    }

    fn api_key(&self) -> Result<(String, String)> {
        match &self.credentials {
            Credentials::Static { api_key, .. } => Ok((api_key.clone(), self.base_url.clone())),
            Credentials::Store(store) => {
                let (workspace, flat) = self.paths("api_key");
                let api_key =
                    resolve_credential(store.as_ref(), &workspace, &flat).ok_or_else(|| {
                        eyre!(
                            "SendGrid api_key not configured. Set {workspace} (or {flat} for a \
                         single workspace)."
                        )
                    })?;
                let (url_ws, url_flat) = self.paths("base_url");
                let base_url = resolve_host(store.as_ref(), api_key.per_user, &url_ws, &url_flat)
                    .unwrap_or_else(|| self.base_url.clone());
                Ok((api_key.value, base_url))
            }
        }
    }

    /// The workspace's default From. A sender resolves as one unit: the
    /// display name is taken only from the tier and path form that supplied
    /// the address, so a per-user address never borrows a shared name.
    pub fn default_sender(&self) -> Option<Sender> {
        match &self.credentials {
            Credentials::Static { sender, .. } => sender.clone(),
            Credentials::Store(store) => {
                let (workspace_email, flat_email) = self.paths("from_email");
                let (workspace_name, flat_name) = self.paths("from_name");
                let user = |path: &str| store.get_user_override(path);
                let shared = |path: &str| store.get(path);
                sender_at(user, &workspace_email, &workspace_name)
                    .or_else(|| sender_at(user, &flat_email, &flat_name))
                    .or_else(|| sender_at(shared, &workspace_email, &workspace_name))
                    .or_else(|| sender_at(shared, &flat_email, &flat_name))
            }
        }
    }

    fn builder(&self, method: Method, path: &str) -> Result<RequestBuilder> {
        let (api_key, base_url) = self.api_key()?;
        let token = bearer_value(&api_key)?;
        Ok(self
            .http
            .request(method, format!("{base_url}{path}"))
            .header(AUTHORIZATION, token)
            .header(ACCEPT, HeaderValue::from_static("application/json"))
            .header(CONTENT_TYPE, HeaderValue::from_static("application/json"))
            .header(USER_AGENT, HeaderValue::from_static("workforce")))
    }

    pub async fn get(&self, path: &str, query: &[(&str, String)]) -> Result<Value> {
        self.request(Method::GET, path, query, None).await
    }

    pub async fn post(&self, path: &str, body: &Value) -> Result<Value> {
        self.request(Method::POST, path, &[], Some(body)).await
    }

    pub async fn delete(&self, path: &str) -> Result<Value> {
        self.request(Method::DELETE, path, &[], None).await
    }

    pub async fn request(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, String)],
        body: Option<&Value>,
    ) -> Result<Value> {
        let builder = self.builder(method, path)?.query(query);
        let builder = match body {
            Some(body) => builder.json(body),
            None => builder,
        };
        json_body(self.send(builder).await?).await
    }

    /// `POST /v3/mail/send`: SendGrid answers `202` with an empty body and
    /// the queued id in `X-Message-Id`.
    pub async fn mail_send(&self, body: &Value) -> Result<Value> {
        let response = self
            .send(self.builder(Method::POST, "/v3/mail/send")?.json(body))
            .await?;
        let message_id = response
            .headers()
            .get(MESSAGE_ID_HEADER)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        Ok(json!({ "accepted": true, "message_id": message_id }))
    }

    async fn send(&self, builder: RequestBuilder) -> Result<Response> {
        check_status(builder.send().await?).await
    }
}

async fn check_status(response: Response) -> Result<Response> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let retry_after = retry_after_secs(response.headers())
        .or_else(|| rate_limit_reset_secs(response.headers(), now_secs()));
    let message = parse_error(&response.text().await.unwrap_or_default(), status);
    Err(match status.as_u16() {
        400 => eyre!("Bad request: {message}"),
        401 => eyre!("Authentication failed — check the SendGrid api_key: {message}"),
        403 => eyre!(
            "Forbidden — the API key lacks the required scope, or the From address is not a \
             verified sender identity: {message}"
        ),
        404 => eyre!("Not found: {message}"),
        413 => eyre!("Payload too large — SendGrid caps a message at 30 MB: {message}"),
        429 => RateLimitedError::new(format!("Rate limited by SendGrid: {message}"), retry_after)
            .into(),
        s if (500..600).contains(&s) => eyre!("SendGrid server error ({s}): {message}"),
        s => eyre!("SendGrid API error ({s}): {message}"),
    })
}

async fn json_body(response: Response) -> Result<Value> {
    if response.status() == StatusCode::NO_CONTENT {
        return Ok(Value::Null);
    }
    let text = response.text().await.unwrap_or_default();
    if text.trim().is_empty() {
        return Ok(Value::Null);
    }
    serde_json::from_str(&text).map_err(|e| eyre!("Failed to parse SendGrid response as JSON: {e}"))
}

fn sender_at(get: impl Fn(&str) -> Option<String>, email: &str, name: &str) -> Option<Sender> {
    get(email).map(|email| Sender {
        email,
        name: get(name),
    })
}

fn bearer_value(api_key: &str) -> Result<HeaderValue> {
    let api_key = api_key.trim();
    if api_key.is_empty() {
        return Err(eyre!("SendGrid api_key cannot be empty"));
    }
    HeaderValue::from_str(&format!("Bearer {api_key}"))
        .map_err(|e| eyre!("Invalid SendGrid api_key: {e}"))
}

fn normalise_base_url(base_url: Option<&str>) -> String {
    base_url
        .map(|s| s.trim_end_matches('/').to_string())
        .unwrap_or_else(|| DEFAULT_BASE_URL.to_string())
}

fn build_http() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()?)
}

/// Render SendGrid's `{"errors":[{message, field, help}]}` envelope as one
/// line; falls back to the raw body, then to the HTTP status.
fn parse_error(body: &str, status: StatusCode) -> String {
    serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| {
            v.get("errors")?
                .as_array()
                .map(|errors| errors.iter().filter_map(render_error).collect::<Vec<_>>())
        })
        .filter(|parts| !parts.is_empty())
        .map(|parts| parts.join("; "))
        .unwrap_or_else(|| {
            if body.trim().is_empty() {
                format!("HTTP {}", status.as_u16())
            } else {
                body.to_string()
            }
        })
}

fn render_error(error: &Value) -> Option<String> {
    let message = error.get("message").and_then(Value::as_str)?;
    Some(
        match error
            .get("field")
            .and_then(Value::as_str)
            .filter(|f| !f.is_empty() && *f != "null")
        {
            Some(field) => format!("{message} (field: {field})"),
            None => message.to_string(),
        },
    )
}

/// Seconds until the `X-RateLimit-Reset` epoch, floored at zero.
fn rate_limit_reset_secs(headers: &HeaderMap, now: u64) -> Option<u64> {
    headers
        .get(RATE_LIMIT_RESET_HEADER)?
        .to_str()
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
        .map(|reset| reset.saturating_sub(now))
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::test_support::{canned_response, InMem, LayeredInMem};
    use wiremock::matchers::{body_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn sender() -> Sender {
        Sender {
            email: "noreply@example.com".into(),
            name: Some("Example".into()),
        }
    }

    #[test]
    fn client_builds_with_defaults_and_trims_base_url() {
        let c = SendGridClient::new("SG.key", None, None).unwrap();
        assert_eq!(c.base_url(), DEFAULT_BASE_URL);
        assert!(c.default_sender().is_none());

        let c = SendGridClient::new(
            "SG.key",
            Some(sender()),
            Some("https://api.eu.sendgrid.com/"),
        )
        .unwrap();
        assert_eq!(c.base_url(), "https://api.eu.sendgrid.com");
        assert_eq!(c.default_sender(), Some(sender()));
    }

    #[test]
    fn client_rejects_empty_or_header_invalid_keys_and_trims_valid_ones() {
        assert!(SendGridClient::new("", None, None).is_err());
        assert!(SendGridClient::new("   ", None, None).is_err());
        assert!(SendGridClient::new("bad\nkey", None, None).is_err());
        let client = SendGridClient::new(" SG.key\n", None, None).unwrap();
        assert_eq!(client.api_key().unwrap().0, "SG.key");
    }

    #[test]
    fn store_resolution_prefers_workspace_over_flat() {
        let store = InMem::from(&[
            ("skills/sendgrid/api_key", "SG.flat"),
            ("skills/sendgrid/from_email", "flat@example.com"),
            ("skills/sendgrid/workspaces/acme/api_key", "SG.acme"),
            (
                "skills/sendgrid/workspaces/acme/from_email",
                "acme@example.com",
            ),
            ("skills/sendgrid/workspaces/acme/from_name", "Acme"),
        ]);
        let acme = SendGridClient::with_store(store.clone(), "acme", None).unwrap();
        assert_eq!(acme.api_key().unwrap().0, "SG.acme");
        assert_eq!(
            acme.default_sender(),
            Some(Sender {
                email: "acme@example.com".into(),
                name: Some("Acme".into())
            })
        );

        let other = SendGridClient::with_store(store, "other", None).unwrap();
        assert_eq!(other.api_key().unwrap().0, "SG.flat");
        assert_eq!(
            other.default_sender(),
            Some(Sender {
                email: "flat@example.com".into(),
                name: None
            })
        );
    }

    #[test]
    fn user_tier_wins_and_the_sender_resolves_as_a_unit() {
        let store = LayeredInMem::new(
            &[
                ("skills/sendgrid/api_key", "SG.shared"),
                ("skills/sendgrid/from_email", "noreply@org.com"),
                ("skills/sendgrid/from_name", "Org Support"),
            ],
            &[
                ("skills/sendgrid/api_key", " SG.user "),
                ("skills/sendgrid/from_email", "me@org.com"),
            ],
        );
        let client = SendGridClient::with_store(store, "default", None).unwrap();
        assert_eq!(client.api_key().unwrap().0, " SG.user ");
        assert_eq!(
            bearer_value(&client.api_key().unwrap().0).unwrap(),
            "Bearer SG.user"
        );
        assert_eq!(
            client.default_sender(),
            Some(Sender {
                email: "me@org.com".into(),
                name: None
            })
        );
    }

    #[test]
    fn a_shared_key_ignores_a_per_user_host_but_a_per_user_key_may_use_one() {
        let store = LayeredInMem::new(
            &[("skills/sendgrid/api_key", "SG.shared")],
            &[("skills/sendgrid/base_url", "https://attacker.example")],
        );
        let client = SendGridClient::with_store(store, "default", None).unwrap();
        assert_eq!(
            client.api_key().unwrap(),
            ("SG.shared".to_string(), DEFAULT_BASE_URL.to_string())
        );

        let store = LayeredInMem::new(
            &[("skills/sendgrid/api_key", "SG.shared")],
            &[
                ("skills/sendgrid/api_key", "SG.mine"),
                ("skills/sendgrid/base_url", "https://api.eu.sendgrid.com/"),
            ],
        );
        let client = SendGridClient::with_store(store, "default", None).unwrap();
        assert_eq!(
            client.api_key().unwrap(),
            (
                "SG.mine".to_string(),
                "https://api.eu.sendgrid.com".to_string()
            )
        );
    }

    #[test]
    fn shared_sender_applies_when_the_user_only_has_a_key() {
        let store = LayeredInMem::new(
            &[
                ("skills/sendgrid/from_email", "noreply@org.com"),
                ("skills/sendgrid/from_name", "Org Support"),
            ],
            &[("skills/sendgrid/api_key", "SG.user")],
        );
        let client = SendGridClient::with_store(store, "default", None).unwrap();
        assert_eq!(client.api_key().unwrap().0, "SG.user");
        assert_eq!(
            client.default_sender(),
            Some(Sender {
                email: "noreply@org.com".into(),
                name: Some("Org Support".into())
            })
        );
    }

    #[test]
    fn user_workspace_sender_beats_user_flat_sender() {
        let store = LayeredInMem::new(
            &[],
            &[
                ("skills/sendgrid/from_email", "flat@org.com"),
                ("skills/sendgrid/from_name", "Flat"),
                ("skills/sendgrid/workspaces/acme/from_email", "acme@org.com"),
            ],
        );
        let client = SendGridClient::with_store(store, "acme", None).unwrap();
        assert_eq!(
            client.default_sender(),
            Some(Sender {
                email: "acme@org.com".into(),
                name: None
            })
        );
    }

    #[tokio::test]
    async fn status_codes_map_to_actionable_messages() {
        for (status, needle) in [
            (401, "check the SendGrid api_key"),
            (404, "Not found"),
            (413, "30 MB"),
            (500, "server error (500)"),
            (418, "API error (418)"),
        ] {
            let err = check_status(canned_response(status, None, ""))
                .await
                .unwrap_err()
                .to_string();
            assert!(err.contains(needle), "{status}: {err}");
            assert!(err.contains(&format!("HTTP {status}")), "{status}: {err}");
        }
    }

    #[test]
    fn store_resolution_reports_missing_key_with_the_path() {
        let client = SendGridClient::with_store(InMem::from(&[]), "acme", None).unwrap();
        let err = client.api_key().unwrap_err().to_string();
        assert!(
            err.contains("skills/sendgrid/workspaces/acme/api_key"),
            "{err}"
        );
        assert!(client.default_sender().is_none());
    }

    #[test]
    fn parse_error_renders_every_shape() {
        let status = StatusCode::BAD_REQUEST;
        assert_eq!(
            parse_error(
                r#"{"errors":[{"message":"The from email does not contain a valid address.","field":"from.email","help":"http://x"},{"message":"other","field":null}]}"#,
                status
            ),
            "The from email does not contain a valid address. (field: from.email); other"
        );
        assert_eq!(
            parse_error(
                r#"{"errors":[{"message":"authorization required","field":"null"}]}"#,
                status
            ),
            "authorization required"
        );
        assert_eq!(parse_error(r#"{"errors":[]}"#, status), r#"{"errors":[]}"#);
        assert_eq!(parse_error("not json", status), "not json");
        assert_eq!(parse_error("  ", StatusCode::NOT_FOUND), "HTTP 404");
    }

    #[test]
    fn rate_limit_reset_is_relative_to_now_and_floored() {
        let mut headers = HeaderMap::new();
        headers.insert(RATE_LIMIT_RESET_HEADER, HeaderValue::from_static("1000"));
        assert_eq!(rate_limit_reset_secs(&headers, 990), Some(10));
        assert_eq!(rate_limit_reset_secs(&headers, 1500), Some(0));
        headers.insert(RATE_LIMIT_RESET_HEADER, HeaderValue::from_static("soon"));
        assert_eq!(rate_limit_reset_secs(&headers, 0), None);
        assert_eq!(rate_limit_reset_secs(&HeaderMap::new(), 0), None);
    }

    #[tokio::test]
    async fn a_429_carries_the_typed_rate_limit_from_retry_after() {
        let err = check_status(canned_response(
            429,
            Some("7"),
            r#"{"errors":[{"message":"too many requests"}]}"#,
        ))
        .await
        .unwrap_err();
        let limit = err
            .downcast_ref::<RateLimitedError>()
            .expect("typed rate-limit signal");
        assert_eq!(limit.retry_after_secs, Some(7));
        assert_eq!(limit.message, "Rate limited by SendGrid: too many requests");
    }

    #[tokio::test]
    async fn a_429_falls_back_to_the_rate_limit_reset_epoch() {
        let reset = now_secs() + 30;
        let response = reqwest::Response::from(
            http::Response::builder()
                .status(429)
                .header("X-RateLimit-Reset", reset.to_string())
                .body("")
                .unwrap(),
        );
        let err = check_status(response).await.unwrap_err();
        let hint = err
            .downcast_ref::<RateLimitedError>()
            .unwrap()
            .retry_after_secs
            .unwrap();
        assert!((28..=30).contains(&hint), "{hint}");
    }

    #[tokio::test]
    async fn a_403_explains_scope_and_sender_verification() {
        let err = check_status(canned_response(
            403,
            None,
            r#"{"errors":[{"message":"The from address does not match a verified Sender Identity.","field":"from"}]}"#,
        ))
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("verified sender identity"), "{err}");
        assert!(err.contains("(field: from)"), "{err}");
    }

    #[tokio::test]
    async fn empty_and_no_content_bodies_are_null() {
        assert_eq!(
            json_body(canned_response(204, None, "")).await.unwrap(),
            Value::Null
        );
        assert_eq!(
            json_body(canned_response(200, None, "  ")).await.unwrap(),
            Value::Null
        );
        assert_eq!(
            json_body(canned_response(200, None, r#"{"ok":true}"#))
                .await
                .unwrap(),
            json!({"ok": true})
        );
    }

    #[tokio::test]
    async fn mail_send_returns_the_queued_message_id() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v3/mail/send"))
            .and(header("authorization", "Bearer SG.key"))
            .and(header("content-type", "application/json"))
            .and(body_json(json!({ "subject": "hi" })))
            .respond_with(ResponseTemplate::new(202).insert_header("X-Message-Id", "abc123"))
            .mount(&server)
            .await;
        let client = SendGridClient::new("SG.key", None, Some(&server.uri())).unwrap();
        let out = client.mail_send(&json!({ "subject": "hi" })).await.unwrap();
        assert_eq!(out, json!({ "accepted": true, "message_id": "abc123" }));
    }

    #[tokio::test]
    async fn request_sends_query_and_parses_json() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v3/stats"))
            .and(wiremock::matchers::query_param("start_date", "2026-09-01"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!([{ "date": "2026-09-01" }])),
            )
            .mount(&server)
            .await;
        let client = SendGridClient::new("SG.key", None, Some(&server.uri())).unwrap();
        let out = client
            .get("/v3/stats", &[("start_date", "2026-09-01".to_string())])
            .await
            .unwrap();
        assert_eq!(out[0]["date"], "2026-09-01");
    }
}
