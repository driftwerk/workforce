//! Credential sourcing for the Bedrock provider. Resolves to a per-request
//! snapshot from either a static packed string or the AWS default credential
//! chain (env, shared config, ECS task role, IRSA, IMDS). The chain variant
//! caches resolved credentials and refreshes them just before STS expiry, so
//! the signed-request hot path never re-walks the chain or re-hits IMDS.

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use aws_config::BehaviorVersion;
use aws_credential_types::provider::{ProvideCredentials, SharedCredentialsProvider};
use aws_credential_types::Credentials;
use eyre::{eyre, Result};
use tokio::sync::{Mutex, OnceCell};

const REFRESH_WINDOW: Duration = Duration::from_secs(60);

/// Sentinel `api_key` string that selects the AWS default credential chain
/// (env → shared config → ECS task role → IRSA → IMDS) instead of static
/// credentials. Callers that have no env var or vault entry should pass this
/// to `create_provider` for the Bedrock provider.
pub const CHAIN_SENTINEL: &str = "none";

#[derive(Clone)]
pub struct BedrockCredentials {
    pub access_key_id: String,
    pub secret_access_key: String,
    pub session_token: Option<String>,
}

impl std::fmt::Debug for BedrockCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BedrockCredentials")
            .field("access_key_id", &self.access_key_id)
            .field("secret_access_key", &"<redacted>")
            .field(
                "session_token",
                &self.session_token.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

impl BedrockCredentials {
    /// Resolve credentials from a packed `ACCESS_KEY:SECRET[:SESSION_TOKEN]`
    /// blob plus AWS environment variables. Any field not present in `packed`
    /// is filled in from `AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY` /
    /// `AWS_SESSION_TOKEN`. The sentinel `"none"` and empty string both mean
    /// env-only.
    pub fn parse_or_env(packed: &str) -> Result<Self> {
        Self::resolve(packed, |k| std::env::var(k).ok())
    }

    fn resolve<F>(packed: &str, env: F) -> Result<Self>
    where
        F: Fn(&str) -> Option<String>,
    {
        let parts: Vec<&str> = if packed.is_empty() || packed == CHAIN_SENTINEL {
            Vec::new()
        } else {
            packed.splitn(3, ':').collect()
        };
        let pick = |i: usize, var: &str| -> Option<String> {
            parts
                .get(i)
                .copied()
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .or_else(|| env(var))
        };
        let access_key_id = pick(0, "AWS_ACCESS_KEY_ID").ok_or_else(|| {
            eyre!("AWS_ACCESS_KEY_ID env not set and no api_key provided for Bedrock")
        })?;
        let secret_access_key = pick(1, "AWS_SECRET_ACCESS_KEY")
            .ok_or_else(|| eyre!("AWS_SECRET_ACCESS_KEY env not set for Bedrock"))?;
        let session_token = pick(2, "AWS_SESSION_TOKEN");
        Ok(Self {
            access_key_id,
            secret_access_key,
            session_token,
        })
    }
}

#[derive(Clone)]
pub enum CredentialResolver {
    Static(BedrockCredentials),
    Chain(Arc<ChainState>),
}

pub struct ChainState {
    provider: OnceCell<SharedCredentialsProvider>,
    cached: Mutex<Option<CachedCreds>>,
}

struct CachedCreds {
    creds: BedrockCredentials,
    expiry: Option<SystemTime>,
}

impl CredentialResolver {
    /// Prefer a static packed/env-derived credential set; fall back to the
    /// AWS default credential chain (env → shared config → ECS task role →
    /// IRSA → IMDS) when no complete static set is available.
    pub fn from_packed(packed: &str) -> Self {
        Self::from_packed_with_env(packed, |k| std::env::var(k).ok())
    }

    fn from_packed_with_env<F>(packed: &str, env: F) -> Self
    where
        F: Fn(&str) -> Option<String>,
    {
        match BedrockCredentials::resolve(packed, env) {
            Ok(creds) => Self::Static(creds),
            Err(_) => Self::Chain(Arc::new(ChainState::new(OnceCell::new()))),
        }
    }

    pub async fn resolve(&self) -> Result<BedrockCredentials> {
        match self {
            Self::Static(c) => Ok(c.clone()),
            Self::Chain(state) => state.resolve().await,
        }
    }

    /// Provide a `SharedCredentialsProvider` for handing to other AWS SDK
    /// clients (e.g. `aws-sdk-bedrock` control plane) so they share the same
    /// identity as the runtime SigV4 path without re-walking the chain.
    pub async fn shared_provider(&self) -> Result<SharedCredentialsProvider> {
        match self {
            Self::Static(creds) => Ok(SharedCredentialsProvider::new(Credentials::new(
                creds.access_key_id.clone(),
                creds.secret_access_key.clone(),
                creds.session_token.clone(),
                None,
                "workforce-bedrock-catalog",
            ))),
            Self::Chain(state) => state.shared_provider().await,
        }
    }

    #[cfg(test)]
    fn from_provider(provider: SharedCredentialsProvider) -> Self {
        Self::Chain(Arc::new(ChainState::new(OnceCell::new_with(Some(
            provider,
        )))))
    }
}

impl ChainState {
    fn new(provider: OnceCell<SharedCredentialsProvider>) -> Self {
        Self {
            provider,
            cached: Mutex::new(None),
        }
    }

    async fn resolve(&self) -> Result<BedrockCredentials> {
        let mut guard = self.cached.lock().await;
        if let Some(entry) = guard.as_ref() {
            if !is_expiring_soon(entry.expiry, SystemTime::now()) {
                return Ok(entry.creds.clone());
            }
        }
        let provider = self.provider.get_or_try_init(load_default_chain).await?;
        let aws_creds = provider
            .provide_credentials()
            .await
            .map_err(|e| eyre!("AWS credential chain failed: {e}"))?;
        let creds = BedrockCredentials {
            access_key_id: aws_creds.access_key_id().to_string(),
            secret_access_key: aws_creds.secret_access_key().to_string(),
            session_token: aws_creds.session_token().map(str::to_string),
        };
        *guard = Some(CachedCreds {
            creds: creds.clone(),
            expiry: aws_creds.expiry(),
        });
        Ok(creds)
    }

    async fn shared_provider(&self) -> Result<SharedCredentialsProvider> {
        self.provider
            .get_or_try_init(load_default_chain)
            .await
            .cloned()
    }
}

fn is_expiring_soon(expiry: Option<SystemTime>, now: SystemTime) -> bool {
    expiry.is_some_and(|t| t <= now + REFRESH_WINDOW)
}

async fn load_default_chain() -> Result<SharedCredentialsProvider> {
    aws_config::defaults(BehaviorVersion::latest())
        .load()
        .await
        .credentials_provider()
        .ok_or_else(|| eyre!("AWS default credential chain produced no provider"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use aws_credential_types::provider::future::ProvideCredentials as ProvideCredsFut;
    use aws_credential_types::Credentials;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn empty_env(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn parse_credentials_two_parts() {
        let c = BedrockCredentials::resolve("AKID:SECRET", empty_env).unwrap();
        assert_eq!(c.access_key_id, "AKID");
        assert_eq!(c.secret_access_key, "SECRET");
        assert!(c.session_token.is_none());
    }

    #[test]
    fn parse_credentials_three_parts() {
        let c = BedrockCredentials::resolve("AKID:SECRET:TOKEN", empty_env).unwrap();
        assert_eq!(c.access_key_id, "AKID");
        assert_eq!(c.secret_access_key, "SECRET");
        assert_eq!(c.session_token.as_deref(), Some("TOKEN"));
    }

    #[test]
    fn parse_credentials_secret_with_slash_preserved() {
        let c = BedrockCredentials::resolve("AKID:wJal/rXUtnFEMI:tok", empty_env).unwrap();
        assert_eq!(c.secret_access_key, "wJal/rXUtnFEMI");
        assert_eq!(c.session_token.as_deref(), Some("tok"));
    }

    #[test]
    fn parse_credentials_bare_key_supplements_secret_from_env() {
        let env = |k: &str| match k {
            "AWS_SECRET_ACCESS_KEY" => Some("envSecret".to_string()),
            _ => None,
        };
        let c = BedrockCredentials::resolve("AKID_FROM_PACKED", env).unwrap();
        assert_eq!(c.access_key_id, "AKID_FROM_PACKED");
        assert_eq!(c.secret_access_key, "envSecret");
        assert!(c.session_token.is_none());
    }

    #[test]
    fn parse_credentials_chain_sentinel_uses_env() {
        let env = |k: &str| match k {
            "AWS_ACCESS_KEY_ID" => Some("envKey".to_string()),
            "AWS_SECRET_ACCESS_KEY" => Some("envSecret".to_string()),
            "AWS_SESSION_TOKEN" => Some("envToken".to_string()),
            _ => None,
        };
        let c = BedrockCredentials::resolve(CHAIN_SENTINEL, env).unwrap();
        assert_eq!(c.access_key_id, "envKey");
        assert_eq!(c.secret_access_key, "envSecret");
        assert_eq!(c.session_token.as_deref(), Some("envToken"));
    }

    #[test]
    fn parse_credentials_missing_secret_errors() {
        assert!(BedrockCredentials::resolve("AKIDONLY", empty_env).is_err());
    }

    #[test]
    fn parse_credentials_no_creds_errors() {
        assert!(BedrockCredentials::resolve("", empty_env).is_err());
    }

    #[test]
    fn from_packed_complete_returns_static() {
        let resolver = CredentialResolver::from_packed_with_env("AKID:SECRET:TOKEN", empty_env);
        match resolver {
            CredentialResolver::Static(c) => {
                assert_eq!(c.access_key_id, "AKID");
                assert_eq!(c.secret_access_key, "SECRET");
                assert_eq!(c.session_token.as_deref(), Some("TOKEN"));
            }
            CredentialResolver::Chain(_) => panic!("expected Static variant"),
        }
    }

    #[test]
    fn from_packed_chain_sentinel_with_empty_env_returns_chain() {
        let resolver = CredentialResolver::from_packed_with_env(CHAIN_SENTINEL, empty_env);
        assert!(matches!(resolver, CredentialResolver::Chain(_)));
    }

    #[test]
    fn from_packed_incomplete_with_empty_env_returns_chain() {
        let resolver = CredentialResolver::from_packed_with_env("", empty_env);
        assert!(matches!(resolver, CredentialResolver::Chain(_)));
    }

    #[test]
    fn is_expiring_soon_treats_missing_expiry_as_permanent() {
        assert!(!is_expiring_soon(None, SystemTime::UNIX_EPOCH));
    }

    #[test]
    fn is_expiring_soon_fires_inside_window() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let edge = now + Duration::from_secs(30);
        let safe = now + Duration::from_secs(120);
        assert!(is_expiring_soon(Some(edge), now));
        assert!(!is_expiring_soon(Some(safe), now));
    }

    #[tokio::test]
    async fn static_resolver_round_trip() {
        let resolver = CredentialResolver::Static(BedrockCredentials {
            access_key_id: "AKID".to_string(),
            secret_access_key: "SECRET".to_string(),
            session_token: Some("TOKEN".to_string()),
        });
        let c = resolver.resolve().await.unwrap();
        assert_eq!(c.access_key_id, "AKID");
        assert_eq!(c.secret_access_key, "SECRET");
        assert_eq!(c.session_token.as_deref(), Some("TOKEN"));
    }

    #[tokio::test]
    async fn chain_resolver_round_trips_from_provider() {
        let creds = Credentials::new("CHAIN_AKID", "CHAIN_SECRET", None, None, "test");
        let resolver = CredentialResolver::from_provider(SharedCredentialsProvider::new(creds));
        let resolved = resolver.resolve().await.unwrap();
        assert_eq!(resolved.access_key_id, "CHAIN_AKID");
        assert_eq!(resolved.secret_access_key, "CHAIN_SECRET");
        assert!(resolved.session_token.is_none());
    }

    #[tokio::test]
    async fn chain_resolver_caches_non_expiring_credentials() {
        #[derive(Debug)]
        struct CountingProvider(Arc<AtomicUsize>);
        impl ProvideCredentials for CountingProvider {
            fn provide_credentials<'a>(&'a self) -> ProvideCredsFut<'a>
            where
                Self: 'a,
            {
                self.0.fetch_add(1, Ordering::SeqCst);
                ProvideCredsFut::ready(Ok(Credentials::new(
                    "AKID", "SECRET", None, None, "counting",
                )))
            }
        }
        let calls = Arc::new(AtomicUsize::new(0));
        let resolver = CredentialResolver::from_provider(SharedCredentialsProvider::new(
            CountingProvider(calls.clone()),
        ));
        for _ in 0..5 {
            resolver.resolve().await.unwrap();
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn chain_resolver_refreshes_expired_credentials() {
        #[derive(Debug)]
        struct RotatingProvider {
            calls: Arc<AtomicUsize>,
        }
        impl ProvideCredentials for RotatingProvider {
            fn provide_credentials<'a>(&'a self) -> ProvideCredsFut<'a>
            where
                Self: 'a,
            {
                let n = self.calls.fetch_add(1, Ordering::SeqCst);
                let expiry = if n == 0 {
                    SystemTime::now() - Duration::from_secs(1)
                } else {
                    SystemTime::now() + Duration::from_secs(3600)
                };
                ProvideCredsFut::ready(Ok(Credentials::new(
                    format!("AKID_{n}"),
                    "SECRET",
                    None,
                    Some(expiry),
                    "rotating",
                )))
            }
        }
        let calls = Arc::new(AtomicUsize::new(0));
        let resolver =
            CredentialResolver::from_provider(SharedCredentialsProvider::new(RotatingProvider {
                calls: calls.clone(),
            }));
        let first = resolver.resolve().await.unwrap();
        let second = resolver.resolve().await.unwrap();
        assert_eq!(first.access_key_id, "AKID_0");
        assert_eq!(second.access_key_id, "AKID_1");
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn chain_resolver_propagates_provider_error() {
        #[derive(Debug)]
        struct FailingProvider;
        impl ProvideCredentials for FailingProvider {
            fn provide_credentials<'a>(&'a self) -> ProvideCredsFut<'a>
            where
                Self: 'a,
            {
                ProvideCredsFut::ready(Err(
                    aws_credential_types::provider::error::CredentialsError::not_loaded(
                        "no creds available",
                    ),
                ))
            }
        }
        let resolver =
            CredentialResolver::from_provider(SharedCredentialsProvider::new(FailingProvider));
        let err = resolver.resolve().await.unwrap_err().to_string();
        assert!(
            err.contains("AWS credential chain failed"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    #[ignore = "requires real AWS credentials in the ambient environment"]
    async fn chain_resolves_from_default_environment() {
        let resolver = CredentialResolver::from_packed("none");
        let _ = resolver.resolve().await.expect("default chain failed");
    }
}
