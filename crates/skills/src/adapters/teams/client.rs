//! Outbound Bot Framework connector client.
//!
//! Sends, updates, and deletes activities against a conversation's `serviceUrl`,
//! authenticating with a cached client-credentials token
//! (<https://learn.microsoft.com/azure/bot-service/rest-api/bot-framework-rest-connector-authentication>).
//! The `serviceUrl` is per-conversation and comes from the inbound activity
//! (carried on a [`ConversationReference`]); the `TeamsSurface` (later increment)
//! resolves it for the generic `ChannelSurface` send path.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use eyre::{eyre, Result};
use serde::Deserialize;
use workforce_agent::tools::RateLimitedError;

use crate::adapters::utils::retry_after_secs;

use super::types::{Activity, ConversationReference};

/// Multi-tenant connector token endpoint (single-tenant bots override per tenant).
const TOKEN_URL: &str = "https://login.microsoftonline.com/botframework.com/oauth2/v2.0/token";
const TOKEN_SCOPE: &str = "https://api.botframework.com/.default";
/// Refresh the token this far ahead of its stated expiry.
const TOKEN_EXPIRY_SKEW: Duration = Duration::from_secs(60);

struct CachedToken {
    value: String,
    expires_at: Instant,
}

/// Calls the Bot Framework connector REST API on behalf of one bot registration.
pub struct BotFrameworkClient {
    http: reqwest::Client,
    app_id: String,
    app_password: String,
    token_url: String,
    token: Mutex<Option<CachedToken>>,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    expires_in: u64,
}

#[derive(Deserialize)]
struct ResourceResponse {
    id: String,
}

impl BotFrameworkClient {
    pub fn new(app_id: impl Into<String>, app_password: impl Into<String>) -> Self {
        Self {
            http: reqwest::Client::new(),
            app_id: app_id.into(),
            app_password: app_password.into(),
            token_url: TOKEN_URL.to_string(),
            token: Mutex::new(None),
        }
    }

    /// Override the token endpoint (single-tenant bots; tests).
    pub fn with_token_url(mut self, url: impl Into<String>) -> Self {
        self.token_url = url.into();
        self
    }

    /// Reply to the conversation a [`ConversationReference`] addresses with a plain
    /// text message, threaded under the referenced activity. Returns the new id.
    pub async fn reply_text(
        &self,
        reference: &ConversationReference,
        text: &str,
    ) -> Result<String> {
        let conversation_id = reference
            .conversation
            .id
            .as_deref()
            .ok_or_else(|| eyre!("conversation reference has no conversation id"))?;
        let activity = Activity {
            activity_type: "message".to_string(),
            text: Some(text.to_string()),
            reply_to_id: reference.activity_id.clone(),
            ..Activity::default()
        };
        self.send_activity(&reference.service_url, conversation_id, &activity)
            .await
    }

    /// `POST {service_url}/v3/conversations/{conversation_id}/activities` — returns
    /// the new activity id.
    pub async fn send_activity(
        &self,
        service_url: &str,
        conversation_id: &str,
        activity: &Activity,
    ) -> Result<String> {
        let resp: ResourceResponse = ensure_success(
            self.http
                .post(activities_url(service_url, conversation_id))
                .bearer_auth(self.token().await?)
                .json(activity)
                .send()
                .await?,
        )?
        .json()
        .await?;
        Ok(resp.id)
    }

    /// `PUT {service_url}/v3/conversations/{conversation_id}/activities/{activity_id}`.
    pub async fn update_activity(
        &self,
        service_url: &str,
        conversation_id: &str,
        activity_id: &str,
        activity: &Activity,
    ) -> Result<()> {
        ensure_success(
            self.http
                .put(activity_url(service_url, conversation_id, activity_id))
                .bearer_auth(self.token().await?)
                .json(activity)
                .send()
                .await?,
        )?;
        Ok(())
    }

    /// `DELETE {service_url}/v3/conversations/{conversation_id}/activities/{activity_id}`.
    pub async fn delete_activity(
        &self,
        service_url: &str,
        conversation_id: &str,
        activity_id: &str,
    ) -> Result<()> {
        ensure_success(
            self.http
                .delete(activity_url(service_url, conversation_id, activity_id))
                .bearer_auth(self.token().await?)
                .send()
                .await?,
        )?;
        Ok(())
    }

    /// A valid bearer token, refreshing via client-credentials when the cache is
    /// empty or within the expiry skew.
    async fn token(&self) -> Result<String> {
        if let Some(value) = self.cached_token() {
            return Ok(value);
        }
        let fetched = self.fetch_token().await?;
        let value = fetched.value.clone();
        *self.token.lock().expect("token mutex") = Some(fetched);
        Ok(value)
    }

    fn cached_token(&self) -> Option<String> {
        self.token
            .lock()
            .expect("token mutex")
            .as_ref()
            .filter(|t| t.expires_at > Instant::now())
            .map(|t| t.value.clone())
    }

