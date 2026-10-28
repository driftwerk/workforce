//! OAuth2 access-token source for Vertex AI.
//!
//! The Vertex client needs a bearer token on every request. We hide the
//! source behind a small async trait so tests can substitute a static token
//! without touching `gcp_auth`. In production [`LazyGcpAuthTokenSource`]
//! defers the `gcp_auth` provider construction until the first token fetch,
//! which lets the synchronous `create_provider` factory hand back a ready
//! client without an async runtime hop.
//!
//! `gcp_auth` caches tokens internally and refreshes them before expiry, so
//! no caching layer is added on top.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use eyre::{Result, WrapErr};
use gcp_auth::{CustomServiceAccount, TokenProvider};
use tokio::sync::OnceCell;

const VERTEX_SCOPE: &str = "https://www.googleapis.com/auth/cloud-platform";

/// Produces a bearer access token for Vertex AI requests.
#[async_trait]
pub trait VertexTokenSource: Send + Sync + 'static {
    async fn fetch_token(&self) -> Result<String>;
}

/// Holds the underlying gcp_auth `TokenProvider` and lazily constructs it on
/// first use so the synchronous factory can instantiate the client without
/// awaiting.
pub struct LazyGcpAuthTokenSource {
    init: ProviderInit,
    provider: OnceCell<Arc<dyn TokenProvider>>,
}

#[derive(Clone)]
enum ProviderInit {
    /// Walk the standard ADC chain at first use.
    Adc,
    /// Load a service-account JSON from the given path at first use.
    ServiceAccountFile(PathBuf),
    /// Parse a service-account key's JSON at first use.
    ServiceAccountKey(super::vertex_credentials::ServiceAccountKey),
}

impl LazyGcpAuthTokenSource {
    /// Use Application Default Credentials. The provider is built on the
    /// first call to [`fetch_token`].
    pub fn adc() -> Self {
        Self {
            init: ProviderInit::Adc,
            provider: OnceCell::new(),
        }
    }

    /// Use a specific service-account JSON file. The file is read on the
    /// first call to [`fetch_token`].
    pub fn from_service_account_file(path: impl Into<PathBuf>) -> Self {
        Self {
            init: ProviderInit::ServiceAccountFile(path.into()),
            provider: OnceCell::new(),
        }
    }

    /// Use a service-account key's JSON, as a vault account holds it. It is
    /// parsed on the first call to [`fetch_token`] and never written to disk.
    pub fn from_service_account_key(key: super::vertex_credentials::ServiceAccountKey) -> Self {
        Self {
            init: ProviderInit::ServiceAccountKey(key),
            provider: OnceCell::new(),
        }
    }

    async fn provider(&self) -> Result<&Arc<dyn TokenProvider>> {
        self.provider
            .get_or_try_init(|| async { build_provider(&self.init).await })
            .await
    }
}

async fn build_provider(init: &ProviderInit) -> Result<Arc<dyn TokenProvider>> {
    match init {
        ProviderInit::Adc => gcp_auth::provider()
            .await
            .wrap_err("failed to discover Google Application Default Credentials"),
        ProviderInit::ServiceAccountFile(path) => load_service_account(path),
        ProviderInit::ServiceAccountKey(key) => {
            let sa = CustomServiceAccount::from_json(key.json())
                .map_err(|_| eyre::eyre!("the Vertex service-account key could not be read"))?;
            Ok(Arc::new(sa))
        }
    }
}

fn load_service_account(path: &Path) -> Result<Arc<dyn TokenProvider>> {
    let sa = CustomServiceAccount::from_file(path)
        .wrap_err_with(|| format!("failed to load service account from {}", path.display()))?;
    Ok(Arc::new(sa))
}

#[async_trait]
impl VertexTokenSource for LazyGcpAuthTokenSource {
    async fn fetch_token(&self) -> Result<String> {
        let provider = self.provider().await?;
        let token = provider
            .token(&[VERTEX_SCOPE])
            .await
            .wrap_err("failed to fetch Vertex access token")?;
        Ok(token.as_str().to_string())
    }
}

/// Test-only token source that returns a fixed string and counts calls.
#[cfg(test)]
pub(crate) struct StaticTokenSource {
    token: String,
    calls: std::sync::atomic::AtomicUsize,
}

#[cfg(test)]
impl StaticTokenSource {
    pub fn new(token: impl Into<String>) -> Self {
        Self {
            token: token.into(),
            calls: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    pub fn calls(&self) -> usize {
        self.calls.load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[cfg(test)]
#[async_trait]
impl VertexTokenSource for StaticTokenSource {
    async fn fetch_token(&self) -> Result<String> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(self.token.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn static_token_source_returns_configured_token() {
        let src = StaticTokenSource::new("test-token-123");
        assert_eq!(src.fetch_token().await.unwrap(), "test-token-123");
        assert_eq!(src.calls(), 1);
        assert_eq!(src.fetch_token().await.unwrap(), "test-token-123");
        assert_eq!(src.calls(), 2);
    }

    #[tokio::test]
    async fn lazy_source_with_missing_sa_file_errors_on_first_fetch() {
        let source =
            LazyGcpAuthTokenSource::from_service_account_file("/definitely/not/a/path/key.json");
        let err = source.fetch_token().await.unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("service account"),
            "expected SA load error, got: {msg}"
        );
    }

    #[tokio::test]
    async fn lazy_source_returns_consistent_error_across_calls() {
        let source =
            LazyGcpAuthTokenSource::from_service_account_file("/definitely/not/a/path/key.json");
        let first = source.fetch_token().await.err().map(|e| format!("{e}"));
        let second = source.fetch_token().await.err().map(|e| format!("{e}"));
        assert_eq!(first, second, "errors must be deterministic across calls");
    }
}
