//! Thin REST client that every Google service module uses.
//!
//! `GoogleApi` wraps a shared `reqwest::Client` + a per-workspace `TokenStore`.
//! Each helper:
//!
//! 1. Pulls a fresh access token via `TokenStore::access_token`.
//! 2. Sends the request with `Authorization: Bearer <token>`.
//! 3. On a 401, forces a single refresh and retries exactly once, then
//!    surfaces a friendly "re-authorize via google.setup" error.
//!
//! The helpers are intentionally *opinionated JSON* — they build URLs,
//! serialize request bodies, deserialize successes, and surface Google's
//! structured error envelope when something goes wrong. Service modules
//! call `api.get_json`, `api.post_json`, etc. and never touch reqwest
//! themselves.

use std::collections::HashMap;
use std::sync::Arc;

use eyre::{eyre, Result, WrapErr};
use reqwest::{header, Method, StatusCode};
use serde::de::DeserializeOwned;
use serde::Serialize;
use tracing::{debug, warn};
use workforce_agent::tools::RateLimitedError;

use crate::adapters::utils::retry_after_secs;

use super::token_store::TokenStore;

/// Shared HTTP client + token store for one workspace.
pub struct GoogleApi {
    http: reqwest::Client,
    tokens: TokenStore,
}

impl std::fmt::Debug for GoogleApi {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GoogleApi")
            .field("tokens", &self.tokens)
            .finish()
    }
}

impl GoogleApi {
    pub fn new(http: reqwest::Client, tokens: TokenStore) -> Self {
        Self { http, tokens }
    }

    pub fn workspace(&self) -> &str {
        self.tokens.workspace()
    }

    pub fn tokens(&self) -> &TokenStore {
        &self.tokens
    }

    pub fn http(&self) -> &reqwest::Client {
        &self.http
    }

    /// Build a request builder with the current access token attached.
    /// Does NOT refresh on 401 — callers should use the higher-level helpers.
    pub async fn authed(&self, method: Method, url: &str) -> Result<reqwest::RequestBuilder> {
        let token = self.tokens.access_token().await?;
        Ok(self.http.request(method, url).bearer_auth(token))
    }

    /// GET a JSON resource with automatic 401 → refresh → retry.
    pub async fn get_json<T: DeserializeOwned>(&self, url: &str) -> Result<T> {
        self.get_json_with_query::<T, ()>(url, None).await
    }

    /// GET with optional query parameters.
    pub async fn get_json_with_query<T, Q>(&self, url: &str, query: Option<&Q>) -> Result<T>
    where
        T: DeserializeOwned,
        Q: Serialize + ?Sized,
    {
        self.send_json(Method::GET, url, query, None::<&()>).await
    }

    pub async fn post_json<T, B>(&self, url: &str, body: &B) -> Result<T>
    where
        T: DeserializeOwned,
        B: Serialize + ?Sized,
    {
        self.send_json(Method::POST, url, None::<&()>, Some(body))
            .await
    }

    pub async fn post_json_with_query<T, B, Q>(
        &self,
        url: &str,
        query: Option<&Q>,
        body: Option<&B>,
    ) -> Result<T>
    where
        T: DeserializeOwned,
        B: Serialize + ?Sized,
        Q: Serialize + ?Sized,
    {
        self.send_json(Method::POST, url, query, body).await
    }

    pub async fn put_json<T, B>(&self, url: &str, body: &B) -> Result<T>
    where
        T: DeserializeOwned,
        B: Serialize + ?Sized,
    {
        self.send_json(Method::PUT, url, None::<&()>, Some(body))
            .await
    }

    pub async fn put_json_with_query<T, B, Q>(
        &self,
        url: &str,
        query: Option<&Q>,
        body: &B,
    ) -> Result<T>
    where
        T: DeserializeOwned,
        B: Serialize + ?Sized,
        Q: Serialize + ?Sized,
    {
        self.send_json(Method::PUT, url, query, Some(body)).await
    }

    pub async fn patch_json<T, B>(&self, url: &str, body: &B) -> Result<T>
    where
        T: DeserializeOwned,
        B: Serialize + ?Sized,
    {
        self.send_json(Method::PATCH, url, None::<&()>, Some(body))
            .await
    }

