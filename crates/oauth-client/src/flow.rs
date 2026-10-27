//! The redirect-flow primitives: build authorize URL, exchange code,
//! refresh. Provider-agnostic — endpoints come from the
//! [`OAuthProvider`] trait.
//!
//! Critical correctness property: [`RefreshResult::refresh_token`] is
//! `Option<String>` so a rotated refresh token is surfaced to the
//! caller (and persisted by [`crate::TokenStore`]). The pre-extraction
//! Google `refresh_access_token` returned only `(access, expires_at)`
//! and silently dropped any rotated RT — fine for Google in practice,
//! catastrophic for OAuth-2.1 providers like Linear and Atlassian
//! that rotate on every refresh.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use tracing::warn;

use crate::error::OAuthError;
use crate::pkce::{code_challenge_s256, gen_random_url_safe};
use crate::provider::{ClientAuth, OAuthProvider};

/// Operator-supplied OAuth client credentials. `client_secret` is
/// `None` for public clients (PKCE-only, e.g. dynamically-registered
/// clients with `token_endpoint_auth_method=none`); when `None` the
/// `client_secret` form field is omitted from token requests entirely.
/// `redirect_uri` is set only when the redirect (web) flow is used;
/// loopback and device flows leave it `None`.
#[derive(Debug, Clone)]
pub struct OAuthClientCreds {
    pub client_id: String,
    pub client_secret: Option<String>,
    pub redirect_uri: Option<String>,
}

/// Tokens persisted to the secret store after a successful exchange.
#[derive(Debug, Clone)]
pub struct StoredTokens {
    pub email: Option<String>,
    pub refresh_token: String,
    pub access_token: String,
    /// Unix seconds.
    pub expires_at: u64,
    pub scopes: String,
}

/// Output of [`OAuthClient::refresh`]. `refresh_token` is `Some` when
/// the provider rotated it (OAuth 2.1 default for confidential
/// clients) and the new value MUST be persisted; `None` means the
/// existing refresh token remains valid.
#[derive(Debug, Clone)]
pub struct RefreshResult {
    pub access_token: String,
    pub refresh_token: Option<String>,
    /// Unix seconds.
    pub expires_at: u64,
    pub scopes: Option<String>,
}

/// Output of [`OAuthClient::start_web_flow`].
#[derive(Debug, Clone)]
pub struct WebFlowStart {
    /// URL to redirect the user to.
    pub authorization_url: String,
    /// Opaque state token; the caller must store it (typically in an
    /// [`crate::OAuthStateStore`]) and verify on callback.
    pub state: String,
    /// PKCE verifier; the caller stores it alongside `state` for use
    /// in [`OAuthClient::exchange_web_code`].
    pub verifier: String,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<i64>,
    #[serde(default)]
    scope: Option<String>,
}

pub struct OAuthClient {
    provider: Arc<dyn OAuthProvider>,
    creds: OAuthClientCreds,
    http: reqwest::Client,
}

impl OAuthClient {
    pub fn new(
        provider: Arc<dyn OAuthProvider>,
        creds: OAuthClientCreds,
        http: reqwest::Client,
    ) -> Self {
        Self {
            provider,
            creds,
            http,
        }
    }

    pub fn provider(&self) -> &Arc<dyn OAuthProvider> {
        &self.provider
    }

    pub fn creds(&self) -> &OAuthClientCreds {
        &self.creds
    }

    /// Build a redirect-flow start (URL + state + verifier). Caller
    /// is responsible for sending the user to `authorization_url`
    /// and storing `state`/`verifier` for the callback.
    pub fn start_web_flow(&self, redirect_uri: &str, scopes: &[&str]) -> WebFlowStart {
        let (challenge, verifier) = if self.provider.uses_pkce() {
            let verifier = gen_random_url_safe();
            let challenge = code_challenge_s256(&verifier);
            (Some(challenge), verifier)
        } else {
            (None, String::new())
        };
        let state = gen_random_url_safe();
        let scope = if scopes.is_empty() {
            self.provider.default_scopes().join(" ")
        } else {
            scopes.join(" ")
        };
        let extras = self.provider.extra_authorize_params();
        let url = build_authorize_url(
            self.provider.authorization_endpoint(),
            &self.creds.client_id,
            redirect_uri,
            &scope,
            challenge.as_deref(),
            &state,
            &extras,
        );
        WebFlowStart {
            authorization_url: url,
            state,
            verifier,
        }
    }

