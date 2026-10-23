//! Postgres-family backend (Postgres, Redshift, Cloud SQL-Postgres, AlloyDB).
//!
//! Reads stream through a server-side cursor in the **text** query protocol, so
//! every value arrives as a string and decodes uniformly via
//! [`text_to_json`] — no per-type binary decoding, and the simple/text path is
//! the Redshift-friendly mode. Each read runs inside a `READ ONLY` transaction
//! with `statement_timeout` set, the defense-in-depth layer beneath the
//! least-privilege database role. Connect, work, and cleanup are each bounded so
//! a dead or slow host can never hang the tool.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use deadpool_postgres::{Manager, ManagerConfig, Object, Pool, RecyclingMethod};
use eyre::{eyre, Result};
use tokio_postgres::config::SslMode;
use tokio_postgres::{NoTls, SimpleQueryMessage};

use super::{text_to_json, Backend, Engine, ExecOutcome, Flow, QueryResult, RowSink};

const POOL_SIZE: usize = 4;
const CURSOR: &str = "wf_db_cursor";
const FETCH_BATCH: usize = 2_000;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(5);
const PG_SYSTEM_SCHEMAS: &[&str] = &["pg_catalog", "information_schema", "pg_toast"];

/// rustls verifier that accepts any server certificate — encrypt without
/// chain verification for endpoints presenting a private/Cloud-CA cert. Used
/// only when an instance is left at the default `tls_insecure` posture.
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
    ) -> std::result::Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
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

fn tls_connector(verify: bool) -> tokio_postgres_rustls::MakeRustlsConnect {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("rustls supports the default protocol versions");
    let config = if verify {
        let mut roots = rustls::RootCertStore::empty();
        if let Ok(certs) = rustls_native_certs::load_native_certs() {
            for cert in certs {
                let _ = roots.add(cert);
            }
        }
        builder.with_root_certificates(roots).with_no_client_auth()
    } else {
        builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(NoCertVerify))
            .with_no_client_auth()
    };
    tokio_postgres_rustls::MakeRustlsConnect::new(config)
}

pub struct PgBackend {
    pool: Pool,
    engine: Engine,
    allow_writes: bool,
}

impl PgBackend {
    /// Build a lazily-connecting pool from a DSN. No connection is opened until
    /// the first query.
    pub fn new(dsn: &str, engine: Engine, allow_writes: bool, tls_insecure: bool) -> Result<Self> {
        let pg_config: tokio_postgres::Config = dsn
            .parse()
            .map_err(|e| eyre!("invalid Postgres connection string: {e}"))?;
        let mgr_config = ManagerConfig {
            recycling_method: RecyclingMethod::Fast,
        };
        let manager = match pg_config.get_ssl_mode() {
            SslMode::Disable => Manager::from_config(pg_config, NoTls, mgr_config),
            _ => Manager::from_config(pg_config, tls_connector(!tls_insecure), mgr_config),
        };
        let pool = Pool::builder(manager)
            .max_size(POOL_SIZE)
            .build()
            .map_err(|e| eyre!("failed to build Postgres pool: {e}"))?;
        Ok(Self {
            pool,
            engine,
            allow_writes,
        })
    }
}

fn timeout_stmt(timeout: Duration) -> String {
    format!("SET LOCAL statement_timeout = {}", timeout.as_millis())
}

/// Acquire a connection with a bounded wait so a dead host can't hang the tool.
async fn connect(pool: &Pool) -> Result<Object> {
    match tokio::time::timeout(CONNECT_TIMEOUT, pool.get()).await {
        Ok(Ok(client)) => Ok(client),
        Ok(Err(e)) => Err(eyre!("failed to acquire a database connection: {e}")),
        Err(_) => Err(eyre!("timed out connecting to the database")),
    }
}

/// Run end-of-transaction statements with their own bound — if the connection
/// is wedged, cleanup can't hang either (the connection is then dropped).
async fn finalize(client: &tokio_postgres::Client, statements: &[&str]) {
    let _ = tokio::time::timeout(CLEANUP_TIMEOUT, async {
        for stmt in statements {
            let _ = client.batch_execute(stmt).await;
        }
    })
    .await;
}

#[async_trait]
impl Backend for PgBackend {
    fn engine(&self) -> Engine {
        self.engine
    }

    fn allow_writes(&self) -> bool {
        self.allow_writes
    }

    async fn ping(&self) -> Result<()> {
        let client = connect(&self.pool).await?;
        match tokio::time::timeout(CONNECT_TIMEOUT, client.simple_query("SELECT 1")).await {
            Ok(r) => r.map(|_| ()).map_err(eyre::Report::from),
            Err(_) => Err(eyre!("database ping timed out")),
        }
    }

