use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use deadpool_postgres::{
    ClientWrapper, Hook, HookError, Manager, ManagerConfig, Pool, RecyclingMethod, WeakPool,
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
        register(schema_of(session_init), &inner);
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(close_idle(inner.weak(), IDLE_SWEEP, IDLE_TIMEOUT));
        }
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

/// How long a connection may sit unused before its pool closes it. A pool
/// opens what its busiest moment asks for; without this it kept every one of
/// them for the life of the process, so each of a hub's pools held its
/// high-water mark and between them they filled the server.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(300);

/// How often each pool looks for connections unused for [`IDLE_TIMEOUT`].
const IDLE_SWEEP: Duration = Duration::from_secs(60);

/// Closes `pool`'s connections that have been unused for `idle`, every
/// `every`, until the pool is dropped.
async fn close_idle(pool: WeakPool, every: Duration, idle: Duration) {
    loop {
        tokio::time::sleep(every).await;
        let Some(pool) = pool.upgrade() else {
            return;
        };
        pool.retain(|_, metrics| metrics.last_used() < idle);
    }
}

/// Every pool the process has built and not yet dropped, by the schema its
/// connections are pinned to.
static POOLS: LazyLock<Mutex<Vec<(String, WeakPool)>>> = LazyLock::new(Mutex::default);

fn register(schema: String, pool: &Pool) {
    let mut pools = POOLS.lock().unwrap_or_else(|e| e.into_inner());
    pools.retain(|(_, pool)| pool.upgrade().is_some());
    pools.push((schema, pool.weak()));
}

/// The schema a pool's `SET search_path TO …` pins, else `public`.
fn schema_of(session_init: &[&str]) -> String {
    session_init
        .iter()
        .find_map(|statement| statement.trim().strip_prefix("SET search_path TO "))
        .and_then(|path| path.split(',').next())
        .map(|schema| schema.trim().to_string())
        .filter(|schema| !schema.is_empty())
        .unwrap_or_else(|| "public".to_string())
}

/// The pools of one schema: how many there are, the most connections they
/// may open between them, and how many they hold now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolShare {
    pub schema: String,
    pub pools: usize,
    pub max: usize,
    pub open: usize,
}

/// The process's pools against the server's `max_connections`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolBudget {
    /// By schema, the largest share first.
    pub shares: Vec<PoolShare>,
    /// `max_connections`, less the slots kept for superusers.
    pub allowed: usize,
}

impl PoolBudget {
    /// The live pools' shares, against `allowed` connections.
    pub fn now(allowed: usize) -> Self {
        let pools = POOLS.lock().unwrap_or_else(|e| e.into_inner());
        let mut shares: Vec<PoolShare> = Vec::new();
        for (schema, pool) in pools.iter() {
            let Some(pool) = pool.upgrade() else {
                continue;
            };
            let status = pool.status();
            match shares.iter_mut().find(|share| share.schema == *schema) {
                Some(share) => {
                    share.pools += 1;
                    share.max += status.max_size;
                    share.open += status.size;
                }
                None => shares.push(PoolShare {
                    schema: schema.clone(),
                    pools: 1,
                    max: status.max_size,
                    open: status.size,
                }),
            }
        }
        shares.sort_by(|a, b| b.max.cmp(&a.max).then_with(|| a.schema.cmp(&b.schema)));
        Self { shares, allowed }
    }

    /// The most connections the pools may open between them.
    pub fn max(&self) -> usize {
        self.shares.iter().map(|share| share.max).sum()
    }

    /// The connections the pools hold now.
    pub fn open(&self) -> usize {
        self.shares.iter().map(|share| share.open).sum()
    }

    /// Whether the pools may open more connections than the server allows.
    pub fn exceeds(&self) -> bool {
        self.max() > self.allowed
    }

    /// The shares as `schema=max`, and `/pools` when a schema has more than
    /// one pool, for the log.
    pub fn by_schema(&self) -> String {
        self.shares
            .iter()
            .map(|share| match share.pools {
                1 => format!("{}={}", share.schema, share.max),
                n => format!("{}={}/{n}", share.schema, share.max),
            })
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// Says how many connections the process's pools may open between them
/// against the `max_connections` of the server they share, and warns when
/// that is more than it allows: one pool's busy moment can then be refused
/// for another's idle connections. Asks the server through any live pool;
/// `None` when there is none or it can't be asked (said in the log).
pub async fn check_budget() -> Option<PoolBudget> {
    let pool = {
        let pools = POOLS.lock().unwrap_or_else(|e| e.into_inner());
        pools.iter().find_map(|(_, pool)| pool.upgrade())?
    };
    // Bounded: on a server already full, the pool would wait for ever and
    // the hub would never start listening.
    let asked = tokio::time::timeout(BUDGET_WAIT, allowed_connections(&pool)).await;
    let allowed = match asked {
        Ok(Ok(allowed)) => allowed,
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "the Postgres connection budget could not be checked");
            return None;
        }
        Err(_) => {
            tracing::warn!(
                waited = ?BUDGET_WAIT,
                "the Postgres connection budget could not be checked: no connection was free"
            );
            return None;
        }
    };
    let budget = PoolBudget::now(allowed);
    if budget.exceeds() {
        tracing::warn!(
            pools_max = budget.max(),
            open = budget.open(),
            allowed = budget.allowed,
            by_schema = %budget.by_schema(),
            "the hub's Postgres pools may open more connections than the server allows; raise max_connections or lower [postgres] pool_size"
        );
    } else {
        tracing::info!(
            pools_max = budget.max(),
            open = budget.open(),
            allowed = budget.allowed,
            by_schema = %budget.by_schema(),
            "the hub's Postgres pools fit the server's connections"
        );
    }
    Some(budget)
}

