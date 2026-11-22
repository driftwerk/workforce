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
use crate::adapters::auth::{field_paths, host_for, resolve_credential};
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
        // Keyed by the App's identity too: a token minted with one App's
        // key is never handed to a call made with another's (or with the
        // same app id and a key that isn't its own).
        let key = token_key(app, owner, repo);
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
            return Err(AppError::NotInstalled {
                repo: format!("{owner}/{repo}"),
            });
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
                self.installation_tokens
                    .forget(&token_key(&app, owner, repo));
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
                let base_url = host_for(store.as_ref(), &token, &url_ws, &url_flat)
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

    /// POST a GraphQL query and return its `data`. An answer carrying `errors`
    /// fails, partial `data` or not: a rate limit as [`RateLimitedError`],
    /// anything else as `GitHub GraphQL {types}: {messages}`.
    pub async fn graphql(&self, query: &str, variables: Value) -> Result<Value> {
        self.graphql_with(query, variables, false).await
    }

    /// [`Self::graphql`] for a `nodes(ids:)` read: an id that is gone is a
    /// `null` node with a `NOT_FOUND` error beside it, which leaves the rest
    /// read; any other error still fails.
    pub async fn graphql_nodes(&self, query: &str, variables: Value) -> Result<Value> {
        self.graphql_with(query, variables, true).await
    }

    async fn graphql_with(&self, query: &str, variables: Value, gone_ok: bool) -> Result<Value> {
        let (headers, base_url) = self.build_request()?;
        let response = self
            .client
            .post(graphql_url(&base_url))
            .headers(headers)
            .json(&serde_json::json!({ "query": query, "variables": variables }))
            .send()
            .await?;
        if !response.status().is_success() {
            return Self::handle_response(response).await;
        }
        let headers = response.headers();
        let exhausted = header_u64(headers, "x-ratelimit-remaining") == Some(0);
        let retry_after = retry_after_secs(headers).or_else(|| reset_in(headers));
        let mut body = response.json::<Value>().await?;
        let errors = body["errors"].as_array().filter(|e| !e.is_empty());
        let Some(errors) = errors else {
            return Ok(body["data"].take());
        };
        let types: Vec<&str> = errors
            .iter()
            .map(|e| e["type"].as_str().unwrap_or("UNKNOWN"))
            .collect();
        let messages: Vec<&str> = errors
            .iter()
            .filter_map(|e| e["message"].as_str())
            .collect();
        let message = messages.join("; ");
        if gone_ok
            && !exhausted
            && types.iter().all(|t| *t == "NOT_FOUND")
            && body["data"]["nodes"].is_array()
        {
            return Ok(body["data"].take());
        }
        if exhausted || types.contains(&"RATE_LIMITED") {
            return Err(RateLimitedError::new(
                format!("GitHub GraphQL rate limited: {message}"),
                retry_after,
            )
            .into());
        }
        let mut distinct: Vec<&str> = Vec::new();
        for t in types {
            if !distinct.contains(&t) {
                distinct.push(t);
            }
        }
        Err(eyre!("GitHub GraphQL {}: {message}", distinct.join(", ")))
    }

    /// Whether the token a call is made with now is the caller's own (a
    /// per-user tier of the vault), not a shared one.
    pub fn token_is_personal(&self) -> bool {
        let Some(store) = self.store.as_ref() else {
            return false;
        };
        let (ws, flat) = self.paths("token");
        resolve_credential(store.as_ref(), &ws, &flat).is_some_and(|r| r.per_user)
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
            Err(Self::failure(response).await)
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
            Err(Self::failure(response).await)
        }
    }

    /// A failed answer's error: a rate limit as [`RateLimitedError`], with
    /// how long GitHub says to wait ([`limited_for`]).
    async fn failure(response: reqwest::Response) -> eyre::Report {
        let status = response.status();
        let limited = limited_for(response.headers());
        let body = response.text().await.unwrap_or_default();
        let message = serde_json::from_str::<Value>(&body)
            .ok()
            .and_then(|v| v["message"].as_str().map(String::from))
            .unwrap_or(body);
        match (status.as_u16(), limited) {
            (401, _) => eyre!("Authentication failed: {}", message),
            (403, Some(retry_after)) => RateLimitedError::new(
                format!("Forbidden (rate limit or permissions): {}", message),
                retry_after,
            )
            .into(),
            (403, None) => eyre!("Forbidden (rate limit or permissions): {}", message),
            (404, _) => eyre!("Not found: {}", message),
            (422, _) => eyre!("Validation error: {}", message),
            (429, limited) => RateLimitedError::new(
                format!("GitHub API rate limited: {}", message),
                limited.flatten(),
            )
            .into(),
            _ => eyre!("GitHub API error ({}): {}", status, message),
        }
    }
}