    /// Exchange `code` (received on the callback) plus the original
    /// PKCE `verifier` for tokens.
    pub async fn exchange_web_code(
        &self,
        code: &str,
        verifier: &str,
        redirect_uri: &str,
    ) -> Result<StoredTokens, OAuthError> {
        let mut params = vec![
            ("code", code.to_string()),
            ("grant_type", "authorization_code".to_string()),
            ("redirect_uri", redirect_uri.to_string()),
        ];
        if self.provider.uses_pkce() {
            params.push(("code_verifier", verifier.to_string()));
        }
        let parsed = self.send_token_request(params).await?;
        let refresh_token = parsed.refresh_token.ok_or_else(|| {
            OAuthError::Malformed(format!(
                "{} did not return a refresh_token on code exchange",
                self.provider.name()
            ))
        })?;
        let expires_at = now_unix() + parsed.expires_in.unwrap_or(3600).max(0) as u64;
        let email = self
            .provider
            .fetch_userinfo(&self.http, &parsed.access_token)
            .await
            .unwrap_or_else(|e| {
                warn!(provider = self.provider.name(), error = %e, "userinfo lookup failed; continuing without email");
                None
            });
        Ok(StoredTokens {
            email,
            refresh_token,
            access_token: parsed.access_token,
            expires_at,
            scopes: parsed.scope.unwrap_or_default(),
        })
    }

    /// Exchange a refresh token for a new access token. Surfaces a
    /// rotated refresh token in [`RefreshResult::refresh_token`] when
    /// the provider returns one (caller MUST persist it).
    pub async fn refresh(&self, refresh_token: &str) -> Result<RefreshResult, OAuthError> {
        let params = vec![
            ("refresh_token", refresh_token.to_string()),
            ("grant_type", "refresh_token".to_string()),
        ];
        let parsed = self.send_token_request(params).await?;
        let expires_at = now_unix() + parsed.expires_in.unwrap_or(3600).max(0) as u64;
        Ok(RefreshResult {
            access_token: parsed.access_token,
            refresh_token: parsed.refresh_token,
            expires_at,
            scopes: parsed.scope,
        })
    }