    async fn fetch_token(&self) -> Result<CachedToken> {
        let resp: TokenResponse = ensure_success(
            self.http
                .post(&self.token_url)
                .form(&[
                    ("grant_type", "client_credentials"),
                    ("client_id", self.app_id.as_str()),
                    ("client_secret", self.app_password.as_str()),
                    ("scope", TOKEN_SCOPE),
                ])
                .send()
                .await?,
        )?
        .json()
        .await?;
        let ttl = Duration::from_secs(resp.expires_in).saturating_sub(TOKEN_EXPIRY_SKEW);
        Ok(CachedToken {
            value: resp.access_token,
            expires_at: Instant::now() + ttl,
        })
    }
}

/// [`reqwest::Response::error_for_status`] with a typed 429: rate-limit
/// refusals surface as [`RateLimitedError`] carrying the `Retry-After` hint,
/// every other status keeps reqwest's own error.
fn ensure_success(resp: reqwest::Response) -> Result<reqwest::Response> {
    if resp.status() != reqwest::StatusCode::TOO_MANY_REQUESTS {
        return Ok(resp.error_for_status()?);
    }
    let retry_after = retry_after_secs(resp.headers());
    let err = resp
        .error_for_status()
        .expect_err("429 always maps to an error");
    Err(RateLimitedError::new(err.to_string(), retry_after).into())
}

fn activities_url(service_url: &str, conversation_id: &str) -> String {
    format!(
        "{}/v3/conversations/{conversation_id}/activities",
        service_url.trim_end_matches('/')
    )
}

fn activity_url(service_url: &str, conversation_id: &str, activity_id: &str) -> String {
    format!(
        "{}/{activity_id}",
        activities_url(service_url, conversation_id)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::teams::types::ConversationAccount;
    use crate::adapters::test_support::canned_response;
    use wiremock::matchers::{body_string_contains, header, method, path, path_regex};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn client_for(server: &MockServer) -> BotFrameworkClient {
        BotFrameworkClient::new("app", "secret").with_token_url(format!("{}/token", server.uri()))
    }

    async fn mount_token(server: &MockServer, expect: u64) {
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "tok-123",
                "token_type": "Bearer",
                "expires_in": 3600
            })))
            .expect(expect)
            .mount(server)
            .await;
    }

    fn text_activity() -> Activity {
        Activity {
            activity_type: "message".into(),
            text: Some("hi".into()),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn send_activity_authenticates_and_returns_id() {
        let server = MockServer::start().await;
        mount_token(&server, 1).await;
        Mock::given(method("POST"))
            .and(path("/v3/conversations/19:thread/activities"))
            .and(header("authorization", "Bearer tok-123"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({ "id": "msg-1" })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let id = client_for(&server)
            .send_activity(&server.uri(), "19:thread", &text_activity())
            .await
            .unwrap();
        assert_eq!(id, "msg-1");
    }

    #[tokio::test]
    async fn token_is_fetched_once_and_reused() {
        let server = MockServer::start().await;
        mount_token(&server, 1).await; // exactly one token fetch across two sends
        Mock::given(method("POST"))
            .and(path_regex(r"/v3/conversations/.+/activities"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({ "id": "x" })),
            )
            .mount(&server)
            .await;

        let client = client_for(&server);
        client
            .send_activity(&server.uri(), "c1", &text_activity())
            .await
            .unwrap();
        client
            .send_activity(&server.uri(), "c2", &text_activity())
            .await
            .unwrap();
        // mount_token's expect(1) is asserted on server drop.
    }

    #[tokio::test]
    async fn reply_text_threads_under_referenced_activity() {
        let server = MockServer::start().await;
        mount_token(&server, 1).await;
        Mock::given(method("POST"))
            .and(path("/v3/conversations/19:c/activities"))
            .and(body_string_contains("replyToId"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({ "id": "r1" })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let reference = ConversationReference {
            activity_id: Some("a-parent".into()),
            user: None,
            bot: None,
            conversation: ConversationAccount {
                id: Some("19:c".into()),
                ..Default::default()
            },
            channel_id: Some("msteams".into()),
            service_url: server.uri(),
        };
        let id = client_for(&server)
            .reply_text(&reference, "echo")
            .await
            .unwrap();
        assert_eq!(id, "r1");
    }

    #[tokio::test]
    async fn token_endpoint_error_propagates() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;
        let err = client_for(&server)
            .send_activity(&server.uri(), "c1", &text_activity())
            .await;
        assert!(err.is_err());
    }

    #[test]
    fn ensure_success_429_is_typed_with_hint() {
        let err = ensure_success(canned_response(429, Some("5"), "busy")).unwrap_err();
        let limited = err
            .downcast_ref::<RateLimitedError>()
            .expect("429 must stay typed");
        assert_eq!(limited.retry_after_secs, Some(5));
        assert!(limited.message.contains("429"), "got: {}", limited.message);
    }

    #[test]
    fn ensure_success_429_without_header_carries_no_hint() {
        let err = ensure_success(canned_response(429, None, "busy")).unwrap_err();
        let limited = err.downcast_ref::<RateLimitedError>().unwrap();
        assert_eq!(limited.retry_after_secs, None);
    }

    #[test]
    fn ensure_success_other_error_defers_to_error_for_status() {
        let err = ensure_success(canned_response(500, None, "boom")).unwrap_err();
        assert!(err.downcast_ref::<RateLimitedError>().is_none());
    }

    #[test]
    fn ensure_success_passes_success_through() {
        assert!(ensure_success(canned_response(200, None, "ok")).is_ok());
    }
}
