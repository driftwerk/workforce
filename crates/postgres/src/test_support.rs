//! Shared parity-test harness for every workforce store crate.
//!
//! Each store crate (`workforce-common`, `workforce-identity`,
//! `workforce-pairing`, `workforce-codebase-knowledge`) used to carry
//! its own copy of `SharedPostgres` + `parse_admin_url` +
//! `shared_postgres()` + `with_embedded_pg*()` — about 700 lines of
//! identical code across seven test files. This module is the single
//! place all of them now go through.
//!
//! ## Behaviour
//!
//! On the first call to [`shared_postgres`] in a test binary:
//!
//! - If `WORKFORCE_TEST_POSTGRES_URL` is set, parse it as the admin
//!   URL and use the server it points at. Nothing is spawned, nothing
//!   is stopped on `Drop`.
//! - Otherwise, spawn `postgresql_embedded` with the bundled binary
//!   and use that. The instance is stopped when the test binary exits:
//!   libtest leaves through `process::exit`, which never drops statics
//!   but does run `atexit` handlers, so the stop is registered there.
//! - If `WORKFORCE_SKIP_POSTGRES_TESTS` is set on the way in,
//!   [`with_test_pool`] returns immediately without touching anything
//!   (used by CI to skip the whole Postgres test surface).
//!
//! ## Usage
//!
//! Each store crate provides its own thin wrapper that knows how to
//! build a pool with `search_path` pinned to its schema:
//!
//! ```ignore
//! workforce_postgres::test_support::with_test_pool(
//!     PostgresIdentityStore::build_pool,
//!     |pool| async move {
//!         let store = PostgresIdentityStore::with_pool(pool).await?;
//!         // ... assertions
//!     },
//! ).await;
//! ```

use std::process::{Command, Stdio};
use std::sync::Arc;

use postgresql_embedded::{PostgreSQL, Settings};
use tokio::sync::{Mutex, OnceCell};
use uuid::Uuid;

use crate::{PostgresConfig, PostgresError, PostgresPool};

/// Either an externally-provided Postgres server (via
/// `WORKFORCE_TEST_POSTGRES_URL`) or a locally-spawned embedded
/// instance. There's exactly one per test binary, behind a `OnceCell`.
#[allow(clippy::large_enum_variant)]
pub enum SharedPostgres {
    External {
        host: String,
        port: u16,
        username: String,
        password: String,
    },
    Embedded {
        postgresql: PostgreSQL,
        host: String,
        port: u16,
        username: String,
        password: String,
    },
}

impl SharedPostgres {
    pub fn host(&self) -> &str {
        match self {
            Self::External { host, .. } | Self::Embedded { host, .. } => host,
        }
    }

    pub fn port(&self) -> u16 {
        match self {
            Self::External { port, .. } | Self::Embedded { port, .. } => *port,
        }
    }

    pub fn username(&self) -> &str {
        match self {
            Self::External { username, .. } | Self::Embedded { username, .. } => username,
        }
    }

    pub fn password(&self) -> &str {
        match self {
            Self::External { password, .. } | Self::Embedded { password, .. } => password,
        }
    }
}

static SHARED_PG: OnceCell<Arc<Mutex<SharedPostgres>>> = OnceCell::const_new();

