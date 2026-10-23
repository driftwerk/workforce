//! Per-target access-token cache with lazy, single-flight refresh. A
//! refresh runs under the backing store's refresh lock, keyed by where the
//! refresh token is written, so every caller sharing that store — in this
//! process or another — spends a refresh token at most once.
//!
//! Vault path layout is supplied by the caller via [`PathLayout`] so
//! existing providers can keep their established prefixes (Google
//! stays at `skills/google/workspaces/<workspace>/...` for backward
//! compatibility) while new providers (MCP) adopt versioned layouts
//! like `mcp/v1/<server_id>/...`.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use eyre::{eyre, Result};
use tracing::debug;

use crate::error::OAuthError;
use crate::flow::{OAuthClient, RefreshResult};
use workforce_common::settings::{SecretStore, VaultActor};

/// Refresh-window slack: refresh any token whose remaining lifetime is
/// less than this. 120 s handles clock drift, network latency, and the
/// occasional slow tool call that re-uses a token mid-flight without
/// fighting providers that round expiry to whole minutes.
pub const REFRESH_SLACK_SECS: u64 = 120;

/// Abstract clock so tests can inject a fake `now()`.
pub trait Clock: Send + Sync + std::fmt::Debug {
    fn now_unix(&self) -> u64;
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_unix(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }
}

/// How `TokenStore` builds vault paths from `(target, suffix)`. Each
/// provider supplies its own layout to preserve historical paths.
pub trait PathLayout: Send + Sync + std::fmt::Debug {
    fn path(&self, target: &str, suffix: &str) -> String;
}

pub struct TokenStore {
    store: Arc<dyn SecretStore>,
    target: String,
    layout: Arc<dyn PathLayout>,
    client: Arc<OAuthClient>,
    clock: Arc<dyn Clock>,
}

impl std::fmt::Debug for TokenStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokenStore")
            .field("target", &self.target)
            .field("layout", &self.layout)
            .finish()
    }
}

impl TokenStore {
    pub fn new(
        store: Arc<dyn SecretStore>,
        target: impl Into<String>,
        layout: Arc<dyn PathLayout>,
        client: Arc<OAuthClient>,
    ) -> Self {
        Self {
            store,
            target: target.into(),
            layout,
            client,
            clock: Arc::new(SystemClock),
        }
    }

    pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }

    pub fn target(&self) -> &str {
        &self.target
    }

    fn path(&self, suffix: &str) -> String {
        self.layout.path(&self.target, suffix)
    }

    fn read_access(&self) -> Option<(String, u64)> {
        let token = self.store.get(&self.path("access_token"))?;
        let expires_at = self
            .store
            .get(&self.path("expires_at"))
            .and_then(|s| s.parse::<u64>().ok())?;
        Some((token, expires_at))
    }

    fn fresh_access(&self) -> Option<String> {
        self.read_access()
            .filter(|(_, expires_at)| self.clock.now_unix() + REFRESH_SLACK_SECS < *expires_at)
            .map(|(token, _)| token)
    }

    fn read_refresh(&self) -> Result<String> {
        self.store.get(&self.path("refresh_token")).ok_or_else(|| {
            eyre!(
                "no refresh token at vault path `{}`",
                self.path("refresh_token")
            )
        })
    }

    /// Return a valid access token, refreshing if missing, expired, or
    /// within `REFRESH_SLACK_SECS` of expiry.
    pub async fn access_token(&self) -> Result<String> {
        if let Some((token, expires_at)) = self.read_access() {
            let now = self.clock.now_unix();
            if now + REFRESH_SLACK_SECS < expires_at {
                return Ok(token);
            }
            debug!(
                target = %self.target,
                now, expires_at,
                "access token near or past expiry; refreshing"
            );
        } else {
            debug!(target = %self.target, "no cached access token; refreshing");
        }
        self.refresh_now().await
    }

    /// Force a refresh regardless of cache state (used on 401 retries).
    pub async fn force_refresh(&self) -> Result<String> {
        debug!(target = %self.target, "forced token refresh");
        self.refresh_now().await
    }

    /// Whether a refresh token has been persisted for this target.
    pub fn is_authenticated(&self) -> bool {
        self.store.get(&self.path("refresh_token")).is_some()
    }

    pub fn status(&self) -> TokenStatus {
        let email = self.store.get(&self.path("email"));
        let has_refresh = self.store.get(&self.path("refresh_token")).is_some();
        let expires_at = self
            .store
            .get(&self.path("expires_at"))
            .and_then(|s| s.parse::<u64>().ok());
        let scopes = self.store.get(&self.path("scopes"));
        TokenStatus {
            target: self.target.clone(),
            email,
            has_refresh,
            expires_at,
            scopes,
        }
    }

    async fn refresh_now(&self) -> Result<String> {
        let refresh_path = self.path("refresh_token");
        let _guard = self
            .store
            .refresh_lock()
            .lock(&self.store.write_location(&refresh_path))
            .await?;

        // Another caller — here or in another process sharing the store —
        // may have refreshed while we waited: read what the store holds now.
        self.store.sync()?;
        if let Some(token) = self.fresh_access() {
            return Ok(token);
        }

        let refresh_token = self.read_refresh()?;
        let result = self
            .try_refresh(&refresh_token)
            .await
            .map_err(|e| self.wrap_refresh_error(e))?;

        // Recompute the lifetime against `SystemClock` (since the
        // upstream `expires_at` is wall-clock seconds) and project it
        // onto the injected clock so test fakes work consistently.
        // Refuse to persist a zero-lifetime token — that's a provider
        // bug; persisting it would put us in an infinite refresh loop.
        let lifetime = result.expires_at.saturating_sub(SystemClock.now_unix());
        if lifetime == 0 {
            return Err(eyre!(
                "provider returned token with zero remaining lifetime for `{}`",
                self.target
            ));
        }
        let expires_at = (self.clock.now_unix() + lifetime).to_string();

        // One write. Stores that apply it secret by secret get the rotated
        // refresh token first, so a crash never pairs a newer access token
        // with an already-spent refresh token.
        let (access_path, expires_path, scopes_path) = (
            self.path("access_token"),
            self.path("expires_at"),
            self.path("scopes"),
        );
        let entries: Vec<(&str, &str)> = result
            .refresh_token
            .as_deref()
            .map(|rotated| (refresh_path.as_str(), rotated))
            .into_iter()
            .chain([
                (access_path.as_str(), result.access_token.as_str()),
                (expires_path.as_str(), expires_at.as_str()),
            ])
            .chain(
                result
                    .scopes
                    .as_deref()
                    .map(|scopes| (scopes_path.as_str(), scopes)),
            )
            .collect();
        self.store
            .set_many(&entries, VaultActor::System("oauth_refresh"))?;

        Ok(result.access_token)
    }

    /// Call the upstream refresh endpoint with one retry on transport
    /// errors (network blip, TLS handshake glitch). Provider-side errors
    /// (e.g. `invalid_grant`) are terminal and surface immediately.
    async fn try_refresh(
        &self,
        refresh_token: &str,
    ) -> std::result::Result<RefreshResult, OAuthError> {
        match self.client.refresh(refresh_token).await {
            Ok(r) => Ok(r),
            Err(OAuthError::Transport(msg)) => {
                debug!(
                    target = %self.target,
                    error = %msg,
                    "transport error during token refresh; retrying once after 500ms",
                );
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                self.client.refresh(refresh_token).await
            }
            Err(other) => Err(other),
        }
    }

    /// Convert an `OAuthError` from the refresh path into an `eyre::Error`
    /// with a user-actionable message. `invalid_grant` is the common
    /// post-inactivity case (Google revokes refresh tokens after a
    /// password change or extended idle period); we surface it distinctly
    /// from generic refresh failures so the caller knows re-authorization
    /// is the only remedy.
    fn wrap_refresh_error(&self, e: OAuthError) -> eyre::Error {
        if let OAuthError::Provider {
            status,
            code,
            description,
        } = &e
        {
            if code == "invalid_grant" {
                return eyre!(
                    "OAuth refresh for `{}` rejected with `invalid_grant` (HTTP {}): {}. \
                     The refresh token is revoked — typically after extended inactivity, a \
                     password change, or a scope change. Re-authorize via the provider's \
                     setup flow.",
                    self.target,
                    status,
                    description.as_deref().unwrap_or("(no description)"),
                );
            }
        }
        eyre!("refreshing access token for `{}`: {}", self.target, e)
    }
}

#[derive(Debug, Clone)]
pub struct TokenStatus {
    pub target: String,
    pub email: Option<String>,
    pub has_refresh: bool,
    pub expires_at: Option<u64>,
    pub scopes: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::flow::OAuthClient;
    use crate::flow::OAuthClientCreds;
    use crate::provider::OAuthProvider;
    use async_trait::async_trait;
    use std::collections::HashMap;
    use std::sync::Mutex as StdMutex;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[derive(Debug, Default)]
    struct MemStore {
        map: StdMutex<HashMap<String, String>>,
        batches: StdMutex<Vec<(Vec<String>, String)>>,
    }

