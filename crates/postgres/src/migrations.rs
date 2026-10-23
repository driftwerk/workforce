//! Refinery-backed migration runner.
//!
//! Each store that gains a Postgres backend owns its own embedded
//! migration set, isolated under its own Postgres schema (so a future
//! roll-back of one store can't affect another). Stores construct a
//! [`MigrationSet`] from a refinery `Runner` and pass it to
//! [`MigrationRunner::run`] at startup. Refinery's `embed_migrations!`
//! macro reads the actual SQL files at compile time inside each store
//! crate, so this module never touches disk at runtime.

use async_trait::async_trait;

use super::{PostgresError, PostgresPool};

/// Trait that each store implements to run its own embedded refinery
/// migrations against an existing Postgres client.
#[async_trait]
pub trait MigrationRun: Send + Sync {
    async fn run(&self, client: &mut tokio_postgres::Client) -> Result<(), PostgresError>;
}

/// A namespaced bundle of migrations for a single store.
///
/// Each store owns its own `embed_migrations!` expansion in-crate;
/// only the resulting [`MigrationRun`] handle and target schema name
/// cross the module boundary.
pub struct MigrationSet {
    schema: String,
    runner: Box<dyn MigrationRun>,
}

impl MigrationSet {
    pub fn new(schema: impl Into<String>, runner: Box<dyn MigrationRun>) -> Self {
        Self {
            schema: schema.into(),
            runner,
        }
    }

    pub fn schema(&self) -> &str {
        &self.schema
    }
}

/// Global advisory-lock key guarding every migration run. Held for the
/// duration of [`MigrationRunner::run`] so concurrent instance boots can't
/// race `CREATE TABLE` / refinery's history table. Transaction-scoped
/// (`pg_advisory_xact_lock`), so it releases on commit, rollback, or a dropped
/// connection (e.g. a crashed instance) — no stuck lock and no manual guard.
/// The value is ASCII `wkmgrate`; it must stay stable across versions so older
/// and newer instances serialise on the same key during a rolling deploy.
const MIGRATION_ADVISORY_LOCK_KEY: i64 = 0x776B_6D67_7261_7465;

/// Applies one or more [`MigrationSet`]s against a shared pool.
///
/// For each set: ensures the target schema exists, sets `search_path`
/// for the connection, and invokes the embedded refinery runner.
/// Schemas are independent — a failure in one set does not roll back
/// migrations applied to another.
pub struct MigrationRunner;

impl MigrationRunner {
    pub async fn run(pool: &PostgresPool, sets: &[MigrationSet]) -> Result<(), PostgresError> {
        let mut lock_conn = pool.get().await?;
        let lock_tx = lock_conn.transaction().await?;
        // Bound the wait for the advisory lock so a holder that is alive but
        // wedged mid-migration surfaces as a loud error instead of an
        // indefinite silent boot hang. Scoped to this transaction — which only
        // acquires the lock — so it never bounds the migrations themselves
        // (they run on separate connections). A crashed holder releases the
        // lock automatically when its session ends.
        lock_tx
            .batch_execute("SET LOCAL statement_timeout = '120s'")
            .await?;
        lock_tx
            .execute(
                "SELECT pg_advisory_xact_lock($1)",
                &[&MIGRATION_ADVISORY_LOCK_KEY],
            )
            .await?;

        match Self::apply_sets(pool, sets).await {
            Ok(()) => {
                lock_tx.commit().await?;
                Ok(())
            }
            Err(e) => {
                let _ = lock_tx.rollback().await;
                Err(e)
            }
        }
    }