/// How long `initdb` and `pg_ctl` may take. The crate's default of five
/// seconds is less than `initdb` needs on a machine busy with other test
/// suites, and a run cut off there fails every test waiting on the server.
const EMBEDDED_COMMAND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// `pg_ctl stop` for the embedded server, runnable without a runtime.
fn pg_ctl_stop(settings: &Settings) -> Command {
    let mut cmd = Command::new(settings.binary_dir().join("pg_ctl"));
    cmd.args(["stop", "-m", "fast", "-w", "-D"])
        .arg(&settings.data_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    cmd
}

/// Stop the embedded server and, for a temporary data directory, remove it.
fn stop_embedded(settings: &Settings) {
    match pg_ctl_stop(settings).status() {
        Ok(status) if status.success() => {
            if settings.temporary {
                let _ = std::fs::remove_dir_all(&settings.data_dir);
            }
        }
        Ok(status) => eprintln!("Failed to stop embedded postgres: pg_ctl exited with {status}"),
        Err(e) => eprintln!("Failed to stop embedded postgres: {e}"),
    }
}

/// `atexit` hook: the test binary is leaving, take the postmaster with it.
/// Only ever registered after the embedded instance exists, so `get()` is
/// `Some` and nothing awaits the lock any more.
extern "C" fn stop_embedded_at_exit() {
    let Some(shared) = SHARED_PG.get() else {
        return;
    };
    let Ok(guard) = shared.try_lock() else {
        return;
    };
    if let SharedPostgres::Embedded { postgresql, .. } = &*guard {
        stop_embedded(postgresql.settings());
    }
}

/// Parse a `postgres://user:pass@host:port/db` admin URL into its
/// components. The trailing path is ignored — tests always use the
/// `postgres` admin database for `CREATE DATABASE`/`DROP DATABASE`
/// against the per-test database.
fn parse_admin_url(url: &str) -> Option<(String, u16, String, String)> {
    let url = url.strip_prefix("postgres://")?;
    let (creds, rest) = url.split_once('@')?;
    let (user, pass) = creds.split_once(':').unwrap_or((creds, ""));
    let host_port = rest.split_once('/').map(|(hp, _)| hp).unwrap_or(rest);
    let (host, port_str) = host_port.split_once(':').unwrap_or((host_port, "5432"));
    let port = port_str.parse::<u16>().ok()?;
    Some((host.to_string(), port, user.to_string(), pass.to_string()))
}

/// Returns the shared `SharedPostgres` for this test binary, spawning
/// it on first call. Honours `WORKFORCE_TEST_POSTGRES_URL` if set.
async fn shared_postgres() -> Arc<Mutex<SharedPostgres>> {
    SHARED_PG
        .get_or_init(|| async {
            if let Ok(url) = std::env::var("WORKFORCE_TEST_POSTGRES_URL") {
                let (host, port, username, password) = parse_admin_url(&url).unwrap_or_else(|| {
                    panic!("WORKFORCE_TEST_POSTGRES_URL is set but couldn't be parsed: {url}")
                });
                return Arc::new(Mutex::new(SharedPostgres::External {
                    host,
                    port,
                    username,
                    password,
                }));
            }

            let settings = Settings {
                username: "postgres".to_string(),
                password: "postgres".to_string(),
                timeout: Some(EMBEDDED_COMMAND_TIMEOUT),
                ..Default::default()
            };
            let mut postgresql = PostgreSQL::new(settings);
            postgresql.setup().await.expect("setup embedded postgres");
            postgresql.start().await.expect("start embedded postgres");
            // SAFETY: `stop_embedded_at_exit` is a plain `extern "C" fn` that
            // touches only the static it is registered for.
            unsafe {
                libc::atexit(stop_embedded_at_exit);
            }

            let s = postgresql.settings();
            let host = s.host.clone();
            let port = s.port;
            let username = s.username.clone();
            let password = s.password.clone();

            // Best-effort perf tuning — disabling fsync etc. on a
            // throwaway test database is safe and noticeably faster.
            // Failures are non-fatal.
            let admin_url = format!("postgres://{username}:{password}@{host}:{port}/postgres");
            if let Ok((client, conn)) =
                tokio_postgres::connect(&admin_url, tokio_postgres::NoTls).await
            {
                let handle = tokio::spawn(async move {
                    let _ = conn.await;
                });
                for cmd in [
                    "ALTER SYSTEM SET fsync = off",
                    "ALTER SYSTEM SET synchronous_commit = off",
                    "ALTER SYSTEM SET full_page_writes = off",
                    "SELECT pg_reload_conf()",
                ] {
                    let _ = client.execute(cmd, &[]).await;
                }
                drop(client);
                handle.abort();
            }

            Arc::new(Mutex::new(SharedPostgres::Embedded {
                postgresql,
                host,
                port,
                username,
                password,
            }))
        })
        .await
        .clone()
}

/// Run a test against a freshly-created database inside the shared
/// Postgres source. The destination database is created via
/// `CREATE DATABASE test_db_<uuid>`, the caller-provided `build_pool`
/// closure builds a [`PostgresPool`] against it (typically by calling
/// `<Store>::build_pool` so `search_path` is pinned to the right
/// schema), the test runs, and then the database is dropped.
///
/// The test future is awaited inline (no `tokio::spawn`) so it
/// doesn't need to be `Send` — that's what lets the migration tests
/// pass `rusqlite::Connection` (which isn't `Sync`) through the body.
/// The database is dropped when the test ends, however it ends: one
/// that panics leaves none behind on a Postgres that outlives the test
/// run (`WORKFORCE_TEST_POSTGRES_URL`), where they would pile up.
///
/// When `WORKFORCE_SKIP_POSTGRES_TESTS` is set in the environment
/// (CI uses this), the function returns immediately without touching
/// anything.
pub async fn with_test_pool<B, F, Fut>(build_pool: B, test: F)
where
    B: FnOnce(&PostgresConfig) -> Result<PostgresPool, PostgresError>,
    F: FnOnce(PostgresPool) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    if std::env::var("WORKFORCE_SKIP_POSTGRES_TESTS").is_ok() {
        eprintln!("[postgres-test] WORKFORCE_SKIP_POSTGRES_TESTS set — skipping");
        return;
    }

    let shared = shared_postgres().await;
    let (host, port, username, password) = {
        let s = shared.lock().await;
        (
            s.host().to_string(),
            s.port(),
            s.username().to_string(),
            s.password().to_string(),
        )
    };

    let db_name = format!("test_db_{}", Uuid::new_v4().simple());
    let admin_url = format!("postgres://{username}:{password}@{host}:{port}/postgres");

    {
        let (client, conn) = tokio_postgres::connect(&admin_url, tokio_postgres::NoTls)
            .await
            .expect("connect admin");
        let handle = tokio::spawn(async move {
            let _ = conn.await;
        });
        client
            .execute(&format!("CREATE DATABASE {db_name}"), &[])
            .await
            .expect("create test db");
        drop(client);
        handle.abort();
    }

    let dropped_after = TestDb {
        admin_url: admin_url.clone(),
        name: db_name.clone(),
    };
    let test_url = format!("postgres://{username}:{password}@{host}:{port}/{db_name}");
    let pg_cfg = PostgresConfig {
        enabled: true,
        url: Some(test_url),
        pool_size: 4,
        ..Default::default()
    };
    let pool = build_pool(&pg_cfg).expect("build pool");

    test(pool.clone()).await;

    drop(pool);
    drop(dropped_after);
}