    async fn export(
        &self,
        sql: &str,
        timeout: Duration,
        sink: &mut (dyn RowSink + Send),
    ) -> Result<usize> {
        let client = connect(&self.pool).await?;
        let drive = async {
            client.batch_execute("BEGIN READ ONLY").await?;
            client.batch_execute(&timeout_stmt(timeout)).await?;
            client
                .batch_execute(&format!("DECLARE {CURSOR} NO SCROLL CURSOR FOR {sql}"))
                .await?;
            let mut total = 0usize;
            let mut sent_columns = false;
            loop {
                let msgs = client
                    .simple_query(&format!("FETCH FORWARD {FETCH_BATCH} FROM {CURSOR}"))
                    .await?;
                let mut fetched = 0usize;
                let mut stop = false;
                for msg in msgs {
                    let SimpleQueryMessage::Row(row) = msg else {
                        continue;
                    };
                    if !sent_columns {
                        let names: Vec<String> =
                            row.columns().iter().map(|c| c.name().to_string()).collect();
                        sink.columns(&names)?;
                        sent_columns = true;
                    }
                    fetched += 1;
                    total += 1;
                    let cells = (0..row.len()).map(|i| text_to_json(row.get(i))).collect();
                    if sink.push(cells)? == Flow::Stop {
                        stop = true;
                        break;
                    }
                }
                if fetched < FETCH_BATCH || stop {
                    break;
                }
            }
            Ok::<usize, eyre::Report>(total)
        };
        let outcome = tokio::time::timeout(timeout + CLEANUP_TIMEOUT, drive).await;
        finalize(&client, &[&format!("CLOSE {CURSOR}"), "COMMIT"]).await;
        match outcome {
            Ok(result) => result,
            Err(_) => Err(eyre!(
                "query exceeded the {}ms statement timeout",
                timeout.as_millis()
            )),
        }
    }

    async fn execute(&self, sql: &str, timeout: Duration) -> Result<ExecOutcome> {
        let client = connect(&self.pool).await?;
        let drive = async {
            client.batch_execute("BEGIN").await?;
            client.batch_execute(&timeout_stmt(timeout)).await?;
            let msgs = client.simple_query(sql).await?;
            let mut outcome = ExecOutcome::default();
            for msg in msgs {
                if let SimpleQueryMessage::CommandComplete(n) = msg {
                    outcome.rows_affected += n;
                    outcome.statements += 1;
                }
            }
            Ok::<ExecOutcome, eyre::Report>(outcome)
        };
        let result = tokio::time::timeout(timeout + CLEANUP_TIMEOUT, drive).await;
        match result {
            Ok(Ok(outcome)) => {
                finalize(&client, &["COMMIT"]).await;
                Ok(outcome)
            }
            Ok(Err(e)) => {
                finalize(&client, &["ROLLBACK"]).await;
                Err(e)
            }
            Err(_) => {
                finalize(&client, &["ROLLBACK"]).await;
                Err(eyre!(
                    "statement exceeded the {}ms timeout and was rolled back",
                    timeout.as_millis()
                ))
            }
        }
    }

    fn system_schemas(&self) -> &'static [&'static str] {
        PG_SYSTEM_SCHEMAS
    }

    /// `EXPLAIN` cannot back a `DECLARE CURSOR`, so run it buffered inside a
    /// read-only transaction; plan output is small.
    async fn explain(&self, sql: &str, timeout: Duration) -> Result<QueryResult> {
        let client = connect(&self.pool).await?;
        let drive = async {
            client.batch_execute("BEGIN READ ONLY").await?;
            client.batch_execute(&timeout_stmt(timeout)).await?;
            let msgs = client.simple_query(&format!("EXPLAIN {sql}")).await?;
            let mut result = QueryResult {
                columns: vec!["QUERY PLAN".to_string()],
                ..QueryResult::default()
            };
            for msg in msgs {
                if let SimpleQueryMessage::Row(row) = msg {
                    result.rows.push(vec![text_to_json(row.get(0))]);
                }
            }
            Ok::<QueryResult, eyre::Report>(result)
        };
        let outcome = tokio::time::timeout(timeout + CLEANUP_TIMEOUT, drive).await;
        finalize(&client, &["COMMIT"]).await;
        outcome.unwrap_or_else(|_| Err(eyre!("EXPLAIN exceeded the statement timeout")))
    }
}
