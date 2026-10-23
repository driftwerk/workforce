use deadpool_postgres::{
    ClientWrapper, Hook, HookError, Manager, ManagerConfig, Pool, RecyclingMethod,
};
use tokio_postgres::NoTls;

use super::{PostgresConfig, PostgresError, SslMode};

/// rustls verifier that accepts any server certificate. Used for
/// `sslmode = "require"`: encrypt the connection (defeating passive sniffing)
/// without verifying the peer's certificate chain — appropriate for an in-VPC
/// link to an endpoint presenting a private/AWS-CA cert. `verify-full` (cert
/// verification against a bundled CA) can be added later as a stronger mode.
#[derive(Debug)]
struct NoCertVerify;

impl rustls::client::danger::ServerCertVerifier for NoCertVerify {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        use rustls::SignatureScheme::*;
        vec![
            RSA_PKCS1_SHA256,
            RSA_PKCS1_SHA384,
            RSA_PKCS1_SHA512,
            ECDSA_NISTP256_SHA256,
            ECDSA_NISTP384_SHA384,
            ECDSA_NISTP521_SHA512,
            RSA_PSS_SHA256,
            RSA_PSS_SHA384,
            RSA_PSS_SHA512,
            ED25519,
        ]
    }
}

/// Build the rustls-based TLS connector for `sslmode = "require"`.
/// Pins the ring crypto provider explicitly so it works regardless of which
/// providers other crates enable (avoids the process-default ambiguity panic).
fn require_tls() -> tokio_postgres_rustls::MakeRustlsConnect {
    let provider = std::sync::Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("rustls supports the default protocol versions")
        .dangerous()
        .with_custom_certificate_verifier(std::sync::Arc::new(NoCertVerify))
        .with_no_client_auth();
    tokio_postgres_rustls::MakeRustlsConnect::new(config)
}

/// Shared, cloneable Postgres connection pool.
///
/// Internally a `deadpool_postgres::Pool`, which is itself cheap to
/// clone (it holds the inner state behind an `Arc`). Construct one at
/// startup via [`PostgresPool::from_config`] and clone it into each
/// store that needs it.
#[derive(Clone)]
pub struct PostgresPool {
    inner: Pool,
}

impl PostgresPool {
    /// Build a pool from the configured connection settings. Errors if
    /// the config can't be turned into a valid `tokio_postgres::Config`
    /// or if the pool builder rejects it.
    pub fn from_config(config: &PostgresConfig) -> Result<Self, PostgresError> {
        Self::from_config_with_session_init(config, &[])
    }

    /// Like [`from_config`] but installs a `post_create` hook that
    /// runs the supplied SQL statements on every freshly-created
    /// connection. Used by individual stores to pin `search_path` to
    /// their schema (e.g. `SET search_path TO telemetry, public`),
    /// guaranteeing every checked-out connection sees its tables
    /// regardless of which order pool reuse happens in. Statements
    /// run via [`tokio_postgres::Client::simple_query`], so they may
    /// not contain placeholders.
    pub fn from_config_with_session_init(
        config: &PostgresConfig,
        session_init: &[&str],
    ) -> Result<Self, PostgresError> {
        let pg_config = config.to_tokio_postgres_config()?;
        let mgr_config = ManagerConfig {
            recycling_method: RecyclingMethod::Fast,
        };
        // `deadpool_postgres::Manager` is type-erased over the TLS connector,
        // so both arms yield the same `Manager` type.
        let manager = match config.sslmode {
            SslMode::Disable => Manager::from_config(pg_config, NoTls, mgr_config),
            SslMode::Require => Manager::from_config(pg_config, require_tls(), mgr_config),
        };
        let mut builder = Pool::builder(manager).max_size(config.pool_size);
        if !session_init.is_empty() {
            // Concatenate all init statements into a single
            // `simple_query` call so the hook only does one round-trip
            // per new connection. `simple_query` accepts multiple `;`
            // separated statements.
            let combined = session_init.join("; ");
            builder = builder.post_create(Hook::async_fn(move |client: &mut ClientWrapper, _| {
                let combined = combined.clone();
                Box::pin(async move {
                    client
                        .simple_query(&combined)
                        .await
                        .map_err(|e| HookError::message(format!("session init failed: {e}")))?;
                    Ok(())
                })
            }));
        }
        let inner = builder
            .build()
            .map_err(|e| PostgresError::Pool(e.to_string()))?;
        Ok(Self { inner })
    }

    /// Borrow a connection from the pool.
    pub async fn get(&self) -> Result<deadpool_postgres::Object, PostgresError> {
        Ok(self.inner.get().await?)
    }

    /// Quick liveness check (`SELECT 1`). Used at hub startup to fail
    /// fast on a misconfigured connection rather than discovering it on
    /// the first store query.
    pub async fn ping(&self) -> Result<(), PostgresError> {
        let conn = self.get().await?;
        conn.simple_query("SELECT 1").await?;
        Ok(())
    }
}

impl std::fmt::Debug for PostgresPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PostgresPool").finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tls_tests {
    use super::*;

    /// Connect over TLS to an SSL-enabled Postgres in `require` mode. Skips
    /// unless `WORKFORCE_TLS_TEST_URL` (a `postgres://…` to a TLS server) is set.
    #[tokio::test]
    async fn require_mode_pings_over_tls() {
        let Ok(url) = std::env::var("WORKFORCE_TLS_TEST_URL") else {
            eprintln!("WORKFORCE_TLS_TEST_URL unset — skipping");
            return;
        };
        let cfg = PostgresConfig {
            enabled: true,
            url: Some(url),
            sslmode: SslMode::Require,
            ..Default::default()
        };
        let pool = PostgresPool::from_config(&cfg).expect("build tls pool");
        pool.ping().await.expect("ping over TLS");
    }
}