/// A test's database, dropped with this: also when the test panics, and
/// what it was awaited in is unwound.
struct TestDb {
    admin_url: String,
    name: String,
}

impl Drop for TestDb {
    fn drop(&mut self) {
        let (admin_url, name) = (self.admin_url.clone(), self.name.clone());
        // On a thread and a runtime of its own: this may be the unwinding
        // of a test's runtime, in which nothing more can be awaited.
        let dropping = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build();
            if let Ok(runtime) = runtime {
                runtime.block_on(teardown_test_db(&admin_url, &name));
            }
        });
        let _ = dropping.join();
    }
}

async fn teardown_test_db(admin_url: &str, db_name: &str) {
    let Ok((client, conn)) = tokio_postgres::connect(admin_url, tokio_postgres::NoTls).await else {
        return;
    };
    let handle = tokio::spawn(async move {
        let _ = conn.await;
    });
    let _ = client
        .execute(
            "SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
             WHERE datname = $1 AND pid <> pg_backend_pid()",
            &[&db_name],
        )
        .await;
    let _ = client
        .execute(&format!("DROP DATABASE IF EXISTS {db_name}"), &[])
        .await;
    drop(client);
    handle.abort();
}

/// `Utc::now()` truncated to microseconds — the precision Postgres
/// `timestamptz` actually stores. Test fixtures MUST use this (not
/// `Utc::now()`) for any timestamp that round-trips through Postgres and
/// is then compared for equality: on Linux the system clock has
/// nanosecond resolution, so a raw `Utc::now()` almost never survives
/// the round trip intact. macOS clocks are µs-aligned, which is why the
/// mismatch only shows up in CI.
pub fn now_micros() -> chrono::DateTime<chrono::Utc> {
    let now = chrono::Utc::now();
    let sub_us = chrono::Duration::nanoseconds(i64::from(now.timestamp_subsec_nanos() % 1000));
    now - sub_us
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_url_with_password() {
        let parsed = parse_admin_url("postgres://alice:secret@db.example.com:6543/postgres");
        assert_eq!(
            parsed,
            Some((
                "db.example.com".to_string(),
                6543,
                "alice".to_string(),
                "secret".to_string()
            ))
        );
    }

    #[test]
    fn parses_url_with_default_port() {
        let parsed = parse_admin_url("postgres://alice:secret@db.example.com/postgres");
        assert_eq!(parsed.unwrap().1, 5432);
    }

    #[test]
    fn parses_url_with_empty_password() {
        let parsed = parse_admin_url("postgres://alice@localhost:5432/postgres");
        assert_eq!(parsed.unwrap().3, "");
    }

    #[test]
    fn rejects_non_postgres_scheme() {
        assert!(parse_admin_url("mysql://alice@host/db").is_none());
    }

    #[test]
    fn pg_ctl_stop_targets_the_embedded_binary_and_data_dir() {
        let settings = Settings {
            installation_dir: "/opt/pg".into(),
            data_dir: "/tmp/pgdata".into(),
            ..Default::default()
        };
        let cmd = pg_ctl_stop(&settings);
        assert_eq!(
            cmd.get_program(),
            settings.binary_dir().join("pg_ctl").as_os_str()
        );
        let args: Vec<_> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args, ["stop", "-m", "fast", "-w", "-D", "/tmp/pgdata"]);
    }
}

