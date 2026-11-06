//! Per-agent session store selection.
//!
//! Picks `JsonlSessionStore(workspace)` (default) or the shared
//! `PostgresSessionStore(pool, agent_id)` when `[sessions] backend =
//! "postgres"`. Backend resolution is fail-closed via
//! [`workforce_common::backend::resolve_store_backend`]: a deployment
//! configured for Postgres never silently degrades to local JSONL (which on a
//! multi-instance install would split session state across hosts) — it fails
//! startup instead.
//!
//! The Postgres backend is shared across all agents on ONE schema-pinned pool
//! (scoped per agent by `agent_id`). The pool is built — and its migrations run
//! — exactly once, memoised so the per-agent `build_inner` calls reuse it. A
//! failed first build is not cached, so a transient outage that exhausts the
//! connect budget surfaces as a startup error rather than a permanent silent
//! downgrade.

use std::path::Path;
use std::sync::Arc;

use workforce_common::session::{JsonlSessionStore, SessionStore};
use workforce_common::settings::WorkforceConfig;

#[cfg(feature = "postgres")]
use {
    std::time::Duration, tokio::sync::OnceCell, workforce_common::backend::connect_with_retry,
    workforce_common::postgres::PostgresPool,
    workforce_common::session_postgres::PostgresSessionStore,
};

/// The shared, already-migrated sessions pool, built on first use. Only a
/// successful build is cached (`get_or_try_init`), so a failed connect retries
/// on the next agent build instead of poisoning the process.
#[cfg(feature = "postgres")]
static SESSIONS_POOL: OnceCell<PostgresPool> = OnceCell::const_new();

/// Build the session store for one agent. Fail-closed: a `"postgres"`
/// deployment never silently degrades to local JSONL — `build_postgres_session_store`
/// (and the pool builder it calls) returns an error when Postgres is
/// unavailable, which aborts startup. `"jsonl"` is the local default.
pub async fn build_session_store(
    config: &WorkforceConfig,
    workspace: &Path,
    agent_id: &str,
) -> eyre::Result<Arc<dyn SessionStore>> {
    match config.sessions.backend.as_str() {
        "jsonl" | "" => Ok(Arc::new(JsonlSessionStore::new(workspace)?)),
        "postgres" => build_postgres_session_store(config, agent_id).await,
        other => {
            eyre::bail!("[sessions] unknown backend {other:?}; expected \"postgres\" or \"jsonl\"")
        }
    }
}

/// On a hub that serves no agents, forget the agents it served before that
/// its `[desk] forget_agents` names (a machine's old Desk agent): each one's
/// chat sessions ([`PostgresSessionStore::forget_agent`]) and its Floor
/// rows (`floor.turns` and `floor.gists` of subject `agent:<id>`), and
/// nothing else. Once each: a marker in `done` says it was, and a later
/// start leaves it be. Only sessions kept in Postgres. The agents forgotten
/// now, with their sessions' count.
pub async fn forget_retired_agents(
    config: &WorkforceConfig,
    done: &Path,
) -> eyre::Result<Vec<(String, u64)>> {
    let mut forgotten = Vec::new();
    if config.serves_agents() || config.sessions.backend != "postgres" {
        return Ok(forgotten);
    }
    for agent in &config.desk.forget_agents {
        let marker = done.join(format!("forgotten-agent-{agent}"));
        if agent.is_empty()
            || !agent
                .chars()
                .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
            || marker.exists()
        {
            continue;
        }
        #[cfg(feature = "postgres")]
        {
            let pool = shared_sessions_pool(config).await?;
            let sessions = PostgresSessionStore::forget_agent(&pool, agent).await?;
            forget_floor_rows(&pool, agent).await?;
            std::fs::create_dir_all(done)?;
            std::fs::write(&marker, "")?;
            forgotten.push((agent.clone(), sessions));
        }
    }
    Ok(forgotten)
}