    pub async fn patch_json_with_query<T, B, Q>(
        &self,
        url: &str,
        query: Option<&Q>,
        body: &B,
    ) -> Result<T>
    where
        T: DeserializeOwned,
        B: Serialize + ?Sized,
        Q: Serialize + ?Sized,
    {
        self.send_json(Method::PATCH, url, query, Some(body)).await
    }

    pub async fn delete(&self, url: &str) -> Result<()> {
        self.delete_with_query::<()>(url, None).await
    }

    pub async fn delete_with_query<Q: Serialize + ?Sized>(
        &self,
        url: &str,
        query: Option<&Q>,
    ) -> Result<()> {
        let _: serde_json::Value = self
            .send_json_inner::<serde_json::Value, (), Q>(Method::DELETE, url, query, None, true)
            .await?;
        Ok(())
    }

    /// Core method with 401 refresh-retry and JSON deserialization.
    async fn send_json<T, B, Q>(
        &self,
        method: Method,
        url: &str,
        query: Option<&Q>,
        body: Option<&B>,
    ) -> Result<T>
    where
        T: DeserializeOwned,
        B: Serialize + ?Sized,
        Q: Serialize + ?Sized,
    {
        self.send_json_inner(method, url, query, body, false).await
    }

    async fn send_json_inner<T, B, Q>(
        &self,
        method: Method,
        url: &str,
        query: Option<&Q>,
        body: Option<&B>,
        allow_empty_body: bool,
    ) -> Result<T>
    where
        T: DeserializeOwned,
        B: Serialize + ?Sized,
        Q: Serialize + ?Sized,
    {
        // First attempt.
        let (status, retry_after, text) = self
            .send_once(&method, url, query, body)
            .await
            .wrap_err_with(|| format!("{method} {url}"))?;

        // 401 → force refresh and retry exactly once.
        if status == StatusCode::UNAUTHORIZED {
            debug!(%method, %url, "401 from Google; forcing token refresh and retrying");
            self.tokens
                .force_refresh()
                .await
                .wrap_err(
                    "token refresh after 401 failed — the refresh token is likely revoked; call google.setup(action='login', no_browser=true) to re-authorize",
                )?;
            let (retry_status, retry_after, retry_text) = self
                .send_once(&method, url, query, body)
                .await
                .wrap_err_with(|| format!("{method} {url} (retry after refresh)"))?;
            return Self::parse_response(retry_status, retry_after, retry_text, allow_empty_body);
        }

        Self::parse_response(status, retry_after, text, allow_empty_body)
    }

    async fn send_once<B, Q>(
        &self,
        method: &Method,
        url: &str,
        query: Option<&Q>,
        body: Option<&B>,
    ) -> Result<(StatusCode, Option<u64>, String)>
    where
        B: Serialize + ?Sized,
        Q: Serialize + ?Sized,
    {
        let mut req = self.authed(method.clone(), url).await?;
        if let Some(q) = query {
            req = req.query(q);
        }
        if let Some(b) = body {
            req = req.json(b);
        }
        let resp = req.send().await.wrap_err("http request failed")?;
        let status = resp.status();
        let retry_after = retry_after_secs(resp.headers());
        let text = resp.text().await.unwrap_or_default();
        Ok((status, retry_after, text))
    }

    fn status_error(status: StatusCode, retry_after: Option<u64>, text: &str) -> eyre::Report {
        let err = Self::decode_error(status, text);
        if status == StatusCode::TOO_MANY_REQUESTS {
            return RateLimitedError::new(err.to_string(), retry_after).into();
        }
        err
    }

    fn parse_response<T: DeserializeOwned>(
        status: StatusCode,
        retry_after: Option<u64>,
        text: String,
        allow_empty_body: bool,
    ) -> Result<T> {
        if !status.is_success() {
            return Err(Self::status_error(status, retry_after, &text));
        }
        if text.is_empty() && allow_empty_body {
            // DELETE and a few other endpoints return 204 with no body. We
            // give back `serde_json::Value::Null` so callers using `Value`
            // work. Typed callers who expect non-null will fail here, which
            // is the correct behavior.
            return serde_json::from_str("null").wrap_err("decoding empty response as null");
        }
        serde_json::from_str::<T>(&text).wrap_err_with(|| format!("decoding response body: {text}"))
    }