/// How long [`check_budget`] waits to ask the server.
const BUDGET_WAIT: Duration = Duration::from_secs(5);

/// `max_connections` less `superuser_reserved_connections`.
async fn allowed_connections(pool: &Pool) -> Result<usize, PostgresError> {
    let conn = pool.get().await?;
    let mut settings = [0usize; 2];
    for (setting, name) in settings
        .iter_mut()
        .zip(["max_connections", "superuser_reserved_connections"])
    {
        let row = conn.query_one(&format!("SHOW {name}"), &[]).await?;
        *setting = row
            .get::<_, String>(0)
            .parse()
            .map_err(|e| PostgresError::Query(format!("{name}: {e}")))?;
    }
    Ok(settings[0].saturating_sub(settings[1]))
}

impl std::fmt::Debug for PostgresPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PostgresPool").finish_non_exhaustive()
    }
}

#[cfg(test)]
mod budget_tests {
    use super::*;

    fn share(schema: &str, pools: usize, max: usize) -> PoolShare {
        PoolShare {
            schema: schema.into(),
            pools,
            max,
            open: 0,
        }
    }

    #[test]
    fn a_pool_is_named_by_the_schema_its_search_path_pins() {
        assert_eq!(schema_of(&["SET search_path TO floor, public"]), "floor");
        assert_eq!(
            schema_of(&["SET statement_timeout = 5", "SET search_path TO artifacts"]),
            "artifacts"
        );
        assert_eq!(schema_of(&[]), "public");
    }

    /// The pools exceed the budget only when, between them, they may open
    /// more than the server allows.
    #[test]
    fn the_budget_is_exceeded_by_the_sum_of_the_pools_maxima() {
        let budget = PoolBudget {
            shares: vec![share("sessions", 1, 16), share("telemetry", 3, 21)],
            allowed: 37,
        };
        assert_eq!(budget.max(), 37);
        assert!(!budget.exceeds());
        assert_eq!(budget.by_schema(), "sessions=16 telemetry=21/3");
        let over = PoolBudget {
            allowed: 36,
            ..budget
        };
        assert!(over.exceeds());
    }
}

#[cfg(all(test, feature = "test-support"))]
mod budget_pg_tests {
    use super::*;
    use crate::test_support::with_test_pool;
    use crate::PostgresConfig;

    /// A pool counts toward the budget while it lives, under its schema,
    /// and the server is asked what it allows.
    #[tokio::test]
    async fn a_live_pool_is_in_the_budget_and_a_dropped_one_is_not() {
        let config = std::sync::Arc::new(Mutex::new(None::<PostgresConfig>));
        let kept = std::sync::Arc::clone(&config);
        let build = move |cfg: &PostgresConfig| {
            *kept.lock().unwrap() = Some(cfg.clone());
            PostgresPool::from_config(cfg)
        };
        with_test_pool(build, |_pool| async move {
            let cfg = config.lock().unwrap().clone().unwrap();
            let probe = PostgresPool::from_config_with_session_init(
                &PostgresConfig {
                    pool_size: 3,
                    ..cfg
                },
                &["SET search_path TO budget_probe, public"],
            )
            .unwrap();
            let counted = |budget: &PoolBudget| {
                budget
                    .shares
                    .iter()
                    .find(|share| share.schema == "budget_probe")
                    .map_or(0, |share| share.max)
            };
            let budget = check_budget().await.expect("the server is asked");
            assert!(budget.allowed > 0);
            assert!(counted(&budget) >= 3);
            drop(probe);
            let after = PoolBudget::now(budget.allowed);
            assert_eq!(counted(&after) + 3, counted(&budget));
        })
        .await;
    }

    /// Connections unused for longer than the idle limit are closed; the
    /// pool opens new ones when next asked, and the sweep ends once the pool
    /// is dropped.
    #[tokio::test]
    async fn idle_connections_are_closed() {
        let config = std::sync::Arc::new(Mutex::new(None::<PostgresConfig>));
        let kept = std::sync::Arc::clone(&config);
        let build = move |cfg: &PostgresConfig| {
            *kept.lock().unwrap() = Some(cfg.clone());
            PostgresPool::from_config(cfg)
        };
        with_test_pool(build, |_pool| async move {
            let cfg = config.lock().unwrap().clone().unwrap();
            let pool = PostgresPool::from_config(&cfg).unwrap();
            let held = (pool.get().await.unwrap(), pool.get().await.unwrap());
            drop(held);
            assert_eq!(pool.inner.status().size, 2);
            let closing = tokio::spawn(close_idle(
                pool.inner.weak(),
                Duration::from_millis(20),
                Duration::from_millis(50),
            ));
            tokio::time::sleep(Duration::from_millis(300)).await;
            assert_eq!(pool.inner.status().size, 0);
            pool.ping().await.unwrap();
            drop(pool);
            tokio::time::timeout(Duration::from_secs(5), closing)
                .await
                .expect("the sweep ends once its pool is dropped")
                .unwrap();
        })
        .await;
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
