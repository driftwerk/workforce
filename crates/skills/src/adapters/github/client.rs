//! GitHub API HTTP client wrapper.

use std::sync::Arc;

use chrono::Utc;
use eyre::{eyre, Result};
use reqwest::header::{HeaderMap, HeaderValue, ACCEPT, AUTHORIZATION, USER_AGENT};
use serde_json::Value;
use workforce_agent::tools::RateLimitedError;
use workforce_common::settings::SecretStore;

use super::app_auth::{
    app_jwt, installation_id_of, installation_token_of, AppCredentials, AppError,
    InstallationToken, InstallationTokens,
};
use crate::adapters::auth::{field_paths, resolve_credential, resolve_host};
use crate::adapters::utils::retry_after_secs;

/// Thin wrapper around `reqwest::Client` with GitHub API authentication.
///
/// Construct via [`GithubClient::new`] for static credentials, or
/// [`GithubClient::with_store`] to resolve
/// `skills/github/workspaces/{workspace}/token` per request — falls back
/// to the bare `skills/github/token` path used by `projects/tools.rs`.
pub struct GithubClient {
    client: reqwest::Client,
    base_url: String,
    static_auth: Option<HeaderValue>,
    store: Option<Arc<dyn SecretStore>>,
    workspace: String,
    static_app: Option<AppCredentials>,
    installation_tokens: InstallationTokens,
}

impl GithubClient {
    /// Create a new GitHub client with a static PAT.
    pub fn new(token: &str, base_url: &str) -> Result<Self> {
        let auth = HeaderValue::from_str(&format!("Bearer {}", token))
            .map_err(|e| eyre!("Invalid token: {}", e))?;
        let client = reqwest::Client::builder().build()?;
        Ok(Self {
            client,
            base_url: base_url.trim_end_matches('/').to_string(),
            static_auth: Some(auth),
            store: None,
            workspace: String::new(),
            static_app: None,
            installation_tokens: InstallationTokens::default(),
        })
    }

    /// Give a static-credential client a GitHub App identity as well.
    pub fn with_app(mut self, app: AppCredentials) -> Self {
        self.static_app = Some(app);
        self
    }

    /// Create a GitHub client that resolves its PAT per request from
    /// `store`. Looks up
    /// `skills/github/workspaces/{workspace}/token` (with `{workspace}`
    /// being the configured workspace name); falls back to the bare
    /// `skills/github/token` for single-workspace deployments.
    pub fn with_store(
        store: Arc<dyn SecretStore>,
        workspace: &str,
        base_url: &str,
    ) -> Result<Self> {
        let client = reqwest::Client::builder().build()?;
        Ok(Self {
            client,
            base_url: base_url.trim_end_matches('/').to_string(),
            static_auth: None,
            store: Some(store),
            workspace: workspace.to_string(),
            static_app: None,
            installation_tokens: InstallationTokens::default(),
        })
    }

    fn paths(&self, field: &str) -> (String, String) {
        field_paths("github", "workspaces", &self.workspace, field)
    }

    /// The GitHub App identity, when one is configured: static, or
    /// `skills/github/workspaces/{workspace}/app_id` + `app_private_key` with
    /// the flat `skills/github/app_id` + `app_private_key` as fallback. The App
    /// belongs to the organisation, so a per-user copy is honoured but never
    /// required.
    fn app_credentials(&self) -> Option<AppCredentials> {
        if let Some(app) = &self.static_app {
            return Some(app.clone());
        }
        let store = self.store.as_ref()?;
        let (id_ws, id_flat) = self.paths("app_id");
        let (key_ws, key_flat) = self.paths("app_private_key");
        let app_id = resolve_credential(store.as_ref(), &id_ws, &id_flat)?;
        let key = resolve_credential(store.as_ref(), &key_ws, &key_flat)?;
        Some(AppCredentials {
            app_id: app_id.value.trim().to_string(),
            private_key_pem: key.value.trim().to_string(),
        })
    }