    fn decode_error(status: StatusCode, text: &str) -> eyre::Error {
        // Google's structured error envelope: {"error": {"code": N, "message": "...", "status": "..."}}
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(text) {
            if let Some(err) = value.get("error") {
                let message = err
                    .get("message")
                    .and_then(|m| m.as_str())
                    .unwrap_or("(no message)");
                let google_status = err.get("status").and_then(|s| s.as_str()).unwrap_or("");
                return eyre!(
                    "Google API error {}: {} ({})",
                    status,
                    message,
                    google_status
                );
            }
        }
        eyre!("Google API error {}: {}", status, text)
    }

    /// Poll a URL until it returns a successful status, retrying on 404 to
    /// absorb Google's eventual-consistency window after resource creation.
    ///
    /// Backoff steps through `BACKOFFS_MS` and stays at `STEADY_BACKOFF_MS`
    /// once exhausted. Any non-404 error status is terminal.
    pub async fn confirm_exists_with_retry(&self, url: &str, max_attempts: u32) -> Result<()> {
        use tokio::time::{sleep, Duration};
        const BACKOFFS_MS: [u64; 3] = [250, 500, 1000];
        const STEADY_BACKOFF_MS: u64 = 1000;
        for attempt in 0..max_attempts as usize {
            let status = self.head_status(url).await?;
            if status.is_success() {
                return Ok(());
            }
            if status != StatusCode::NOT_FOUND {
                return Err(eyre!(
                    "unexpected status {status} confirming `{url}` exists"
                ));
            }
            let ms = BACKOFFS_MS
                .get(attempt)
                .copied()
                .unwrap_or(STEADY_BACKOFF_MS);
            sleep(Duration::from_millis(ms)).await;
        }
        Err(eyre!(
            "resource at `{url}` did not become readable after {max_attempts} attempts"
        ))
    }

    /// GET `url` and return the response status, with one 401 → refresh →
    /// retry cycle. Body is discarded — used by existence checks where we
    /// only care about reachability.
    async fn head_status(&self, url: &str) -> Result<StatusCode> {
        let (status, _, _text) = self
            .send_once::<(), ()>(&Method::GET, url, None, None)
            .await
            .wrap_err_with(|| format!("GET {url} (existence check)"))?;
        if status == StatusCode::UNAUTHORIZED {
            self.tokens.force_refresh().await.wrap_err(
                "token refresh during existence check failed; re-authorize via google.setup",
            )?;
            let (retry_status, _, _retry_text) = self
                .send_once::<(), ()>(&Method::GET, url, None, None)
                .await
                .wrap_err_with(|| format!("GET {url} (existence check, retry after refresh)"))?;
            return Ok(retry_status);
        }
        Ok(status)
    }

    /// Upload raw bytes with a content-type header (used by Drive upload,
    /// Gmail attachment upload, etc.).
    pub async fn upload_bytes(
        &self,
        url: &str,
        query: &HashMap<&str, String>,
        content_type: &str,
        body: Vec<u8>,
    ) -> Result<serde_json::Value> {
        let token = self.tokens.access_token().await?;
        let resp = self
            .http
            .post(url)
            .bearer_auth(token)
            .query(query)
            .header(header::CONTENT_TYPE, content_type)
            .body(body)
            .send()
            .await
            .wrap_err("upload request failed")?;
        let status = resp.status();
        let retry_after = retry_after_secs(resp.headers());
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(Self::status_error(status, retry_after, &text));
        }
        serde_json::from_str(&text).wrap_err_with(|| format!("parsing upload response: {text}"))
    }

    /// Download raw bytes from a URL (used by Drive download, Gmail
    /// attachment download, Docs/Slides/Sheets export).
    pub async fn download_bytes(&self, url: &str) -> Result<Vec<u8>> {
        let token = self.tokens.access_token().await?;
        let resp = self
            .http
            .get(url)
            .bearer_auth(token)
            .send()
            .await
            .wrap_err("download request failed")?;
        let status = resp.status();
        let retry_after = retry_after_secs(resp.headers());
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            if status == StatusCode::UNAUTHORIZED {
                warn!("download returned 401; forcing refresh and retrying");
                self.tokens.force_refresh().await?;
                let token = self.tokens.access_token().await?;
                let resp2 = self
                    .http
                    .get(url)
                    .bearer_auth(token)
                    .send()
                    .await
                    .wrap_err("download retry failed")?;
                let retry_status = resp2.status();
                let retry_after = retry_after_secs(resp2.headers());
                if !retry_status.is_success() {
                    let t = resp2.text().await.unwrap_or_default();
                    return Err(Self::status_error(retry_status, retry_after, &t));
                }
                return resp2
                    .bytes()
                    .await
                    .map(|b| b.to_vec())
                    .wrap_err("reading bytes");
            }
            return Err(Self::status_error(status, retry_after, &text));
        }
        resp.bytes()
            .await
            .map(|b| b.to_vec())
            .wrap_err("reading bytes")
    }
}