    async fn apply_sets(pool: &PostgresPool, sets: &[MigrationSet]) -> Result<(), PostgresError> {
        for set in sets {
            let mut conn = pool.get().await?;
            let client: &mut tokio_postgres::Client = &mut conn;
            // Capture the connection's `search_path` so the `SET search_path TO
            // <schema>` this migration issues does not leak back into the pool.
            // Pool connections are reused, so a connection left on the migration's
            // schema would resolve a *later* caller's unqualified query in the
            // wrong schema — a boot-time "relation does not exist". `SHOW`'s output
            // is a valid `SET` argument, and its value is the server's own (never
            // caller input), so re-applying it is injection-safe.
            let prior_search_path: String = client.query_one("SHOW search_path", &[]).await?.get(0);
            client
                .simple_query(&format!(
                    "CREATE SCHEMA IF NOT EXISTS {schema}; SET search_path TO {schema}",
                    schema = quote_ident(set.schema())
                ))
                .await?;
            let run = set.runner.run(client).await;
            // Restore before the connection returns to the pool, even if the
            // migration failed (a poisoned connection must never re-enter the pool).
            let restore = client
                .simple_query(&format!("SET search_path TO {prior_search_path}"))
                .await;
            run?;
            restore?;
            tracing::info!(schema = %set.schema(), "Postgres migrations applied");
        }
        Ok(())
    }
}

/// Quote a Postgres identifier defensively. Schema names come from
/// config files, so we wrap unconditionally and double any embedded `"`
/// to keep schema names from breaking out of the quoting.
fn quote_ident(name: &str) -> String {
    let escaped = name.replace('"', "\"\"");
    format!("\"{escaped}\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quote_ident_wraps_simple_names() {
        assert_eq!(quote_ident("telemetry"), "\"telemetry\"");
    }

    #[test]
    fn quote_ident_escapes_embedded_quotes() {
        assert_eq!(quote_ident("evil\"name"), "\"evil\"\"name\"");
    }

    #[test]
    fn quote_ident_handles_dots() {
        assert_eq!(quote_ident("foo.bar"), "\"foo.bar\"");
    }

    #[test]
    fn migration_advisory_lock_key_is_stable() {
        assert_eq!(MIGRATION_ADVISORY_LOCK_KEY, 0x776B_6D67_7261_7465);
    }
}

#[cfg(all(test, feature = "test-support"))]
mod pg_tests {
    use super::*;
    use crate::test_support::with_test_pool;
    use crate::PostgresPool;

    struct MarkerMigration;

    #[async_trait]
    impl MigrationRun for MarkerMigration {
        async fn run(&self, client: &mut tokio_postgres::Client) -> Result<(), PostgresError> {
            client
                .batch_execute(
                    "CREATE TABLE IF NOT EXISTS boot_marker (id int PRIMARY KEY); \
                     INSERT INTO boot_marker (id) VALUES (1) ON CONFLICT DO NOTHING;",
                )
                .await?;
            Ok(())
        }
    }

    /// Two instances booting at once must not race `CREATE TABLE` / refinery
    /// history: the advisory lock serialises them, so both succeed and the
    /// migration applies exactly once.
    #[tokio::test]
    async fn concurrent_runs_are_serialised() {
        with_test_pool(PostgresPool::from_config, |pool| async move {
            let run_once = || async {
                MigrationRunner::run(
                    &pool,
                    &[MigrationSet::new("hscale_boot", Box::new(MarkerMigration))],
                )
                .await
            };

            let (first, second) = tokio::join!(run_once(), run_once());
            first.expect("first concurrent migration run");
            second.expect("second concurrent migration run");

            let conn = pool.get().await.expect("pool connection");
            let row = conn
                .query_one("SELECT count(*)::int FROM hscale_boot.boot_marker", &[])
                .await
                .expect("count marker rows");
            assert_eq!(
                row.get::<_, i32>(0),
                1,
                "migration applied exactly once under concurrency"
            );
        })
        .await;
    }