    fn headers_with(auth: HeaderValue) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, auth);
        headers.insert(
            ACCEPT,
            HeaderValue::from_static("application/vnd.github+json"),
        );
        headers.insert(USER_AGENT, HeaderValue::from_static("workforce"));
        headers.insert(
            "X-GitHub-Api-Version",
            HeaderValue::from_static("2022-11-28"),
        );
        headers
    }

    fn bearer(token: &str) -> Result<HeaderValue> {
        HeaderValue::from_str(&format!("Bearer {token}"))
            .map_err(|e| eyre!("Invalid GitHub token: {e}"))
    }

    /// An installation token for `owner/repo`, minted through the App's JWT
    /// and reused while it has time left.
    async fn installation_token(
        &self,
        app: &AppCredentials,
        owner: &str,
        repo: &str,
    ) -> std::result::Result<InstallationToken, AppError> {
        let key = format!("{owner}/{repo}");
        let now = Utc::now();
        if let Some(token) = self.installation_tokens.cached(&key, now) {
            return Ok(token);
        }
        let jwt = Self::bearer(&app_jwt(app, now)?)?;
        let lookup = self
            .client
            .get(format!(
                "{}/repos/{owner}/{repo}/installation",
                self.base_url
            ))
            .headers(Self::headers_with(jwt.clone()))
            .send()
            .await?;
        if lookup.status() == reqwest::StatusCode::NOT_FOUND {
            return Err(AppError::NotInstalled { repo: key });
        }
        let id = installation_id_of(&Self::handle_response(lookup).await?)?;
        let minted = self
            .client
            .post(format!(
                "{}/app/installations/{id}/access_tokens",
                self.base_url
            ))
            .headers(Self::headers_with(jwt))
            .json(&serde_json::json!({ "repositories": [repo] }))
            .send()
            .await?;
        let token = installation_token_of(&Self::handle_response(minted).await?)?;
        self.installation_tokens.remember(&key, token.clone());
        Ok(token)
    }

    /// POST as the GitHub App rather than the token. `Ok(None)` when no App
    /// is configured, so the caller can choose what to do without one. A
    /// cached installation token GitHub no longer honours (revoked, or the
    /// installation re-created) is forgotten and minted once more.
    pub async fn post_as_app(
        &self,
        owner: &str,
        repo: &str,
        path: &str,
        body: &Value,
    ) -> std::result::Result<Option<Value>, AppError> {
        let Some(app) = self.app_credentials() else {
            return Ok(None);
        };
        let mut reminted = false;
        loop {
            let token = self.installation_token(&app, owner, repo).await?;
            let response = self
                .client
                .post(format!("{}{path}", self.base_url))
                .headers(Self::headers_with(Self::bearer(&token.token)?))
                .json(body)
                .send()
                .await?;
            if response.status() == reqwest::StatusCode::UNAUTHORIZED && !reminted {
                self.installation_tokens.forget(&format!("{owner}/{repo}"));
                reminted = true;
                continue;
            }
            return Ok(Some(Self::handle_response(response).await?));
        }
    }

    fn build_request(&self) -> Result<(HeaderMap, String)> {
        let (auth, base_url) = match (&self.static_auth, &self.store) {
            (Some(h), _) => (h.clone(), self.base_url.clone()),
            (None, Some(store)) => {
                let (token_ws, token_flat) = self.paths("token");
                let token = resolve_credential(store.as_ref(), &token_ws, &token_flat).ok_or_else(
                    || eyre!("GitHub token not configured at {token_ws} or {token_flat}"),
                )?;
                let (url_ws, url_flat) = self.paths("base_url");
                let base_url = resolve_host(store.as_ref(), token.per_user, &url_ws, &url_flat)
                    .unwrap_or_else(|| self.base_url.clone());
                (Self::bearer(&token.value)?, base_url)
            }
            (None, None) => return Err(eyre!("GitHub client has no credential store")),
        };
        Ok((Self::headers_with(auth), base_url))
    }

    /// GET request to a GitHub API endpoint.
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

    /// POST request to a GitHub API endpoint.
    pub async fn post(&self, path: &str, body: &Value) -> Result<Value> {
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

    /// PUT request to a GitHub API endpoint.
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

    /// PATCH request to a GitHub API endpoint.
    pub async fn patch(&self, path: &str, body: &Value) -> Result<Value> {
        let (headers, base_url) = self.build_request()?;
        let url = format!("{base_url}{path}");
        let response = self
            .client
            .patch(&url)
            .headers(headers)
            .json(body)
            .send()
            .await?;
        Self::handle_response(response).await
    }

    /// DELETE request to a GitHub API endpoint.
    pub async fn delete(&self, path: &str) -> Result<Value> {
        let (headers, base_url) = self.build_request()?;
        let url = format!("{base_url}{path}");
        let response = self.client.delete(&url).headers(headers).send().await?;
        Self::handle_response(response).await
    }

    /// GET request that returns the response as plain text (for log endpoints).
    pub async fn get_text(&self, path: &str) -> Result<String> {
        let (headers, base_url) = self.build_request()?;
        let url = format!("{base_url}{path}");
        let response = self.client.get(&url).headers(headers).send().await?;
        let status = response.status();
        if status.is_success() {
            Ok(response.text().await?)
        } else {
            let retry_after = retry_after_secs(response.headers());
            let body = response.text().await.unwrap_or_default();
            let message = serde_json::from_str::<Value>(&body)
                .ok()
                .and_then(|v| v["message"].as_str().map(String::from))
                .unwrap_or(body);
            match status.as_u16() {
                401 => Err(eyre!("Authentication failed: {}", message)),
                403 if retry_after.is_some() => Err(RateLimitedError::new(
                    format!("Forbidden (rate limit or permissions): {}", message),
                    retry_after,
                )
                .into()),
                403 => Err(eyre!("Forbidden (rate limit or permissions): {}", message)),
                404 => Err(eyre!("Not found: {}", message)),
                429 => Err(RateLimitedError::new(
                    format!("GitHub API rate limited: {}", message),
                    retry_after,
                )
                .into()),
                _ => Err(eyre!("GitHub API error ({}): {}", status, message)),
            }
        }
    }

    /// Handle the HTTP response, mapping errors appropriately.
    async fn handle_response(response: reqwest::Response) -> Result<Value> {
        let status = response.status();

        if status.is_success() {
            // 204 No Content
            if status == reqwest::StatusCode::NO_CONTENT {
                return Ok(Value::Null);
            }
            let body = response.json::<Value>().await?;
            Ok(body)
        } else {
            let retry_after = retry_after_secs(response.headers());
            let body = response.text().await.unwrap_or_default();
            let message = serde_json::from_str::<Value>(&body)
                .ok()
                .and_then(|v| v["message"].as_str().map(String::from))
                .unwrap_or(body);

            match status.as_u16() {
                401 => Err(eyre!("Authentication failed: {}", message)),
                403 if retry_after.is_some() => Err(RateLimitedError::new(
                    format!("Forbidden (rate limit or permissions): {}", message),
                    retry_after,
                )
                .into()),
                403 => Err(eyre!("Forbidden (rate limit or permissions): {}", message)),
                404 => Err(eyre!("Not found: {}", message)),
                422 => Err(eyre!("Validation error: {}", message)),
                429 => Err(RateLimitedError::new(
                    format!("GitHub API rate limited: {}", message),
                    retry_after,
                )
                .into()),
                _ => Err(eyre!("GitHub API error ({}): {}", status, message)),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::test_support::canned_response;

    #[test]
    fn client_builds_with_valid_token() {
        let client = GithubClient::new("ghp_test123", "https://api.github.com");
        assert!(client.is_ok());
    }

    #[test]
    fn client_trims_trailing_slash() {
        let client = GithubClient::new("ghp_test", "https://api.github.com/").unwrap();
        assert_eq!(client.base_url, "https://api.github.com");
    }

    use crate::adapters::test_support::{InMem, LayeredInMem};

    #[test]
    fn build_headers_falls_back_to_flat_path_when_workspace_missing() {
        let store = InMem::from(&[("skills/github/token", "ghp_flat_token")]);
        let client = GithubClient::with_store(store, "myorg", "https://api.github.com").unwrap();
        let (headers, base_url) = client.build_request().unwrap();
        assert_eq!(headers.get(AUTHORIZATION).unwrap(), "Bearer ghp_flat_token");
        assert_eq!(base_url, "https://api.github.com");
    }

    #[test]
    fn build_headers_prefers_workspace_path_over_flat() {
        let store = InMem::from(&[
            ("skills/github/token", "flat_token"),
            ("skills/github/workspaces/myorg/token", "ws_token"),
        ]);
        let client = GithubClient::with_store(store, "myorg", "https://api.github.com").unwrap();
        let (headers, _) = client.build_request().unwrap();
        assert_eq!(headers.get(AUTHORIZATION).unwrap(), "Bearer ws_token");
    }

    #[test]
    fn a_shared_token_ignores_a_per_user_base_url() {
        let store = LayeredInMem::new(
            &[("skills/github/token", "shared_token")],
            &[(
                "skills/github/workspaces/myorg/base_url",
                "https://attacker.example",
            )],
        );
        let client = GithubClient::with_store(store, "myorg", "https://api.github.com").unwrap();
        let (_, base_url) = client.build_request().unwrap();
        assert_eq!(base_url, "https://api.github.com");
    }

    #[test]
    fn a_per_user_token_may_use_its_own_enterprise_host() {
        let store = LayeredInMem::new(
            &[("skills/github/token", "shared_token")],
            &[
                ("skills/github/workspaces/myorg/token", "my_token"),
                (
                    "skills/github/workspaces/myorg/base_url",
                    "https://ghe.me.example/api/v3/",
                ),
            ],
        );
        let client = GithubClient::with_store(store, "myorg", "https://api.github.com").unwrap();
        let (headers, base_url) = client.build_request().unwrap();
        assert_eq!(headers.get(AUTHORIZATION).unwrap(), "Bearer my_token");
        assert_eq!(base_url, "https://ghe.me.example/api/v3");
    }

    #[tokio::test]
    async fn handle_response_429_maps_to_typed_rate_limit() {
        let resp = canned_response(429, Some("30"), r#"{"message":"API rate limit exceeded"}"#);
        let err = GithubClient::handle_response(resp).await.unwrap_err();
        let limited = err
            .downcast_ref::<RateLimitedError>()
            .expect("429 must stay typed");
        assert_eq!(limited.retry_after_secs, Some(30));
        assert_eq!(
            limited.message,
            "GitHub API rate limited: API rate limit exceeded"
        );
    }

    #[tokio::test]
    async fn handle_response_403_with_retry_after_is_typed_secondary_limit() {
        let resp = canned_response(
            403,
            Some("60"),
            r#"{"message":"You have exceeded a secondary rate limit"}"#,
        );
        let err = GithubClient::handle_response(resp).await.unwrap_err();
        let limited = err
            .downcast_ref::<RateLimitedError>()
            .expect("403 with Retry-After is a secondary rate limit");
        assert_eq!(limited.retry_after_secs, Some(60));
        assert_eq!(
            limited.message,
            "Forbidden (rate limit or permissions): You have exceeded a secondary rate limit"
        );
    }

    #[tokio::test]
    async fn handle_response_plain_403_stays_untyped() {
        let resp = canned_response(403, None, r#"{"message":"Resource not accessible"}"#);
        let err = GithubClient::handle_response(resp).await.unwrap_err();
        assert!(err.downcast_ref::<RateLimitedError>().is_none());
        assert_eq!(
            err.to_string(),
            "Forbidden (rate limit or permissions): Resource not accessible"
        );
    }

    #[tokio::test]
    async fn get_text_429_maps_to_typed_rate_limit() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/logs"))
            .respond_with(
                ResponseTemplate::new(429)
                    .insert_header("Retry-After", "12")
                    .set_body_string("slow down"),
            )
            .mount(&server)
            .await;

        let client = GithubClient::new("ghp_test", &server.uri()).unwrap();
        let err = client.get_text("/logs").await.unwrap_err();
        let limited = err
            .downcast_ref::<RateLimitedError>()
            .expect("429 must stay typed");
        assert_eq!(limited.retry_after_secs, Some(12));
        assert_eq!(limited.message, "GitHub API rate limited: slow down");
    }

    #[tokio::test]
    async fn get_text_403_with_retry_after_is_typed() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/logs"))
            .respond_with(
                ResponseTemplate::new(403)
                    .insert_header("Retry-After", "7")
                    .set_body_string("secondary limit"),
            )
            .mount(&server)
            .await;

        let client = GithubClient::new("ghp_test", &server.uri()).unwrap();
        let err = client.get_text("/logs").await.unwrap_err();
        let limited = err.downcast_ref::<RateLimitedError>().unwrap();
        assert_eq!(limited.retry_after_secs, Some(7));
    }

    mod app_identity {
        use super::super::*;
        use crate::adapters::github::app_auth::test_keys;
        use crate::adapters::test_support::InMem;
        use serde_json::json;
        use wiremock::matchers::{body_json, header_regex, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        fn app() -> AppCredentials {
            AppCredentials {
                app_id: "4242".into(),
                private_key_pem: test_keys::PRIVATE_PEM.into(),
            }
        }

        fn far_future() -> String {
            (Utc::now() + chrono::Duration::hours(1)).to_rfc3339()
        }

        #[tokio::test]
        async fn a_review_goes_out_with_a_minted_installation_token_and_the_token_is_reused() {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/repos/org/repo/installation"))
                .and(header_regex("authorization", "^Bearer ey"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "id": 77 })))
                .expect(1)
                .mount(&server)
                .await;
            Mock::given(method("POST"))
                .and(path("/app/installations/77/access_tokens"))
                .and(body_json(json!({ "repositories": ["repo"] })))
                .respond_with(ResponseTemplate::new(201).set_body_json(json!({
                    "token": "ghs_installation",
                    "expires_at": far_future()
                })))
                .expect(1)
                .mount(&server)
                .await;
            Mock::given(method("POST"))
                .and(path("/repos/org/repo/pulls/9/reviews"))
                .and(header_regex("authorization", "^Bearer ghs_installation$"))
                .and(body_json(
                    json!({ "event": "APPROVE", "commit_id": "abc123", "body": "ok" }),
                ))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "id": 1, "state": "APPROVED", "commit_id": "abc123"
                })))
                .expect(2)
                .mount(&server)
                .await;
            let client = GithubClient::new("ghp_pat", &server.uri())
                .unwrap()
                .with_app(app());
            let body = json!({ "event": "APPROVE", "commit_id": "abc123", "body": "ok" });
            for _ in 0..2 {
                let review = client
                    .post_as_app("org", "repo", "/repos/org/repo/pulls/9/reviews", &body)
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(review["state"], "APPROVED");
            }
        }

        #[tokio::test]
        async fn a_token_github_stopped_honouring_is_minted_again_once() {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/repos/org/repo/installation"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "id": 5 })))
                .expect(1)
                .mount(&server)
                .await;
            Mock::given(method("POST"))
                .and(path("/app/installations/5/access_tokens"))
                .respond_with(ResponseTemplate::new(201).set_body_json(json!({
                    "token": "ghs_fresh",
                    "expires_at": far_future()
                })))
                .expect(1)
                .mount(&server)
                .await;
            Mock::given(method("POST"))
                .and(path("/repos/org/repo/pulls/9/reviews"))
                .and(header_regex("authorization", "^Bearer ghs_revoked$"))
                .respond_with(
                    ResponseTemplate::new(401)
                        .set_body_json(json!({ "message": "Bad credentials" })),
                )
                .expect(1)
                .mount(&server)
                .await;
            Mock::given(method("POST"))
                .and(path("/repos/org/repo/pulls/9/reviews"))
                .and(header_regex("authorization", "^Bearer ghs_fresh$"))
                .respond_with(
                    ResponseTemplate::new(200).set_body_json(json!({ "state": "APPROVED" })),
                )
                .expect(1)
                .mount(&server)
                .await;
            let client = GithubClient::new("ghp_pat", &server.uri())
                .unwrap()
                .with_app(app());
            client.installation_tokens.remember(
                "org/repo",
                InstallationToken {
                    token: "ghs_revoked".into(),
                    expires_at: Utc::now() + chrono::Duration::hours(1),
                },
            );
            let review = client
                .post_as_app("org", "repo", "/repos/org/repo/pulls/9/reviews", &json!({}))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(review["state"], "APPROVED");
        }

        #[tokio::test]
        async fn a_repository_outside_the_installation_is_reported_not_installed() {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/repos/org/elsewhere/installation"))
                .respond_with(
                    ResponseTemplate::new(404).set_body_json(json!({ "message": "Not Found" })),
                )
                .mount(&server)
                .await;
            let client = GithubClient::new("ghp_pat", &server.uri())
                .unwrap()
                .with_app(app());
            let err = client
                .post_as_app(
                    "org",
                    "elsewhere",
                    "/repos/org/elsewhere/pulls/1/reviews",
                    &json!({}),
                )
                .await
                .unwrap_err();
            assert!(matches!(err, AppError::NotInstalled { ref repo } if repo == "org/elsewhere"));
        }

        #[tokio::test]
        async fn without_an_app_the_call_reports_none_and_touches_nothing() {
            let server = MockServer::start().await;
            let client = GithubClient::new("ghp_pat", &server.uri()).unwrap();
            let out = client
                .post_as_app("org", "repo", "/repos/org/repo/pulls/1/reviews", &json!({}))
                .await
                .unwrap();
            assert!(out.is_none());
            assert!(server.received_requests().await.unwrap().is_empty());
        }

        #[test]
        fn a_users_own_app_copy_wins_over_the_shared_one_as_the_token_does() {
            let store = crate::adapters::test_support::LayeredInMem::new(
                &[
                    ("skills/github/app_id", "shared"),
                    ("skills/github/app_private_key", "shared-key"),
                ],
                &[
                    ("skills/github/app_id", "mine\n"),
                    ("skills/github/app_private_key", "\nmine-key\n"),
                ],
            );
            let client = GithubClient::with_store(store, "org", "https://api.github.com").unwrap();
            let creds = client.app_credentials().unwrap();
            assert_eq!(
                (creds.app_id.as_str(), creds.private_key_pem.as_str()),
                ("mine", "mine-key")
            );
        }

        #[test]
        fn app_credentials_come_from_the_workspace_path_then_the_flat_path() {
            let store = InMem::from(&[
                ("skills/github/token", "flat_token"),
                ("skills/github/app_id", " 100 "),
                ("skills/github/app_private_key", "flat-key"),
                ("skills/github/workspaces/org/app_id", "200"),
                ("skills/github/workspaces/org/app_private_key", "ws-key"),
            ]);
            let ws = GithubClient::with_store(store, "org", "https://api.github.com").unwrap();
            let creds = ws.app_credentials().unwrap();
            assert_eq!(
                (creds.app_id.as_str(), creds.private_key_pem.as_str()),
                ("200", "ws-key")
            );
            let store = InMem::from(&[
                ("skills/github/app_id", " 100 "),
                ("skills/github/app_private_key", "flat-key"),
            ]);
            let flat = GithubClient::with_store(store, "other", "https://api.github.com").unwrap();
            let creds = flat.app_credentials().unwrap();
            assert_eq!(
                (creds.app_id.as_str(), creds.private_key_pem.as_str()),
                ("100", "flat-key")
            );
            let none = GithubClient::with_store(
                InMem::from(&[] as &[(&str, &str)]),
                "other",
                "https://api.github.com",
            )
            .unwrap();
            assert!(none.app_credentials().is_none());
        }
    }
}