    impl SecretStore for MemStore {
        fn get(&self, p: &str) -> Option<String> {
            self.map.lock().unwrap().get(p).cloned()
        }
        fn set(&self, p: &str, v: &str) -> Result<()> {
            self.map.lock().unwrap().insert(p.into(), v.into());
            Ok(())
        }
        fn delete(&self, p: &str) -> Result<bool> {
            Ok(self.map.lock().unwrap().remove(p).is_some())
        }
        fn list(&self, prefix: Option<&str>) -> Vec<String> {
            let m = self.map.lock().unwrap();
            match prefix {
                Some(p) => m.keys().filter(|k| k.starts_with(p)).cloned().collect(),
                None => m.keys().cloned().collect(),
            }
        }
        fn set_many(&self, entries: &[(&str, &str)], actor: VaultActor<'_>) -> Result<()> {
            self.batches.lock().unwrap().push((
                entries.iter().map(|(p, _)| p.to_string()).collect(),
                actor.audit_string(),
            ));
            entries.iter().try_for_each(|(p, v)| self.set(p, v))
        }
    }

    /// A store another process has just refreshed: its `sync` brings the
    /// fresh token in.
    #[derive(Debug, Default)]
    struct RefreshedElsewhere(MemStore);

    impl SecretStore for RefreshedElsewhere {
        fn get(&self, p: &str) -> Option<String> {
            self.0.get(p)
        }
        fn set(&self, p: &str, v: &str) -> Result<()> {
            self.0.set(p, v)
        }
        fn delete(&self, p: &str) -> Result<bool> {
            self.0.delete(p)
        }
        fn list(&self, prefix: Option<&str>) -> Vec<String> {
            self.0.list(prefix)
        }
        fn sync(&self) -> Result<()> {
            self.0.set("p/default/access_token", "at-peer")?;
            self.0.set("p/default/expires_at", "999999")
        }
    }

    #[derive(Debug)]
    struct TestClock(StdMutex<u64>);
    impl Clock for TestClock {
        fn now_unix(&self) -> u64 {
            *self.0.lock().unwrap()
        }
    }

    #[derive(Debug)]
    struct FlatLayout {
        prefix: String,
    }
    impl PathLayout for FlatLayout {
        fn path(&self, target: &str, suffix: &str) -> String {
            format!("{}/{}/{}", self.prefix, target, suffix)
        }
    }