    /// Deterministic proof of exclusivity on the real lock key: while one
    /// session holds the lock a second cannot take it, and it becomes
    /// available again once the holder's transaction ends.
    #[tokio::test]
    async fn advisory_lock_is_exclusive() {
        with_test_pool(PostgresPool::from_config, |pool| async move {
            let mut holder = pool.get().await.expect("holder connection");
            let holder_tx = holder.transaction().await.expect("holder transaction");
            let acquired: bool = holder_tx
                .query_one(
                    "SELECT pg_try_advisory_xact_lock($1)",
                    &[&MIGRATION_ADVISORY_LOCK_KEY],
                )
                .await
                .expect("holder acquire")
                .get(0);
            assert!(acquired, "holder acquires the lock");

            let contender = pool.get().await.expect("contender connection");
            let blocked: bool = contender
                .query_one(
                    "SELECT pg_try_advisory_xact_lock($1)",
                    &[&MIGRATION_ADVISORY_LOCK_KEY],
                )
                .await
                .expect("contender attempt")
                .get(0);
            assert!(!blocked, "a second session cannot acquire while held");

            holder_tx.commit().await.expect("release lock");

            let reacquired: bool = contender
                .query_one(
                    "SELECT pg_try_advisory_xact_lock($1)",
                    &[&MIGRATION_ADVISORY_LOCK_KEY],
                )
                .await
                .expect("contender retry")
                .get(0);
            assert!(reacquired, "lock is acquirable once the holder releases");
        })
        .await;
    }

    /// A blocked acquire is time-bounded (the `statement_timeout` guard in
    /// `run`), so a wedged holder can never hang another instance's boot
    /// indefinitely.
    #[tokio::test]
    async fn advisory_lock_wait_is_bounded() {
        with_test_pool(PostgresPool::from_config, |pool| async move {
            let mut holder = pool.get().await.expect("holder connection");
            let holder_tx = holder.transaction().await.expect("holder transaction");
            holder_tx
                .execute(
                    "SELECT pg_advisory_xact_lock($1)",
                    &[&MIGRATION_ADVISORY_LOCK_KEY],
                )
                .await
                .expect("holder acquire");

            let contender = pool.get().await.expect("contender connection");
            contender
                .batch_execute("SET statement_timeout = '300ms'")
                .await
                .expect("set timeout");
            let result = contender
                .execute(
                    "SELECT pg_advisory_xact_lock($1)",
                    &[&MIGRATION_ADVISORY_LOCK_KEY],
                )
                .await;
            assert!(
                result.is_err(),
                "a blocked acquire is bounded, not an indefinite hang"
            );

            holder_tx.commit().await.expect("release lock");
        })
        .await;
    }

    /// A migration set targeting a non-default schema must RESTORE the pooled
    /// connection's `search_path`. Otherwise the connection re-enters the pool on
    /// the migration's schema and a later caller's UNQUALIFIED query resolves in
    /// the wrong place — the exact edge boot crash (`edge_analytics` search_path
    /// left on a shared-pool connection broke the config store's unqualified
    /// `edge_playbooks` query). `pool_size = 2` covers the runner's own lock
    /// connection plus the apply connection; both must come back clean.
    #[tokio::test]
    async fn migration_restores_search_path_on_pooled_connections() {
        with_test_pool(
            |cfg| {
                let mut cfg = cfg.clone();
                cfg.pool_size = 2;
                PostgresPool::from_config_with_session_init(
                    &cfg,
                    &["SET search_path TO probe_home, public"],
                )
            },
            |pool| async move {
                {
                    let conn = pool.get().await.expect("setup connection");
                    conn.batch_execute(
                        "CREATE SCHEMA IF NOT EXISTS probe_home; \
                         CREATE TABLE IF NOT EXISTS probe_home.probe (id int);",
                    )
                    .await
                    .expect("create probe table in the pool's default schema");
                }
                MigrationRunner::run(
                    &pool,
                    &[MigrationSet::new(
                        "probe_migration",
                        Box::new(MarkerMigration),
                    )],
                )
                .await
                .expect("migration under a non-default schema");
                // Hold BOTH pooled connections and confirm each still resolves
                // `probe` unqualified — i.e. the migration's connection was
                // restored, not left on `probe_migration`. Without the restore,
                // whichever connection ran the migration fails this query.
                let c1 = pool.get().await.expect("connection 1");
                let c2 = pool.get().await.expect("connection 2");
                for c in [&c1, &c2] {
                    c.query("SELECT count(*) FROM probe", &[]).await.expect(
                        "unqualified probe must resolve on every pooled connection \
                         after the migration (search_path restored)",
                    );
                }
            },
        )
        .await;
    }
}