/// The header `name` as a number, when it is one.
fn header_u64(headers: &HeaderMap, name: &str) -> Option<u64> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse().ok())
}

/// How long until GitHub's rate limit resets (`x-ratelimit-reset`): 0
/// once it has.
fn reset_in(headers: &HeaderMap) -> Option<u64> {
    header_u64(headers, "x-ratelimit-reset")
        .map(|reset| reset.saturating_sub(Utc::now().timestamp().max(0) as u64))
}

/// Whether `headers` say GitHub is rate-limiting the caller, and for how
/// long when they say: a `Retry-After` (a secondary limit), or the primary
/// limit spent (`x-ratelimit-remaining: 0`) until it resets.
fn limited_for(headers: &HeaderMap) -> Option<Option<u64>> {
    match (
        retry_after_secs(headers),
        header_u64(headers, "x-ratelimit-remaining"),
    ) {
        (Some(secs), _) => Some(Some(secs)),
        (None, Some(0)) => Some(reset_in(headers)),
        _ => None,
    }
}

/// The GraphQL endpoint of an API base: `{base}/graphql` for github.com, and
/// `/api/graphql` where the base ends in GitHub Enterprise Server's `/api/v3`.
pub fn graphql_url(base: &str) -> String {
    let base = base.trim_end_matches('/');
    match base.strip_suffix("/api/v3") {
        Some(host) => format!("{host}/api/graphql"),
        None => format!("{base}/graphql"),
    }
}

/// What GitHub last said of a key, by a non-secret stand-in for the key and
/// the host it calls ([`GithubClient::key_id`]): whether it reaches a
/// repository, and whether it may read check runs. Kept a while, so a
/// check isn't asked again on every call, and for every caller holding the
/// same key, since it is the key's own.
static KEY_FACTS: std::sync::LazyLock<
    parking_lot::Mutex<std::collections::HashMap<String, (bool, std::time::Instant)>>,
> = std::sync::LazyLock::new(Default::default);
/// The most key facts kept.
const KEY_FACTS_KEPT: usize = 4_096;
/// How long a key's fact is kept.
const KEY_FACT_FOR: std::time::Duration = std::time::Duration::from_secs(3_600);
/// How long a repository a key didn't reach is kept so: shorter, as the
/// person may give the key that repository.
const UNREACHED_FOR: std::time::Duration = std::time::Duration::from_secs(600);

fn key_fact(key: &str) -> Option<bool> {
    let facts = KEY_FACTS.lock();
    facts
        .get(key)
        .filter(|(_, until)| *until > std::time::Instant::now())
        .map(|(fact, _)| *fact)
}

fn keep_key_fact(key: String, fact: bool, kept_for: std::time::Duration) {
    let mut facts = KEY_FACTS.lock();
    if facts.len() >= KEY_FACTS_KEPT {
        facts.clear();
    }
    facts.insert(key, (fact, std::time::Instant::now() + kept_for));
}

/// GitHub's word that the key may not read what was asked (404, 401, or a
/// 403 that isn't a rate limit), as against GitHub not answering.
fn refused(e: &eyre::Report) -> bool {
    let message = e.to_string();
    // GitHub's own words, after what the status was taken for.
    let said = message.split_once(": ").map_or("", |(_, said)| said);
    e.downcast_ref::<RateLimitedError>().is_none()
        && !said.to_lowercase().contains("rate limit")
        && ["Not found:", "Authentication failed:", "Forbidden "]
            .iter()
            .any(|p| message.starts_with(p))
}

/// GitHub's word that the key can't read check runs at all: a personal
/// access token never may ("Resource not accessible by personal access
/// token"), nor an App without the Checks permission ("… by integration").
pub fn refuses_check_runs(e: &eyre::Report) -> bool {
    refused(e) && e.to_string().contains("not accessible by")
}

impl GithubClient {
    /// A non-secret stand-in for the key this client calls with now and the
    /// host it calls: `None` without a key.
    fn key_id(&self) -> Option<String> {
        use sha2::{Digest, Sha256};
        let (headers, base_url) = self.build_request().ok()?;
        let digest = Sha256::new()
            .chain_update(headers.get(AUTHORIZATION)?.as_bytes())
            .chain_update([0u8])
            .chain_update(base_url.as_bytes())
            .finalize();
        Some(digest.iter().take(16).map(|b| format!("{b:02x}")).collect())
    }