#[cfg(test)]
mod leaks {
    use super::*;

    /// A test that panics leaves no database behind, as one that passes
    /// leaves none.
    #[test]
    fn a_test_that_panics_leaves_no_database_behind() {
        if std::env::var("WORKFORCE_SKIP_POSTGRES_TESTS").is_ok() {
            return;
        }
        let named = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let run = |panics: bool| {
            let named = Arc::clone(&named);
            std::thread::spawn(move || {
                let runtime = tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                runtime.block_on(with_test_pool(
                    PostgresPool::from_config,
                    |pool| async move {
                        let client = pool.get().await.unwrap();
                        let row = client
                            .query_one("SELECT current_database()", &[])
                            .await
                            .unwrap();
                        named.lock().unwrap().push(row.get(0));
                        drop(client);
                        assert!(!panics, "the test fails");
                    },
                ));
            })
            .join()
        };
        assert!(run(false).is_ok());
        assert!(run(true).is_err(), "it panicked");

        let named = named.lock().unwrap().clone();
        assert_eq!(named.len(), 2);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let left: i64 = runtime.block_on(async {
            let shared = shared_postgres().await;
            let admin_url = {
                let s = shared.lock().await;
                format!(
                    "postgres://{}:{}@{}:{}/postgres",
                    s.username(),
                    s.password(),
                    s.host(),
                    s.port()
                )
            };
            let (client, conn) = tokio_postgres::connect(&admin_url, tokio_postgres::NoTls)
                .await
                .unwrap();
            tokio::spawn(async move {
                let _ = conn.await;
            });
            let row = client
                .query_one(
                    "SELECT count(*) FROM pg_database WHERE datname = ANY($1)",
                    &[&named],
                )
                .await
                .unwrap();
            row.get(0)
        });
        assert_eq!(left, 0, "of {named:?}");
    }
}