    struct TestProvider {
        endpoint: String,
    }
    #[async_trait]
    impl OAuthProvider for TestProvider {
        fn name(&self) -> &str {
            "test"
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

    fn client(server: &MockServer) -> Arc<OAuthClient> {
        Arc::new(OAuthClient::new(
            Arc::new(TestProvider {
                endpoint: server.uri(),
            }),
            OAuthClientCreds {
                client_id: "cid".into(),
                client_secret: Some("cs".into()),
                redirect_uri: None,
            },
            reqwest::Client::new(),
        ))
    }

    fn seed(store: &dyn SecretStore) {
        store.set("p/default/refresh_token", "rt-old").unwrap();
        store.set("p/default/access_token", "at-old").unwrap();
        store.set("p/default/expires_at", "1000").unwrap();
    }

    fn token_store(
        store: Arc<dyn SecretStore>,
        server: &MockServer,
        clock: &Arc<TestClock>,
    ) -> TokenStore {
        let layout: Arc<dyn PathLayout> = Arc::new(FlatLayout { prefix: "p".into() });
        TokenStore::new(store, "default", layout, client(server))
            .with_clock(clock.clone() as Arc<dyn Clock>)
    }

    fn fixtures(server: &MockServer) -> (Arc<MemStore>, TokenStore, Arc<TestClock>) {
        let store: Arc<MemStore> = Arc::new(MemStore::default());
        seed(store.as_ref());
        let clock = Arc::new(TestClock(StdMutex::new(100)));
        let ts = token_store(store.clone() as Arc<dyn SecretStore>, server, &clock);
        (store, ts, clock)
    }

    #[tokio::test]
    async fn fresh_token_returned_without_refresh() {
        let server = MockServer::start().await;
        // No mock — proves no HTTP call happens.
        let (_store, ts, _clock) = fixtures(&server);
        // now=100, expires_at=1000 → 900 s left, far above slack.
        let tok = ts.access_token().await.unwrap();
        assert_eq!(tok, "at-old");
    }

    #[tokio::test]
    async fn rotated_refresh_token_is_persisted_on_refresh() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "at-new",
                "refresh_token": "rt-new",
                "expires_in": 3600,
                "scope": "read write"
            })))
            .mount(&server)
            .await;
        let (store, ts, clock) = fixtures(&server);
        // Force expiry.
        *clock.0.lock().unwrap() = 2000;

        let tok = ts.access_token().await.unwrap();
        assert_eq!(tok, "at-new");
        assert_eq!(
            store.get("p/default/refresh_token").as_deref(),
            Some("rt-new"),
            "rotated RT must be persisted (regression test for the original Google bug)"
        );
        assert_eq!(
            store.get("p/default/access_token").as_deref(),
            Some("at-new")
        );
        assert_eq!(store.get("p/default/scopes").as_deref(), Some("read write"));
    }

    #[tokio::test]
    async fn a_refresh_is_written_as_one_batch_refresh_token_first() {
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
        let (store, ts, clock) = fixtures(&server);
        *clock.0.lock().unwrap() = 2000;

        ts.access_token().await.unwrap();
        let batches = store.batches.lock().unwrap();
        assert_eq!(
            *batches,
            vec![(
                vec![
                    "p/default/refresh_token".to_string(),
                    "p/default/access_token".to_string(),
                    "p/default/expires_at".to_string(),
                    "p/default/scopes".to_string(),
                ],
                "system:oauth_refresh".to_string()
            )]
        );
    }

    #[tokio::test]
    async fn a_token_refreshed_elsewhere_is_used_without_calling_the_provider() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(&server)
            .await;
        let store = Arc::new(RefreshedElsewhere::default());
        seed(store.as_ref());
        let clock = Arc::new(TestClock(StdMutex::new(2000)));
        let ts = token_store(store as Arc<dyn SecretStore>, &server, &clock);

        assert_eq!(ts.access_token().await.unwrap(), "at-peer");
    }

    #[tokio::test]
    async fn missing_rotated_rt_keeps_existing_one() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "at-new",
                "expires_in": 3600
            })))
            .mount(&server)
            .await;
        let (store, ts, clock) = fixtures(&server);
        *clock.0.lock().unwrap() = 2000;

        ts.access_token().await.unwrap();
        assert_eq!(
            store.get("p/default/refresh_token").as_deref(),
            Some("rt-old"),
            "no rotation → existing RT preserved"
        );
    }

    #[tokio::test]
    async fn concurrent_callers_share_a_single_refresh() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "at-new",
                "refresh_token": "rt-new",
                "expires_in": 3600
            })))
            .expect(1) // exactly one upstream request despite N concurrent callers
            .mount(&server)
            .await;
        let (_store, ts, clock) = fixtures(&server);
        *clock.0.lock().unwrap() = 2000;

        let ts = Arc::new(ts);
        let mut handles = Vec::new();
        for _ in 0..16 {
            let ts = Arc::clone(&ts);
            handles.push(tokio::spawn(
                async move { ts.access_token().await.unwrap() },
            ));
        }
        for h in handles {
            assert_eq!(h.await.unwrap(), "at-new");
        }
        // wiremock's `.expect(1)` will fail the test on drop if more
        // than one request landed.
    }

    #[tokio::test]
    async fn missing_refresh_token_errors_clearly() {
        let server = MockServer::start().await;
        let store: Arc<MemStore> = Arc::new(MemStore::default());
        let layout: Arc<dyn PathLayout> = Arc::new(FlatLayout { prefix: "p".into() });
        let ts = TokenStore::new(
            store as Arc<dyn SecretStore>,
            "default",
            layout,
            client(&server),
        );
        let err = ts.access_token().await.unwrap_err();
        assert!(err.to_string().contains("no refresh token"));
    }

    #[tokio::test]
    async fn invalid_grant_surfaces_actionable_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "error": "invalid_grant",
                "error_description": "Token has been expired or revoked."
            })))
            .mount(&server)
            .await;
        let (_store, ts, clock) = fixtures(&server);
        *clock.0.lock().unwrap() = 2000;

        let err = ts.access_token().await.unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("invalid_grant"), "msg: {msg}");
        assert!(msg.contains("Re-authorize"), "msg: {msg}");
        assert!(
            msg.contains("Token has been expired or revoked"),
            "msg: {msg}"
        );
    }

    #[tokio::test]
    async fn refresh_returning_zero_lifetime_is_rejected() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "at-bad",
                "expires_in": 0
            })))
            .mount(&server)
            .await;
        let (_store, ts, clock) = fixtures(&server);
        *clock.0.lock().unwrap() = 2000;
        let err = ts.access_token().await.unwrap_err();
        assert!(err.to_string().contains("zero remaining lifetime"));
    }
}