    /// Whether this client's key reaches `owner/repo`: as GitHub last said,
    /// else asked (`GET /repos/{owner}/{repo}`) and kept. An error only when
    /// GitHub couldn't say (a rate limit, the network).
    pub async fn reaches(&self, owner: &str, repo: &str) -> Result<bool> {
        let Some(id) = self.key_id() else {
            return Ok(false);
        };
        let key = format!(
            "{id} reaches {}/{}",
            owner.to_lowercase(),
            repo.to_lowercase()
        );
        if let Some(known) = key_fact(&key) {
            return Ok(known);
        }
        match self.get(&format!("/repos/{owner}/{repo}")).await {
            Ok(_) => {
                keep_key_fact(key, true, KEY_FACT_FOR);
                Ok(true)
            }
            Err(e) if refused(&e) => {
                keep_key_fact(key, false, UNREACHED_FOR);
                Ok(false)
            }
            Err(e) => Err(e),
        }
    }

    /// Whether GitHub refused this client's key check runs lately.
    pub fn check_runs_refused(&self) -> bool {
        self.key_id()
            .and_then(|id| key_fact(&format!("{id} no check runs")))
            .unwrap_or(false)
    }

    /// Keeps that GitHub refused this client's key check runs.
    pub fn refuse_check_runs(&self) {
        if let Some(id) = self.key_id() {
            keep_key_fact(format!("{id} no check runs"), true, KEY_FACT_FOR);
        }
    }
}

/// A non-secret stand-in for an App's identity: its id and a digest of its
/// key.
fn app_fingerprint(app: &AppCredentials) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::new()
        .chain_update(app.app_id.as_bytes())
        .chain_update([0u8])
        .chain_update(app.private_key_pem.as_bytes())
        .finalize();
    digest.iter().take(16).map(|b| format!("{b:02x}")).collect()
}

/// The installation-token cache key for `owner/repo` under `app`.
fn token_key(app: &AppCredentials, owner: &str, repo: &str) -> String {
    format!("{}/{owner}/{repo}", app_fingerprint(app))
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
    async fn a_403_with_the_primary_limit_spent_is_typed_until_its_reset() {
        let spent = |remaining: &str| {
            let reset = (Utc::now().timestamp() + 120).to_string();
            reqwest::Response::from(
                http::Response::builder()
                    .status(403)
                    .header("x-ratelimit-remaining", remaining)
                    .header("x-ratelimit-reset", reset)
                    .body(r#"{"message":"API rate limit exceeded for user ID 1."}"#)
                    .unwrap(),
            )
        };
        let err = GithubClient::handle_response(spent("0")).await.unwrap_err();
        let limited = err
            .downcast_ref::<RateLimitedError>()
            .expect("a spent primary limit is a rate limit");
        assert!(limited
            .retry_after_secs
            .is_some_and(|s| (110..=120).contains(&s)));
        let err = GithubClient::handle_response(spent("12"))
            .await
            .unwrap_err();
        assert!(err.downcast_ref::<RateLimitedError>().is_none());
    }

    #[tokio::test]
    async fn a_token_refused_check_runs_is_told_from_a_rate_limit() {
        let pat = canned_response(
            403,
            None,
            r#"{"message":"Resource not accessible by personal access token"}"#,
        );
        let pat = GithubClient::handle_response(pat).await.unwrap_err();
        assert!(refused(&pat) && refuses_check_runs(&pat));
        let limited = canned_response(
            403,
            None,
            r#"{"message":"You have exceeded a secondary rate limit"}"#,
        );
        let limited = GithubClient::handle_response(limited).await.unwrap_err();
        assert!(!refused(&limited) && !refuses_check_runs(&limited));
        let missing = canned_response(404, None, r#"{"message":"Not Found"}"#);
        let missing = GithubClient::handle_response(missing).await.unwrap_err();
        assert!(refused(&missing) && !refuses_check_runs(&missing));
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

        #[test]
        fn an_installation_token_is_kept_for_the_app_that_minted_it() {
            let mine = app();
            let other_key = AppCredentials {
                private_key_pem: format!("{}x", mine.private_key_pem),
                ..mine.clone()
            };
            assert_eq!(
                token_key(&mine, "org", "repo"),
                token_key(&app(), "org", "repo")
            );
            assert_ne!(
                token_key(&mine, "org", "repo"),
                token_key(&other_key, "org", "repo")
            );
        }

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
                &token_key(&app(), "org", "repo"),
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