/// `agent`'s rows on the Floor: its turns and its gist, by its subject key,
/// named in the Floor's schema. None where this hub has kept no Floor yet.
#[cfg(feature = "postgres")]
async fn forget_floor_rows(pool: &PostgresPool, agent: &str) -> eyre::Result<()> {
    // The Floor's subject key of an agent (`SubjectKey::agent`).
    let subject = format!("agent:{agent}");
    let mut conn = pool.get().await?;
    let tx = conn.transaction().await?;
    for table in ["floor.gists", "floor.turns"] {
        let kept: bool = tx
            .query_one("SELECT to_regclass($1) IS NOT NULL", &[&table])
            .await?
            .get(0);
        if kept {
            tx.execute(
                &format!("DELETE FROM {table} WHERE subject = $1"),
                &[&subject],
            )
            .await?;
        }
    }
    tx.commit().await?;
    Ok(())
}

/// Whether this hub's sessions are in Postgres, beside which a person's
/// Desk read points are kept.
pub(crate) fn keeps_desk_reads() -> bool {
    #[cfg(feature = "postgres")]
    return SESSIONS_POOL.initialized();
    #[cfg(not(feature = "postgres"))]
    false
}

#[cfg(feature = "postgres")]
async fn build_postgres_session_store(
    config: &WorkforceConfig,
    agent_id: &str,
) -> eyre::Result<Arc<dyn SessionStore>> {
    let pool = shared_sessions_pool(config).await?;
    tracing::info!(agent = agent_id, "Using Postgres session store");
    Ok(PostgresSessionStore::with_shared_pool(pool, agent_id) as Arc<dyn SessionStore>)
}

#[cfg(not(feature = "postgres"))]
async fn build_postgres_session_store(
    _config: &WorkforceConfig,
    _agent_id: &str,
) -> eyre::Result<Arc<dyn SessionStore>> {
    eyre::bail!(
        "[sessions] backend = \"postgres\" but workforce-hub was built without the postgres feature"
    )
}

/// Build (once) the shared sessions pool and run its migrations, retrying the
/// connection for the configured budget so a cold restart doesn't lose the
/// race. Errors propagate so a Postgres-configured deployment fails closed.
#[cfg(feature = "postgres")]
async fn shared_sessions_pool(config: &WorkforceConfig) -> eyre::Result<PostgresPool> {
    SESSIONS_POOL
        .get_or_try_init(|| async {
            let pg_cfg = config
                .postgres
                .as_ref()
                .filter(|p| p.enabled)
                .ok_or_else(|| {
                    eyre::eyre!("[sessions] backend = \"postgres\" but [postgres] is not enabled")
                })?;
            let budget = Duration::from_secs(pg_cfg.connect_timeout_secs);
            let pool = connect_with_retry(
                "sessions-postgres",
                budget,
                Duration::from_secs(5),
                || async {
                    let pool = PostgresSessionStore::build_pool(pg_cfg)?;
                    pool.ping().await?;
                    Ok(pool)
                },
            )
            .await?;
            workforce_common::session_postgres::run_session_migrations(&pool).await?;
            reject_edge_database(&pool).await?;
            tracing::info!("Sessions Postgres pool initialised and migrated");
            Ok(pool)
        })
        .await
        .cloned()
}

/// Refuse a database the edge has made its own. The edge's tenancy set runs
/// in the same `sessions` schema and puts row-level security on the session
/// tables, keyed on a transaction setting the hub never sets: a hub pointed
/// at such a database would read and write nothing, and no query would say
/// so. Its migration history table is the mark.
#[cfg(feature = "postgres")]
async fn reject_edge_database(pool: &PostgresPool) -> eyre::Result<()> {
    let conn = pool.get().await?;
    let carries_edge_history: bool = conn
        .query_one(
            "SELECT to_regclass('sessions.refinery_edge_history') IS NOT NULL",
            &[],
        )
        .await?
        .get(0);
    if carries_edge_history {
        eyre::bail!(
            "[sessions] this database carries the edge's tenancy migrations; \
             the hub must not share it"
        );
    }
    Ok(())
}