    /// POST a grant request to the token endpoint, applying the
    /// provider's client-authentication scheme. `grant_params` carries
    /// only the grant-specific fields; client credentials are added here
    /// per [`ClientAuth`]: `RequestBody` appends `client_id` (+
    /// `client_secret` for confidential clients) to the form — public
    /// clients authenticate via PKCE alone — while `Basic` sets the HTTP
    /// Basic auth header and leaves credentials out of the body.
    async fn send_token_request(
        &self,
        mut grant_params: Vec<(&'static str, String)>,
    ) -> Result<TokenResponse, OAuthError> {
        let mut req = self.http.post(self.provider.token_endpoint());
        match self.provider.client_auth() {
            ClientAuth::RequestBody => {
                grant_params.push(("client_id", self.creds.client_id.clone()));
                if let Some(secret) = &self.creds.client_secret {
                    grant_params.push(("client_secret", secret.clone()));
                }
            }
            ClientAuth::Basic => {
                req = req.basic_auth(&self.creds.client_id, self.creds.client_secret.as_deref());
            }
        }
        let resp = req
            .form(&grant_params)
            .send()
            .await
            .map_err(OAuthError::from)?;
        let status = resp.status();
        let body = resp.text().await.map_err(OAuthError::from)?;
        if !status.is_success() {
            return Err(OAuthError::from_response(status.as_u16(), &body));
        }
        serde_json::from_str(&body).map_err(|e| OAuthError::Malformed(e.to_string()))
    }
}

fn build_authorize_url(
    endpoint: &str,
    client_id: &str,
    redirect_uri: &str,
    scope: &str,
    challenge: Option<&str>,
    state: &str,
    extras: &[(&str, &str)],
) -> String {
    let mut url = url::Url::parse(endpoint).expect("provider authorization_endpoint must be a URL");
    {
        let mut q = url.query_pairs_mut();
        q.append_pair("client_id", client_id)
            .append_pair("redirect_uri", redirect_uri)
            .append_pair("response_type", "code");
        if !scope.is_empty() {
            q.append_pair("scope", scope);
        }
        if let Some(challenge) = challenge {
            q.append_pair("code_challenge", challenge)
                .append_pair("code_challenge_method", "S256");
        }
        q.append_pair("state", state);
        for (k, v) in extras {
            q.append_pair(k, v);
        }
    }
    url.to_string()
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// Test-only provider whose `token_endpoint` is the wiremock URL.
    struct WiredProvider {
        endpoint: String,
    }

    #[async_trait]
    impl OAuthProvider for WiredProvider {
        fn name(&self) -> &str {
            "fake"
        }
        fn authorization_endpoint(&self) -> &str {
            "https://example.test/auth"
        }
        fn token_endpoint(&self) -> &str {
            &self.endpoint
        }
        fn default_scopes(&self) -> &[&str] {
            &["read"]
        }
    }

    /// Test-only provider shaped like FreeAgent: HTTP Basic client auth,
    /// no PKCE, no default scopes.
    struct BasicNoPkceProvider {
        endpoint: String,
    }

    #[async_trait]
    impl OAuthProvider for BasicNoPkceProvider {
        fn name(&self) -> &str {
            "basic"
        }
        fn authorization_endpoint(&self) -> &str {
            "https://example.test/approve_app"
        }
        fn token_endpoint(&self) -> &str {
            &self.endpoint
        }
        fn default_scopes(&self) -> &[&str] {
            &[]
        }
        fn uses_pkce(&self) -> bool {
            false
        }
        fn client_auth(&self) -> ClientAuth {
            ClientAuth::Basic
        }
    }

    fn client(server: &MockServer) -> OAuthClient {
        OAuthClient::new(
            Arc::new(WiredProvider {
                endpoint: server.uri(),
            }),
            OAuthClientCreds {
                client_id: "cid".into(),
                client_secret: Some("csecret".into()),
                redirect_uri: None,
            },
            reqwest::Client::new(),
        )
    }

    fn basic_client(server: &MockServer) -> OAuthClient {
        OAuthClient::new(
            Arc::new(BasicNoPkceProvider {
                endpoint: server.uri(),
            }),
            OAuthClientCreds {
                client_id: "cid".into(),
                client_secret: Some("csecret".into()),
                redirect_uri: Some("https://example.test/cb".into()),
            },
            reqwest::Client::new(),
        )
    }

    fn public_client(server: &MockServer) -> OAuthClient {
        OAuthClient::new(
            Arc::new(WiredProvider {
                endpoint: server.uri(),
            }),
            OAuthClientCreds {
                client_id: "cid".into(),
                client_secret: None,
                redirect_uri: None,
            },
            reqwest::Client::new(),
        )
    }

    #[tokio::test]
    async fn refresh_surfaces_rotated_refresh_token() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "at-new",
                "refresh_token": "rt-new",
                "expires_in": 3600,
                "scope": "read"
            })))
            .mount(&server)
            .await;
        let client = client(&server);

        let result = client.refresh("rt-old").await.unwrap();
        assert_eq!(result.access_token, "at-new");
        assert_eq!(
            result.refresh_token.as_deref(),
            Some("rt-new"),
            "rotated RT must be surfaced for caller to persist"
        );
    }

    #[tokio::test]
    async fn refresh_returns_none_rt_when_not_rotated() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "at-new",
                "expires_in": 3600
            })))
            .mount(&server)
            .await;
        let client = client(&server);

        let result = client.refresh("rt-old").await.unwrap();
        assert_eq!(result.access_token, "at-new");
        assert!(result.refresh_token.is_none());
    }

    #[tokio::test]
    async fn public_client_omits_client_secret_param() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "at",
                "refresh_token": "rt",
                "expires_in": 3600
            })))
            .mount(&server)
            .await;
        let client = public_client(&server);
        client.refresh("rt-old").await.unwrap();

        let reqs = server.received_requests().await.unwrap();
        let body = String::from_utf8_lossy(&reqs[0].body);
        assert!(body.contains("client_id=cid"));
        assert!(
            !body.contains("client_secret"),
            "public client must not send client_secret: {body}"
        );
    }

    #[tokio::test]
    async fn refresh_maps_provider_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "error": "invalid_grant",
                "error_description": "Token has been expired or revoked."
            })))
            .mount(&server)
            .await;
        let client = client(&server);

        let err = client.refresh("rt-bad").await.unwrap_err();
        match err {
            OAuthError::Provider {
                status,
                code,
                description,
            } => {
                assert_eq!(status, 400);
                assert_eq!(code, "invalid_grant");
                assert!(description.is_some());
            }
            _ => panic!("expected Provider error, got {err:?}"),
        }
    }

    #[tokio::test]
    async fn exchange_web_code_requires_refresh_token_in_response() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "at",
                "expires_in": 3600
                // no refresh_token
            })))
            .mount(&server)
            .await;
        let client = client(&server);

        let err = client
            .exchange_web_code("code", "verifier", "https://example.test/cb")
            .await
            .unwrap_err();
        match err {
            OAuthError::Malformed(m) => assert!(m.contains("did not return a refresh_token")),
            _ => panic!("expected Malformed, got {err:?}"),
        }
    }

    #[tokio::test]
    async fn basic_auth_refresh_uses_header_and_omits_body_creds() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "at-new",
                "refresh_token": "rt-new",
                "expires_in": 3600
            })))
            .mount(&server)
            .await;
        let client = basic_client(&server);
        client.refresh("rt-old").await.unwrap();

        let reqs = server.received_requests().await.unwrap();
        // cid:csecret base64-encoded → Y2lkOmNzZWNyZXQ=
        assert_eq!(
            reqs[0].headers.get("authorization").unwrap(),
            "Basic Y2lkOmNzZWNyZXQ="
        );
        let body = String::from_utf8_lossy(&reqs[0].body);
        assert!(body.contains("refresh_token=rt-old"), "{body}");
        assert!(
            !body.contains("client_id") && !body.contains("client_secret"),
            "Basic auth must keep credentials out of the body: {body}"
        );
    }

    #[tokio::test]
    async fn basic_auth_exchange_omits_pkce_verifier() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "at",
                "refresh_token": "rt",
                "expires_in": 3600
            })))
            .mount(&server)
            .await;
        let client = basic_client(&server);
        client
            .exchange_web_code("the-code", "unused-verifier", "https://example.test/cb")
            .await
            .unwrap();

        let reqs = server.received_requests().await.unwrap();
        assert!(reqs[0].headers.get("authorization").is_some());
        let body = String::from_utf8_lossy(&reqs[0].body);
        assert!(body.contains("code=the-code"), "{body}");
        assert!(body.contains("grant_type=authorization_code"), "{body}");
        assert!(
            !body.contains("code_verifier"),
            "non-PKCE provider must not send code_verifier: {body}"
        );
    }

    #[test]
    fn non_pkce_authorize_url_omits_challenge_and_empty_scope() {
        let server_uri = "http://unused.test";
        let client = OAuthClient::new(
            Arc::new(BasicNoPkceProvider {
                endpoint: server_uri.into(),
            }),
            OAuthClientCreds {
                client_id: "cid".into(),
                client_secret: Some("csecret".into()),
                redirect_uri: Some("https://example.test/cb".into()),
            },
            reqwest::Client::new(),
        );
        let started = client.start_web_flow("https://example.test/cb", &[]);
        assert!(
            !started.authorization_url.contains("code_challenge"),
            "{}",
            started.authorization_url
        );
        assert!(
            !started.authorization_url.contains("scope="),
            "empty scope must be omitted: {}",
            started.authorization_url
        );
        assert!(started.authorization_url.contains("response_type=code"));
        assert!(started.authorization_url.contains("state="));
        assert!(started.verifier.is_empty());
    }

    #[test]
    fn pkce_authorize_url_includes_challenge() {
        let client = OAuthClient::new(
            Arc::new(WiredProvider {
                endpoint: "http://unused.test".into(),
            }),
            OAuthClientCreds {
                client_id: "cid".into(),
                client_secret: Some("csecret".into()),
                redirect_uri: Some("https://example.test/cb".into()),
            },
            reqwest::Client::new(),
        );
        let started = client.start_web_flow("https://example.test/cb", &[]);
        assert!(started.authorization_url.contains("code_challenge"));
        assert!(started
            .authorization_url
            .contains("code_challenge_method=S256"));
        assert!(started.authorization_url.contains("scope=read"));
        assert!(!started.verifier.is_empty());
    }
}