/// Holds one or more `GoogleApi` instances for multi-account support.
///
/// Tools use `resolve(account)` to get the right client. When no `account`
/// is specified, the default client is used. Mirrors the shape of the old
/// `GoogleToolContext` so Phase 3 tools can drop in without churn.
pub struct GoogleApiContext {
    default: Arc<GoogleApi>,
    accounts: HashMap<String, Arc<GoogleApi>>,
}

impl GoogleApiContext {
    pub fn single(api: Arc<GoogleApi>) -> Self {
        Self {
            default: api,
            accounts: HashMap::new(),
        }
    }

    pub fn multi(default: Arc<GoogleApi>, accounts: HashMap<String, Arc<GoogleApi>>) -> Self {
        Self { default, accounts }
    }

    pub fn resolve(&self, account: Option<&str>) -> &GoogleApi {
        match account {
            Some(name) => self
                .accounts
                .get(name)
                .map(|a| a.as_ref())
                .unwrap_or(&self.default),
            None => &self.default,
        }
    }

    pub fn account_names(&self) -> Vec<&str> {
        self.accounts.keys().map(|s| s.as_str()).collect()
    }
}

/// Helper: build the shared `reqwest::Client` we use for all Google calls.
///
/// We set a generous timeout and identify ourselves via User-Agent so Google
/// has something sane to show in the Cloud Console access logs.
pub fn build_http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .user_agent(concat!("workforce/", env!("CARGO_PKG_VERSION")))
        .build()
        .wrap_err("building Google HTTP client")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_structured_google_error() {
        let body = r#"{"error":{"code":404,"message":"Not found","status":"NOT_FOUND"}}"#;
        let err = GoogleApi::decode_error(StatusCode::NOT_FOUND, body);
        let msg = format!("{err}");
        assert!(msg.contains("Not found"), "got: {msg}");
        assert!(msg.contains("NOT_FOUND"));
    }

    #[test]
    fn decode_unstructured_error_falls_back_to_raw_body() {
        let err = GoogleApi::decode_error(StatusCode::IM_A_TEAPOT, "<html>oops</html>");
        assert!(format!("{err}").contains("oops"));
    }

    #[test]
    fn parse_response_429_is_typed_with_hint() {
        use workforce_agent::tools::RateLimitedError;
        let body =
            r#"{"error":{"code":429,"message":"Quota exceeded","status":"RESOURCE_EXHAUSTED"}}"#;
        let err = GoogleApi::parse_response::<serde_json::Value>(
            StatusCode::TOO_MANY_REQUESTS,
            Some(12),
            body.to_string(),
            false,
        )
        .unwrap_err();
        let limited = err
            .downcast_ref::<RateLimitedError>()
            .expect("429 must stay typed");
        assert_eq!(limited.retry_after_secs, Some(12));
        assert_eq!(
            limited.message,
            "Google API error 429 Too Many Requests: Quota exceeded (RESOURCE_EXHAUSTED)"
        );
    }

    #[test]
    fn parse_response_429_without_header_carries_no_hint() {
        use workforce_agent::tools::RateLimitedError;
        let err = GoogleApi::parse_response::<serde_json::Value>(
            StatusCode::TOO_MANY_REQUESTS,
            None,
            "busy".to_string(),
            false,
        )
        .unwrap_err();
        let limited = err.downcast_ref::<RateLimitedError>().unwrap();
        assert_eq!(limited.retry_after_secs, None);
    }

    #[test]
    fn parse_response_non_429_error_stays_untyped() {
        use workforce_agent::tools::RateLimitedError;
        let err = GoogleApi::parse_response::<serde_json::Value>(
            StatusCode::NOT_FOUND,
            Some(12),
            "nope".to_string(),
            false,
        )
        .unwrap_err();
        assert!(err.downcast_ref::<RateLimitedError>().is_none());
    }

    #[test]
    fn build_http_client_ok() {
        assert!(build_http_client().is_ok());
    }
}
