//! Postgres backend for [`crate::session::SessionStore`].
//!
//! Part of active-active horizontal scaling (inc1b). `JsonlSessionStore`
//! keeps an authoritative in-memory cache per agent workspace, so two hubs
//! diverge and the full-rewrite `save` loses updates. This backend moves the
//! session store to the shared Postgres so any instance serves any session,
//! and it is read-through (no authoritative cache).
//!
//! ## Async-to-sync bridge
//!
//! [`crate::session::SessionStore`] is synchronous (matching the historical
//! file API). `tokio_postgres` is async. We bridge with the same shim every
//! other store uses, hardened so it never panics when a store method is
//! called from inside `spawn_blocking` (the redaction sweep does this): on a
//! multi-threaded runtime [`tokio::task::block_in_place`] offloads the
//! blocking await; otherwise the current (blocking-pool) thread drives the
//! future directly via `Handle::block_on`.
//!
//! ## Concurrency model
//!
//! - `get_or_create` → `INSERT … ON CONFLICT DO NOTHING` then select-active,
//!   so two concurrent creators converge on one active session (the partial
//!   unique index `(tenant_id, agent_id, channel, owner) WHERE NOT sealed`,
//!   and on a hub the single-tenant one it keeps beside it; see V011).
//!
//! ## Tenants
//!
//! Every row carries a `tenant_id`; a store built with
//! [`PostgresSessionStore::with_shared_pool_in_tenant`] binds its tenant into
//! every query and runs each method in a transaction that publishes it as
//! `app.tenant_id`, for row-level security to enforce the same boundary. A
//! store without a tenant (the hub) reads and writes the `'default'` tenant
//! outside any such transaction, as it always has.
//! - `save` → optimistic `revision` with append-preserving conflict
//!   resolution inside one `SELECT … FOR UPDATE` transaction: if the row's
//!   `revision` still equals the caller's `loaded_revision`, apply the
//!   incoming `messages` exactly; otherwise append only the incoming
//!   messages whose `id` isn't already stored. This never loses an appended
//!   turn while still letting compaction/edits (which reduce messages) apply
//!   when uncontended.
//! - Mutations (`reset`/`truncate`/`delete_message`/redaction/model/label)
//!   run under `SELECT … FOR UPDATE`, honouring the trait's "mutate under the
//!   store lock" contract.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use deadpool_postgres::{Object, Transaction};
use eyre::{Context, Result};
use tokio_postgres::types::ToSql;
use uuid::Uuid;

use crate::postgres::{
    MigrationRun, MigrationRunner, MigrationSet, PostgresConfig, PostgresError, PostgresPool,
};
use crate::session::{
    compute_stats, context_usage_for, coordination_score, format_excerpt_rows,
    message_to_transcript_entry, paginate_entries, rank_search_results, reassigned_channel,
    redact_message_secrets, redact_messages_in_place, search_terms, tool_calls_for, ArchivePage,
    InterruptedTurn, JournalEntry, MessageRole, Reassigned, RedactionStats, Session,
    SessionContextType, SessionContextUsage, SessionExport, SessionKind, SessionMessage,
    SessionSearchResult, SessionStats, SessionStore, ToolCallRecord, TranscriptEntry,
    TurnPrincipal,
};

mod embedded {
    refinery::embed_migrations!("migrations/sessions");
}

/// How stale a heartbeat marks its owner dead. Six missed beats at the hub's
/// 15s cadence — long enough to ride out a GC pause or a saturated pool,
/// short enough that a crashed instance's sessions free in under two minutes.
pub const HEARTBEAT_STALE_SECS: u64 = 90;

/// The tenant every row belongs to when the store is not scoped to one: the
/// hub has no tenants, so all of its rows carry this value.
const DEFAULT_TENANT: &str = "default";

/// Postgres backend for the conversation session store. Shared across all
/// agents on one pool; every query is scoped by `tenant_id` and `agent_id`.
pub struct PostgresSessionStore {
    pool: PostgresPool,
    /// The tenant whose rows this store reads and writes. `None` is the hub,
    /// which has no tenants: its rows live under [`DEFAULT_TENANT`] and its
    /// queries run outside any tenant transaction.
    tenant: Option<String>,
    agent_id: String,
    /// Identifies this process in `turn_claims.owner_instance` and
    /// `instance_heartbeats` — the process-wide id, fresh-nonced per boot, so
    /// a restarted process never matches its predecessor's claims.
    instance_id: String,
}

/// The connection a store method runs on. Scoped to a tenant it is a
/// transaction carrying `app.tenant_id` so a row-level security policy can
/// read it; the setting is transaction-local, so it ends with the
/// transaction instead of riding the pooled connection to its next borrower
/// (the same leak `workforce_postgres::migrations` guards `search_path`
/// against). Unscoped — the hub — it is the bare connection as before.
enum Scoped<'a> {
    Plain(&'a Object),
    Tenant(Transaction<'a>),
}

impl Scoped<'_> {
    async fn execute(
        &self,
        statement: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<u64, tokio_postgres::Error> {
        match self {
            Self::Plain(conn) => conn.execute(statement, params).await,
            Self::Tenant(tx) => tx.execute(statement, params).await,
        }
    }

    async fn query(
        &self,
        statement: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<Vec<tokio_postgres::Row>, tokio_postgres::Error> {
        match self {
            Self::Plain(conn) => conn.query(statement, params).await,
            Self::Tenant(tx) => tx.query(statement, params).await,
        }
    }

    async fn query_one(
        &self,
        statement: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<tokio_postgres::Row, tokio_postgres::Error> {
        match self {
            Self::Plain(conn) => conn.query_one(statement, params).await,
            Self::Tenant(tx) => tx.query_one(statement, params).await,
        }
    }

    async fn query_opt(
        &self,
        statement: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<Option<tokio_postgres::Row>, tokio_postgres::Error> {
        match self {
            Self::Plain(conn) => conn.query_opt(statement, params).await,
            Self::Tenant(tx) => tx.query_opt(statement, params).await,
        }
    }

    /// Commit the tenant transaction, if there is one.
    async fn finish(self) -> Result<()> {
        if let Self::Tenant(tx) = self {
            tx.commit().await?;
        }
        Ok(())
    }
}

impl PostgresSessionStore {
    /// Schema this store owns. All session tables live under it.
    pub const SCHEMA: &'static str = "sessions";

    /// Build a [`PostgresPool`] with `search_path` pinned to the sessions
    /// schema, so unqualified table names always resolve. Used by
    /// [`Self::new`] / [`Self::with_pool`] and reusable by the migration tool.
    pub fn build_pool(config: &PostgresConfig) -> Result<PostgresPool, PostgresError> {
        PostgresPool::from_config_with_session_init(
            config,
            &["SET search_path TO sessions, public"],
        )
    }

    /// Build a schema-pinned pool, run pending migrations once, and return a
    /// store handle scoped to `agent_id`.
    pub async fn new(config: &PostgresConfig, agent_id: impl Into<String>) -> Result<Arc<Self>> {
        let pool = Self::build_pool(config).context("building sessions pool")?;
        Self::with_pool(pool, agent_id).await
    }

    /// Variant taking an already-built (schema-pinned) pool. Runs migrations;
    /// idempotent under the shared advisory lock. Used by tests and by the
    /// hub, which builds one shared pool and constructs per-agent stores from
    /// it via [`Self::with_shared_pool`].
    pub async fn with_pool(pool: PostgresPool, agent_id: impl Into<String>) -> Result<Arc<Self>> {
        run_session_migrations(&pool).await?;
        Ok(Self::with_shared_pool(pool, agent_id))
    }

    /// Construct a per-agent store from an already-migrated shared pool,
    /// without re-running migrations. The hub builds the pool + runs
    /// migrations once at startup, then mints one of these per agent.
    pub fn with_shared_pool(pool: PostgresPool, agent_id: impl Into<String>) -> Arc<Self> {
        Arc::new(Self {
            pool,
            tenant: None,
            agent_id: agent_id.into(),
            instance_id: crate::instance::process_instance_id().to_string(),
        })
    }

    /// A per-agent store confined to one tenant's rows. Every method runs in
    /// a transaction that publishes the tenant as `app.tenant_id`, so a
    /// row-level security policy on the session tables can enforce the same
    /// boundary the queries draw.
    pub fn with_shared_pool_in_tenant(
        pool: PostgresPool,
        tenant: impl Into<String>,
        agent_id: impl Into<String>,
    ) -> Arc<Self> {
        Arc::new(Self {
            pool,
            tenant: Some(tenant.into()),
            agent_id: agent_id.into(),
            instance_id: crate::instance::process_instance_id().to_string(),
        })
    }

    /// Same store shape with an explicit owner identity — tests use this to
    /// model a second hub process on the shared pool.
    #[cfg(test)]
    fn with_shared_pool_as(
        pool: PostgresPool,
        agent_id: impl Into<String>,
        instance_id: impl Into<String>,
    ) -> Arc<Self> {
        Arc::new(Self {
            pool,
            tenant: None,
            agent_id: agent_id.into(),
            instance_id: instance_id.into(),
        })
    }

    /// The tenant bound into every query: the scoped tenant, or the default
    /// every unscoped row belongs to.
    fn tenant_id(&self) -> &str {
        self.tenant.as_deref().unwrap_or(DEFAULT_TENANT)
    }

    /// The connection a method runs its queries on: a tenant transaction
    /// when scoped, the bare connection otherwise. Pair with
    /// [`Scoped::finish`].
    async fn scope<'a>(&self, conn: &'a mut Object) -> Result<Scoped<'a>> {
        if self.tenant.is_none() {
            return Ok(Scoped::Plain(conn));
        }
        let tx = conn.transaction().await?;
        self.bind_tenant(&tx).await?;
        Ok(Scoped::Tenant(tx))
    }

    /// Publish the scoped tenant to a transaction this method opened itself.
    /// `set_config(…, true)` is transaction-local: the setting cannot
    /// outlive the transaction, so it never reaches the pool's next borrower.
    async fn bind_tenant(&self, tx: &Transaction<'_>) -> Result<()> {
        if let Some(tenant) = &self.tenant {
            tx.execute("SELECT set_config('app.tenant_id', $1, true)", &[tenant])
                .await?;
        }
        Ok(())
    }

    /// What a scoped query sees as `app.tenant_id`, read the way a row-level
    /// security policy would.
    #[cfg(test)]
    async fn tenant_setting_as_seen_by_a_query(&self) -> Result<Option<String>> {
        let mut conn = self.pool.get().await?;
        let scoped = self.scope(&mut conn).await?;
        let row = scoped
            .query_one("SELECT current_setting('app.tenant_id', true)", &[])
            .await?;
        scoped.finish().await?;
        Ok(row.get(0))
    }

    /// Run a `Result`-returning future to completion, blocking the calling
    /// thread. Safe from a runtime worker (`block_in_place`) and from a
    /// blocking-pool thread (`Handle::block_on`) — see the module note.
    fn block_on<F, T>(&self, fut: F) -> Result<T>
    where
        F: std::future::Future<Output = Result<T>>,
    {
        let handle = tokio::runtime::Handle::current();
        match handle.runtime_flavor() {
            tokio::runtime::RuntimeFlavor::MultiThread => {
                tokio::task::block_in_place(|| handle.block_on(fut))
            }
            _ => handle.block_on(fut),
        }
    }
}

/// Apply the embedded sessions migration set (V001..) against a pool.
/// Idempotent — refinery records applied versions and skips them.
pub async fn run_session_migrations(pool: &PostgresPool) -> Result<()> {
    let set = MigrationSet::new(
        PostgresSessionStore::SCHEMA,
        Box::new(EmbeddedSessionMigrations) as Box<dyn MigrationRun>,
    );
    MigrationRunner::run(pool, &[set])
        .await
        .context("running session migrations")?;
    Ok(())
}

struct EmbeddedSessionMigrations;

#[async_trait::async_trait]
impl MigrationRun for EmbeddedSessionMigrations {
    async fn run(&self, client: &mut tokio_postgres::Client) -> Result<(), PostgresError> {
        embedded::migrations::runner()
            .run_async(client)
            .await
            .map_err(PostgresError::from)?;
        Ok(())
    }
}

// ─── Row helpers ─────────────────────────────────────────────────────────

/// Ceiling on one agent's interrupted-turn scan. Reached only on the first
/// sweep after an upgrade, when every pre-migration marker in the table
/// becomes visible at once; the sweep repeats every 15 minutes, so a bounded
/// pass still drains the backlog.
const RESUME_SCAN_LIMIT: i64 = 1_000;

fn parse_uuid(id: &str) -> Result<Uuid> {
    Uuid::parse_str(id).with_context(|| format!("invalid session UUID: {id}"))
}

/// Metadata keys where a stale-baseline save's intent is discarded: the
/// caller's value differs from the stored one, whether that is an addition, a
/// change, or a removal the merge declines to apply. Pure; drives the warning
/// that makes a dropped write visible instead of leaving it to be
/// reconstructed from behaviour weeks later.
/// Name what a save's metadata intent lost, on the two branches that keep the
/// stored blob instead of the caller's. Silence here is what made a dropped
/// marker take a forensic dig to find.
fn warn_discarded_metadata(
    session: &Session,
    incoming: &serde_json::Value,
    stored_row: &tokio_postgres::Row,
    branch: &'static str,
) {
    let stored: serde_json::Value = stored_row.get(7);
    let discarded = discarded_metadata_keys(incoming, &stored);
    if !discarded.is_empty() {
        tracing::warn!(
            session_id = %session.id,
            branch,
            baseline = ?session.loaded_revision,
            current = stored_row.get::<_, i64>(0),
            discarded_metadata_keys = ?discarded,
            "Session save: metadata not applied"
        );
    }
}

fn discarded_metadata_keys(
    incoming: &serde_json::Value,
    stored: &serde_json::Value,
) -> Vec<String> {
    let empty = serde_json::Map::new();
    let incoming = incoming.as_object().unwrap_or(&empty);
    let stored = stored.as_object().unwrap_or(&empty);
    incoming
        .keys()
        .chain(stored.keys())
        .filter(|key| incoming.get(*key) != stored.get(*key))
        .cloned()
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn kind_to_str(kind: &SessionKind) -> &'static str {
    match kind {
        SessionKind::Main => "main",
        SessionKind::Isolated => "isolated",
        SessionKind::SubAgent => "sub_agent",
    }
}

fn kind_from_str(s: &str) -> SessionKind {
    match s {
        "isolated" => SessionKind::Isolated,
        "sub_agent" => SessionKind::SubAgent,
        _ => SessionKind::Main,
    }
}

fn context_type_to_str(ct: &SessionContextType) -> &'static str {
    match ct {
        SessionContextType::Dm => "dm",
        SessionContextType::Group => "group",
        SessionContextType::Thread => "thread",
    }
}

fn context_type_from_str(s: &str) -> Option<SessionContextType> {
    match s {
        "dm" => Some(SessionContextType::Dm),
        "group" => Some(SessionContextType::Group),
        "thread" => Some(SessionContextType::Thread),
        _ => None,
    }
}

fn role_to_str(role: &MessageRole) -> &'static str {
    match role {
        MessageRole::User => "user",
        MessageRole::Assistant => "assistant",
        MessageRole::System => "system",
        MessageRole::Tool => "tool",
    }
}

/// Columns selected by every full-session read, in `row_to_session` order.
const SESSION_COLUMNS: &str = "id, label, channel, owner, agent_id, kind, context_type, \
     created_at, updated_at, metadata, total_tokens, compaction_history, model_override, \
     message_count, messages, revision";

/// Build a hydrated [`Session`] from a full row. `messages` comes from the
/// JSONB blob; `loaded_revision` is stamped from the row for the
/// append-preserving `save`.
fn row_to_session(r: &tokio_postgres::Row) -> Result<Session> {
    let id: Uuid = r.get(0);
    let context_type: Option<String> = r.get(6);
    let compaction_json: serde_json::Value = r.get(11);
    let messages_json: serde_json::Value = r.get(14);
    Ok(Session {
        id: id.to_string(),
        label: r.get(1),
        channel: r.get(2),
        owner: r.get(3),
        agent_id: r.get(4),
        kind: kind_from_str(r.get::<_, &str>(5)),
        context_type: context_type.as_deref().and_then(context_type_from_str),
        messages: serde_json::from_value(messages_json).context("decode session messages")?,
        created_at: r.get(7),
        updated_at: r.get(8),
        metadata: r.get(9),
        total_tokens: r.get::<_, i64>(10) as u64,
        compaction_history: serde_json::from_value(compaction_json)
            .context("decode compaction history")?,
        model_override: r.get(12),
        cached_message_count: None,
        loaded_revision: Some(r.get::<_, i64>(15)),
    })
}

/// Build a session shell (empty `messages`, populated `message_count`,
/// `loaded_revision` set) from a row that does NOT select `messages`. Used by
/// `list`/`list_since`/`find` so the lifecycle sweep's no-hydration
/// assumption holds. Column order matches [`SHELL_COLUMNS`].
fn row_to_shell(r: &tokio_postgres::Row) -> Result<Session> {
    let id: Uuid = r.get(0);
    let context_type: Option<String> = r.get(6);
    let compaction_json: serde_json::Value = r.get(11);
    let message_count: i32 = r.get(13);
    Ok(Session {
        id: id.to_string(),
        label: r.get(1),
        channel: r.get(2),
        owner: r.get(3),
        agent_id: r.get(4),
        kind: kind_from_str(r.get::<_, &str>(5)),
        context_type: context_type.as_deref().and_then(context_type_from_str),
        messages: Vec::new(),
        created_at: r.get(7),
        updated_at: r.get(8),
        metadata: r.get(9),
        total_tokens: r.get::<_, i64>(10) as u64,
        compaction_history: serde_json::from_value(compaction_json)
            .context("decode compaction history")?,
        model_override: r.get(12),
        // Mirrors SessionMeta::into_session: only mark a shell when there is
        // something to hydrate, so message_count() answers correctly.
        cached_message_count: (message_count > 0).then_some(message_count as usize),
        loaded_revision: Some(r.get::<_, i64>(14)),
    })
}

/// Projection for shells — same as full, minus the heavy `messages` blob.
/// Column order matches [`row_to_shell`].
const SHELL_COLUMNS: &str = "id, label, channel, owner, agent_id, kind, context_type, \
     created_at, updated_at, metadata, total_tokens, compaction_history, model_override, \
     message_count, revision";

/// Serialise the parts of a session the `sessions` row stores. Returns the
/// owned values bound into INSERT/UPDATE; promoted columns mirror the
/// metadata accessors so reads round-trip faithfully.
struct SessionRow {
    id: Uuid,
    messages: serde_json::Value,
    metadata: serde_json::Value,
    compaction_history: serde_json::Value,
    sealed: bool,
    seq: i64,
    prev_session_id: Option<Uuid>,
    message_count: i32,
    total_tokens: i64,
}

impl SessionRow {
    fn from_session(s: &Session) -> Result<Self> {
        // Postgres rejects NUL in text/jsonb; binary tool output can smuggle
        // NULs into message content, and a save must never fail on that.
        let mut messages = serde_json::to_value(&s.messages).context("encode session messages")?;
        crate::pg_text::strip_nuls_json(&mut messages);
        let mut metadata = s.metadata.clone();
        crate::pg_text::strip_nuls_json(&mut metadata);
        let mut compaction_history =
            serde_json::to_value(&s.compaction_history).context("encode compaction history")?;
        crate::pg_text::strip_nuls_json(&mut compaction_history);
        Ok(Self {
            id: parse_uuid(&s.id)?,
            messages,
            metadata,
            compaction_history,
            sealed: s.is_sealed(),
            seq: s.seq() as i64,
            prev_session_id: s.prev_session_id().map(parse_uuid).transpose()?,
            message_count: s.message_count() as i32,
            total_tokens: s.total_tokens as i64,
        })
    }
}

impl SessionStore for PostgresSessionStore {
    fn find(&self, channel: &str, owner: &str) -> Result<Option<Session>> {
        let channel = channel.to_string();
        let owner = owner.to_string();
        self.block_on(async move {
            let mut conn = self.pool.get().await?;
            let scoped = self.scope(&mut conn).await?;
            // The active session is the newest unsealed member sharing
            // (tenant_id, agent_id, channel, owner); the partial-unique index
            // makes this at most one row, but ORDER BY seq DESC matches the
            // file store.
            let row = scoped
                .query_opt(
                    &format!(
                        "SELECT {SHELL_COLUMNS} FROM sessions
                         WHERE agent_id = $1 AND channel = $2 AND owner = $3 AND NOT sealed
                           AND tenant_id = $4
                         ORDER BY seq DESC LIMIT 1"
                    ),
                    &[&self.agent_id, &channel, &owner, &self.tenant_id()],
                )
                .await?;
            scoped.finish().await?;
            row.as_ref().map(row_to_shell).transpose()
        })
    }

    fn get_or_create(&self, channel: &str, owner: &str, agent_id: &str) -> Result<Session> {
        let channel = channel.to_string();
        let owner = owner.to_string();
        let agent_id = agent_id.to_string();
        self.block_on(async move {
            let mut conn = self.pool.get().await?;
            let scoped = self.scope(&mut conn).await?;
            // Insert a fresh active session; a concurrent creator that won the
            // partial-unique race makes this a no-op. Either way the active
            // row is then selected, so both callers converge on one session.
            let mut fresh = Session::new(&channel, &owner, &agent_id);
            fresh.label = Some(format!("{channel}:{owner}"));
            let row = SessionRow::from_session(&fresh)?;
            // Insert a fresh active session; a concurrent creator that won the
            // partial-unique race makes this a no-op. A concurrent *seal* can
            // remove the active row between the insert and the select, so retry
            // rather than failing the turn when the select finds none.
            //
            // No ON CONFLICT target: a hub carries the single-tenant active
            // index beside the tenant-first one (V011 keeps it for a rolling
            // deploy), and a race that lands on an index the clause did not
            // name is a unique violation, not a no-op. Named by none, every
            // unique index arbitrates; the only other one is the fresh UUID.
            for _ in 0..5 {
                scoped
                    .execute(
                        "INSERT INTO sessions
                         (id, tenant_id, agent_id, channel, owner, label, kind, context_type,
                          created_at, updated_at, total_tokens, message_count, model_override,
                          compaction_history, metadata, messages, sealed, seq, prev_session_id,
                          revision)
                         VALUES ($1, $10, $2, $3, $4, $5, $6, NULL, $7, $8, 0, 0, NULL, '[]'::jsonb,
                                 $9, '[]'::jsonb, FALSE, 0, NULL, 0)
                         ON CONFLICT DO NOTHING",
                        &[
                            &row.id,
                            &self.agent_id,
                            &channel,
                            &owner,
                            &fresh.label,
                            &kind_to_str(&fresh.kind),
                            &fresh.created_at,
                            &fresh.updated_at,
                            &row.metadata,
                            &self.tenant_id(),
                        ],
                    )
                    .await?;
                if let Some(found) = scoped
                    .query_opt(
                        &format!(
                            "SELECT {SESSION_COLUMNS} FROM sessions
                             WHERE agent_id = $1 AND channel = $2 AND owner = $3 AND NOT sealed
                               AND tenant_id = $4
                             ORDER BY seq DESC LIMIT 1"
                        ),
                        &[&self.agent_id, &channel, &owner, &self.tenant_id()],
                    )
                    .await?
                {
                    scoped.finish().await?;
                    return row_to_session(&found);
                }
            }
            eyre::bail!("get_or_create: active session repeatedly vanished under concurrent seals")
        })
    }

    fn save(&self, session: &Session) -> Result<()> {
        self.save_returning_revision(session).map(|_| ())
    }

    fn save_refreshing(&self, session: &mut Session) -> Result<()> {
        let revision = self.save_returning_revision(session)?;
        session.loaded_revision = Some(revision);
        Ok(())
    }

    /// Writes the promoted columns only, and deliberately does not bump
    /// `revision`: nothing else reads or writes them, so they sit outside the
    /// versioned set and bumping would manufacture CAS misses for the very
    /// turn this record belongs to.
    fn begin_turn(
        &self,
        session_id: &str,
        message_id: Option<&str>,
        principal: &TurnPrincipal,
    ) -> Result<String> {
        let id = parse_uuid(session_id)?;
        let token = Uuid::new_v4().to_string();
        let issued = token.clone();
        self.block_on(async move {
            let mut conn = self.pool.get().await?;
            let scoped = self.scope(&mut conn).await?;
            let updated = scoped
                .execute(
                    "UPDATE sessions
                        SET turn_instance = $1, turn_started_at = $2, turn_message_id = $3,
                            turn_token = $4, turn_run_as = $5, turn_bill_to = $6,
                            turn_principal_token = $4
                      WHERE id = $7 AND agent_id = $8 AND tenant_id = $9",
                    &[
                        &self.instance_id,
                        &Utc::now(),
                        &message_id,
                        &issued,
                        &principal.run_as,
                        &principal.bill_to,
                        &id,
                        &self.agent_id,
                        &self.tenant_id(),
                    ],
                )
                .await?;
            scoped.finish().await?;
            if updated == 0 {
                eyre::bail!("Session not found: {session_id}");
            }
            Ok(())
        })?;
        Ok(token)
    }

    fn end_turn(&self, session_id: &str, token: &str) -> Result<bool> {
        let id = parse_uuid(session_id)?;
        self.block_on(async move {
            let mut conn = self.pool.get().await?;
            let scoped = self.scope(&mut conn).await?;
            let ended = scoped
                .execute(
                    "UPDATE sessions
                        SET turn_instance = NULL, turn_started_at = NULL, turn_message_id = NULL,
                            turn_token = NULL, turn_run_as = NULL, turn_bill_to = NULL,
                            turn_principal_token = NULL
                      WHERE id = $1 AND agent_id = $2 AND turn_token = $3 AND tenant_id = $4",
                    &[&id, &self.agent_id, &token, &self.tenant_id()],
                )
                .await?;
            scoped.finish().await?;
            Ok(ended > 0)
        })
    }

    fn journal_append(&self, session_id: &str, entry: &JournalEntry) -> Result<()> {
        let body = serde_json::to_value(entry)?;
        self.block_on(async move {
            let mut conn = self.pool.get().await?;
            let scoped = self.scope(&mut conn).await?;
            scoped
                .execute(
                    "INSERT INTO turn_journal
                     (tenant_id, agent_id, session_id, call_id, entry_key, entry)
                     VALUES ($6, $1, $2, $3, $4, $5)
                     ON CONFLICT DO NOTHING",
                    &[
                        &self.agent_id,
                        &session_id,
                        &entry.call,
                        &entry.key(),
                        &body,
                        &self.tenant_id(),
                    ],
                )
                .await?;
            scoped.finish().await
        })
    }

    fn journal(&self, session_id: &str) -> Result<Vec<JournalEntry>> {
        self.block_on(async move {
            let mut conn = self.pool.get().await?;
            let scoped = self.scope(&mut conn).await?;
            let rows = scoped
                .query(
                    "SELECT entry FROM turn_journal
                      WHERE agent_id = $1 AND session_id = $2 AND tenant_id = $3
                      ORDER BY seq",
                    &[&self.agent_id, &session_id, &self.tenant_id()],
                )
                .await?;
            scoped.finish().await?;
            Ok(rows
                .iter()
                .filter_map(|row| {
                    serde_json::from_value(row.get(0))
                        .map_err(|e| {
                            tracing::warn!(session_id, error = %e, "Skipping an unreadable journal entry")
                        })
                        .ok()
                })
                .collect())
        })
    }

    fn journal_clear(&self, session_id: &str) -> Result<()> {
        self.block_on(async move {
            let mut conn = self.pool.get().await?;
            let scoped = self.scope(&mut conn).await?;
            scoped
                .execute(
                    "DELETE FROM turn_journal
                      WHERE agent_id = $1 AND session_id = $2 AND tenant_id = $3",
                    &[&self.agent_id, &session_id, &self.tenant_id()],
                )
                .await?;
            scoped.finish().await
        })
    }

    fn journals(&self, prefix: &str) -> Result<Vec<String>> {
        self.block_on(async move {
            let mut conn = self.pool.get().await?;
            let scoped = self.scope(&mut conn).await?;
            let rows = scoped
                .query(
                    "SELECT DISTINCT session_id FROM turn_journal
                      WHERE agent_id = $1 AND starts_with(session_id, $2) AND tenant_id = $3",
                    &[&self.agent_id, &prefix, &self.tenant_id()],
                )
                .await?;
            scoped.finish().await?;
            Ok(rows.iter().map(|row| row.get(0)).collect())
        })
    }

    /// Served by the `sessions_turn_active` partial index. `RESUME_SCAN_LIMIT`
    /// bounds a sweep that finds an unexpected backlog; oldest first, so a
    /// bounded pass still drains it.
    /// One statement, no `revision` bump — see the trait doc for the
    /// end-of-turn precondition that makes both sound.
    fn merge_metadata(
        &self,
        session_id: &str,
        patch: serde_json::Map<String, serde_json::Value>,
        remove: &[&str],
    ) -> Result<()> {
        if patch.is_empty() && remove.is_empty() {
            return Ok(());
        }
        let id = parse_uuid(session_id)?;
        let patch = serde_json::Value::Object(patch);
        let remove: Vec<&str> = remove.to_vec();
        self.block_on(async move {
            let mut conn = self.pool.get().await?;
            let scoped = self.scope(&mut conn).await?;
            scoped
                .execute(
                    "UPDATE sessions
                    SET metadata = (CASE WHEN jsonb_typeof(metadata) = 'object'
                                         THEN metadata ELSE '{}'::jsonb END - $1::text[])
                                   || $2::jsonb
                  WHERE id = $3 AND agent_id = $4 AND tenant_id = $5",
                    &[&remove, &patch, &id, &self.agent_id, &self.tenant_id()],
                )
                .await?;
            scoped.finish().await
        })
    }

    fn interrupted_turns(&self) -> Result<Vec<InterruptedTurn>> {
        self.block_on(async move {
            let mut conn = self.pool.get().await?;
            let scoped = self.scope(&mut conn).await?;
            let rows = scoped
                .query(
                    "SELECT id, turn_started_at, turn_message_id, coalesce(turn_token, ''),
                            CASE WHEN turn_principal_token = turn_token THEN turn_run_as END,
                            CASE WHEN turn_principal_token = turn_token THEN turn_bill_to END
                       FROM sessions
                      WHERE agent_id = $1
                        AND turn_instance IS NOT NULL
                        AND turn_instance <> $2
                        AND tenant_id = $4
                      ORDER BY turn_started_at ASC NULLS LAST
                      LIMIT $3",
                    &[
                        &self.agent_id,
                        &self.instance_id,
                        &RESUME_SCAN_LIMIT,
                        &self.tenant_id(),
                    ],
                )
                .await?;
            scoped.finish().await?;
            Ok(rows
                .iter()
                .map(|r| InterruptedTurn {
                    session_id: r.get::<_, Uuid>(0).to_string(),
                    started_at: r.get(1),
                    message_id: r.get(2),
                    token: r.get(3),
                    principal: TurnPrincipal {
                        run_as: r.get(4),
                        bill_to: r.get(5),
                    },
                })
                .collect())
        })
    }

    fn try_claim_turn(&self, session_id: &str, claim_id: &str, ttl_secs: u64) -> Result<bool> {
        self.block_on(async move {
            let mut conn = self.pool.get().await?;
            let scoped = self.scope(&mut conn).await?;
            // Steal conditions, in order: the holder is us (renewal or a newer
            // local turn), the claim TTL lapsed, or the holder's heartbeat has
            // gone stale (crashed process — its row outlives it by design). A
            // holder with NO heartbeat row is only stealable via TTL, which
            // keeps mixed-version fleets safe during a rolling deploy.
            //
            // The arbiter is the primary key by name: (agent_id, session_id)
            // on a hub, (tenant_id, agent_id, session_id) once a tenanted
            // deployment's own set has rebuilt it. DO UPDATE needs one
            // arbiter, and it must be the table's only unique index, or a
            // race landing on the other is a unique violation.
            let updated = scoped
                .execute(
                    "INSERT INTO turn_claims
                     (tenant_id, agent_id, session_id, owner_instance, claim_id, expires_at)
                     VALUES ($7, $1, $2, $3, $4, now() + make_interval(secs => $5::double precision))
                     ON CONFLICT ON CONSTRAINT turn_claims_pkey DO UPDATE SET
                         owner_instance = EXCLUDED.owner_instance,
                         claim_id = EXCLUDED.claim_id,
                         expires_at = EXCLUDED.expires_at
                     WHERE turn_claims.owner_instance = EXCLUDED.owner_instance
                        OR turn_claims.expires_at < now()
                        OR EXISTS (
                             SELECT 1 FROM instance_heartbeats h
                             WHERE h.instance_id = turn_claims.owner_instance
                               AND h.last_seen < now() - make_interval(secs => $6::double precision)
                           )",
                    &[
                        &self.agent_id,
                        &session_id,
                        &self.instance_id,
                        &claim_id,
                        &(ttl_secs as f64),
                        &(HEARTBEAT_STALE_SECS as f64),
                        &self.tenant_id(),
                    ],
                )
                .await?;
            scoped.finish().await?;
            Ok(updated == 1)
        })
    }

    fn renew_turn_claim(&self, session_id: &str, claim_id: &str, ttl_secs: u64) -> Result<bool> {
        self.block_on(async move {
            let mut conn = self.pool.get().await?;
            let scoped = self.scope(&mut conn).await?;
            let updated = scoped
                .execute(
                    "UPDATE turn_claims
                     SET expires_at = now() + make_interval(secs => $5::double precision)
                     WHERE agent_id = $1 AND session_id = $2
                       AND claim_id = $3 AND owner_instance = $4 AND tenant_id = $6",
                    &[
                        &self.agent_id,
                        &session_id,
                        &claim_id,
                        &self.instance_id,
                        &(ttl_secs as f64),
                        &self.tenant_id(),
                    ],
                )
                .await?;
            scoped.finish().await?;
            Ok(updated == 1)
        })
    }

    fn release_turn_claim(&self, session_id: &str, claim_id: &str) -> Result<()> {
        self.block_on(async move {
            let mut conn = self.pool.get().await?;
            let scoped = self.scope(&mut conn).await?;
            scoped
                .execute(
                    "DELETE FROM turn_claims
                     WHERE agent_id = $1 AND session_id = $2 AND claim_id = $3 AND tenant_id = $4",
                    &[&self.agent_id, &session_id, &claim_id, &self.tenant_id()],
                )
                .await?;
            scoped.finish().await
        })
    }

    fn turn_claim_holder(&self, session_id: &str) -> Result<Option<(String, u64)>> {
        self.block_on(async move {
            let mut conn = self.pool.get().await?;
            let scoped = self.scope(&mut conn).await?;
            let row = scoped
                .query_opt(
                    "SELECT owner_instance,
                            GREATEST(0, EXTRACT(EPOCH FROM (expires_at - now())))::double precision
                     FROM turn_claims
                     WHERE agent_id = $1 AND session_id = $2 AND expires_at > now()
                       AND tenant_id = $3",
                    &[&self.agent_id, &session_id, &self.tenant_id()],
                )
                .await?;
            scoped.finish().await?;
            Ok(row.map(|r| (r.get::<_, String>(0), r.get::<_, f64>(1) as u64)))
        })
    }

    fn release_owned_turn_claims(&self) -> Result<u64> {
        self.block_on(async move {
            let mut conn = self.pool.get().await?;
            let scoped = self.scope(&mut conn).await?;
            let deleted = scoped
                .execute(
                    "DELETE FROM turn_claims WHERE owner_instance = $1 AND tenant_id = $2",
                    &[&self.instance_id, &self.tenant_id()],
                )
                .await?;
            scoped.finish().await?;
            Ok(deleted)
        })
    }

    fn heartbeat_instance(&self) -> Result<()> {
        self.block_on(async move {
            let conn = self.pool.get().await?;
            conn.execute(
                "INSERT INTO instance_heartbeats (instance_id, last_seen)
                 VALUES ($1, now())
                 ON CONFLICT (instance_id) DO UPDATE SET last_seen = now()",
                &[&self.instance_id],
            )
            .await?;
            conn.execute(
                "DELETE FROM instance_heartbeats WHERE last_seen < now() - interval '1 day'",
                &[],
            )
            .await?;
            Ok(())
        })
    }

    fn seal_and_insert_successor(&self, sealed: &Session, successor: &Session) -> Result<()> {
        let sealed = sealed.clone();
        let successor = successor.clone();
        self.block_on(async move {
            let mut conn = self.pool.get().await?;
            let tx = conn.transaction().await?;
            self.bind_tenant(&tx).await?;
            let sealed_row = SessionRow::from_session(&sealed)?;
            // Seal the predecessor first so the partial-unique index sees no
            // unsealed clash when the successor is inserted in the same tx.
            tx.execute(
                "UPDATE sessions SET sealed = TRUE, metadata = $1, updated_at = $2,
                     revision = revision + 1
                 WHERE id = $3 AND tenant_id = $4",
                &[
                    &sealed_row.metadata,
                    &sealed.updated_at,
                    &sealed_row.id,
                    &self.tenant_id(),
                ],
            )
            .await?;
            let row = SessionRow::from_session(&successor)?;
            tx.execute(
                "INSERT INTO sessions
                 (id, tenant_id, agent_id, channel, owner, label, kind, context_type, created_at,
                  updated_at, total_tokens, message_count, model_override, compaction_history,
                  metadata, messages, sealed, seq, prev_session_id, revision)
                 VALUES ($1, $19, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15,
                         $16, $17, $18, 0)",
                &[
                    &row.id,
                    &self.agent_id,
                    &successor.channel,
                    &successor.owner,
                    &successor.label,
                    &kind_to_str(&successor.kind),
                    &successor.context_type.as_ref().map(context_type_to_str),
                    &successor.created_at,
                    &successor.updated_at,
                    &row.total_tokens,
                    &row.message_count,
                    &successor.model_override,
                    &row.compaction_history,
                    &row.metadata,
                    &row.messages,
                    &row.sealed,
                    &row.seq,
                    &row.prev_session_id,
                    &self.tenant_id(),
                ],
            )
            .await?;
            tx.commit().await?;
            Ok(())
        })
    }

    fn get(&self, session_id: &str) -> Result<Option<Session>> {
        let id = match parse_uuid(session_id) {
            Ok(id) => id,
            Err(_) => return Ok(None),
        };
        self.block_on(async move {
            let mut conn = self.pool.get().await?;
            let scoped = self.scope(&mut conn).await?;
            let row = scoped
                .query_opt(
                    &format!(
                        "SELECT {SESSION_COLUMNS} FROM sessions
                         WHERE id = $1 AND agent_id = $2 AND tenant_id = $3"
                    ),
                    &[&id, &self.agent_id, &self.tenant_id()],
                )
                .await?;
            scoped.finish().await?;
            row.as_ref().map(row_to_session).transpose()
        })
    }

    fn list(&self, channel: Option<&str>, limit: usize) -> Result<Vec<Session>> {
        let channel = channel.map(str::to_string);
        let limit = limit as i64;
        self.block_on(async move {
            let mut conn = self.pool.get().await?;
            let scoped = self.scope(&mut conn).await?;
            let rows = match channel {
                Some(ref ch) => {
                    scoped
                        .query(
                            &format!(
                                "SELECT {SHELL_COLUMNS} FROM sessions
                                 WHERE agent_id = $1 AND channel = $2 AND tenant_id = $4
                                 ORDER BY updated_at DESC LIMIT $3"
                            ),
                            &[&self.agent_id, ch, &limit, &self.tenant_id()],
                        )
                        .await?
                }
                None => {
                    scoped
                        .query(
                            &format!(
                                "SELECT {SHELL_COLUMNS} FROM sessions
                                 WHERE agent_id = $1 AND tenant_id = $3
                                 ORDER BY updated_at DESC LIMIT $2"
                            ),
                            &[&self.agent_id, &limit, &self.tenant_id()],
                        )
                        .await?
                }
            };
            scoped.finish().await?;
            rows.iter().map(row_to_shell).collect()
        })
    }

    fn list_for_owner(&self, owner: &str, limit: usize) -> Result<Vec<Session>> {
        let owner = owner.to_string();
        let limit = limit as i64;
        self.block_on(async move {
            let mut conn = self.pool.get().await?;
            let scoped = self.scope(&mut conn).await?;
            let rows = scoped
                .query(
                    &format!(
                        "SELECT {SHELL_COLUMNS} FROM sessions
                         WHERE agent_id = $1 AND owner = $2 AND tenant_id = $4
                         ORDER BY updated_at DESC LIMIT $3"
                    ),
                    &[&self.agent_id, &owner, &limit, &self.tenant_id()],
                )
                .await?;
            scoped.finish().await?;
            rows.iter().map(row_to_shell).collect()
        })
    }

    fn owners(&self) -> Result<std::collections::BTreeMap<String, usize>> {
        self.block_on(async move {
            let mut conn = self.pool.get().await?;
            let scoped = self.scope(&mut conn).await?;
            let rows = scoped
                .query(
                    "SELECT owner, COUNT(*) FROM sessions
                     WHERE agent_id = $1 AND tenant_id = $2 GROUP BY owner",
                    &[&self.agent_id, &self.tenant_id()],
                )
                .await?;
            scoped.finish().await?;
            Ok(rows
                .iter()
                .map(|row| {
                    (
                        row.get::<_, String>(0),
                        usize::try_from(row.get::<_, i64>(1)).unwrap_or_default(),
                    )
                })
                .collect())
        })
    }

    fn list_since(
        &self,
        agent_id: &str,
        cutoff: DateTime<Utc>,
        limit: usize,
    ) -> Result<Vec<Session>> {
        let agent_id = agent_id.to_string();
        let limit = limit as i64;
        self.block_on(async move {
            let mut conn = self.pool.get().await?;
            let scoped = self.scope(&mut conn).await?;
            let rows = scoped
                .query(
                    &format!(
                        "SELECT {SHELL_COLUMNS} FROM sessions
                         WHERE agent_id = $1 AND updated_at >= $2 AND tenant_id = $4
                         ORDER BY updated_at DESC LIMIT $3"
                    ),
                    &[&agent_id, &cutoff, &limit, &self.tenant_id()],
                )
                .await?;
            scoped.finish().await?;
            rows.iter().map(row_to_shell).collect()
        })
    }

    /// One transaction, so the one-open-session-per-channel index never sees
    /// both owners on a channel. `revision` moves, so a replica saving from an
    /// older copy appends rather than overwriting, and its save never writes
    /// `owner` or `channel` back.
    fn reassign_owner(&self, from: &str, to: &str, tag: &str) -> Result<Vec<Reassigned>> {
        let (from, to) = (from.to_string(), to.to_string());
        let suffix = reassigned_channel("", tag);
        self.block_on(async move {
            let mut conn = self.pool.get().await?;
            let tx = conn.transaction().await?;
            self.bind_tenant(&tx).await?;
            let renamed: std::collections::HashSet<uuid::Uuid> = tx
                .query(
                    "UPDATE sessions a SET channel = a.channel || $4, revision = a.revision + 1
                     WHERE a.agent_id = $1 AND a.owner = $2 AND NOT a.sealed AND a.tenant_id = $5
                       AND EXISTS (SELECT 1 FROM sessions b
                                   WHERE b.agent_id = $1 AND b.owner = $3 AND b.tenant_id = $5
                                     AND b.channel = a.channel AND NOT b.sealed)
                     RETURNING a.id",
                    &[&self.agent_id, &from, &to, &suffix, &self.tenant_id()],
                )
                .await?
                .iter()
                .map(|row| row.get(0))
                .collect();
            let moved = tx
                .query(
                    "UPDATE sessions SET owner = $3, revision = revision + 1
                     WHERE agent_id = $1 AND owner = $2 AND tenant_id = $4
                     RETURNING id, channel",
                    &[&self.agent_id, &from, &to, &self.tenant_id()],
                )
                .await?;
            tx.commit().await?;
            Ok(moved
                .iter()
                .map(|row| {
                    let id: uuid::Uuid = row.get(0);
                    Reassigned {
                        session_id: id.to_string(),
                        channel: row.get(1),
                        renamed: renamed.contains(&id),
                    }
                })
                .collect())
        })
    }

    fn delete(&self, session_id: &str) -> Result<()> {
        let id = parse_uuid(session_id)?;
        self.block_on(async move {
            let mut conn = self.pool.get().await?;
            let scoped = self.scope(&mut conn).await?;
            scoped
                .execute(
                    "DELETE FROM sessions WHERE id = $1 AND agent_id = $2 AND tenant_id = $3",
                    &[&id, &self.agent_id, &self.tenant_id()],
                )
                .await?;
            scoped
                .execute(
                    "DELETE FROM session_archive WHERE session_id = $1 AND tenant_id = $2",
                    &[&id, &self.tenant_id()],
                )
                .await?;
            scoped
                .execute(
                    "DELETE FROM turn_journal
                     WHERE agent_id = $1 AND session_id = $2 AND tenant_id = $3",
                    &[&self.agent_id, &session_id, &self.tenant_id()],
                )
                .await?;
            scoped.finish().await
        })
    }

    fn read_transcript(&self, session_id: &str) -> Result<Vec<TranscriptEntry>> {
        let session = self
            .get(session_id)?
            .ok_or_else(|| eyre::eyre!("Session not found: {session_id}"))?;
        Ok(session
            .messages
            .iter()
            .map(|msg| message_to_transcript_entry(session_id, msg))
            .collect())
    }

    fn export_session(&self, session_id: &str) -> Result<SessionExport> {
        let session = self
            .get(session_id)?
            .ok_or_else(|| eyre::eyre!("Session not found: {session_id}"))?;
        let transcript: Vec<TranscriptEntry> = session
            .messages
            .iter()
            .map(|msg| message_to_transcript_entry(session_id, msg))
            .collect();
        let stats = compute_stats(&transcript, &session);
        Ok(SessionExport {
            id: session.id,
            label: session.label,
            channel: session.channel,
            owner: session.owner,
            agent_id: session.agent_id,
            kind: session.kind,
            created_at: session.created_at,
            updated_at: session.updated_at,
            total_tokens: session.total_tokens,
            model_override: session.model_override,
            transcript,
            stats,
        })
    }

    fn session_stats(&self, session_id: &str) -> Result<SessionStats> {
        let session = self
            .get(session_id)?
            .ok_or_else(|| eyre::eyre!("Session not found: {session_id}"))?;
        let transcript: Vec<TranscriptEntry> = session
            .messages
            .iter()
            .map(|msg| message_to_transcript_entry(session_id, msg))
            .collect();
        Ok(compute_stats(&transcript, &session))
    }

    fn reset_session(&self, session_id: &str) -> Result<()> {
        let id = parse_uuid(session_id)?;
        self.block_on(async move {
            let mut conn = self.pool.get().await?;
            let scoped = self.scope(&mut conn).await?;
            let n = scoped
                .execute(
                    "UPDATE sessions SET messages = '[]'::jsonb, message_count = 0,
                         total_tokens = 0, updated_at = $1, revision = revision + 1
                     WHERE id = $2 AND agent_id = $3 AND tenant_id = $4",
                    &[&Utc::now(), &id, &self.agent_id, &self.tenant_id()],
                )
                .await?;
            scoped.finish().await?;
            if n == 0 {
                eyre::bail!("Session not found: {session_id}");
            }
            Ok(())
        })
    }

    fn set_session_model(&self, session_id: &str, model: Option<String>) -> Result<()> {
        let id = parse_uuid(session_id)?;
        self.block_on(async move {
            let mut conn = self.pool.get().await?;
            let scoped = self.scope(&mut conn).await?;
            let n = scoped
                .execute(
                    "UPDATE sessions SET model_override = $1, updated_at = $2,
                         revision = revision + 1
                     WHERE id = $3 AND agent_id = $4 AND tenant_id = $5",
                    &[&model, &Utc::now(), &id, &self.agent_id, &self.tenant_id()],
                )
                .await?;
            scoped.finish().await?;
            if n == 0 {
                eyre::bail!("Session not found: {session_id}");
            }
            Ok(())
        })
    }

    fn set_session_label(&self, session_id: &str, label: Option<String>) -> Result<()> {
        let id = parse_uuid(session_id)?;
        self.block_on(async move {
            let mut conn = self.pool.get().await?;
            let scoped = self.scope(&mut conn).await?;
            let n = scoped
                .execute(
                    "UPDATE sessions SET label = $1, updated_at = $2, revision = revision + 1
                     WHERE id = $3 AND agent_id = $4 AND tenant_id = $5",
                    &[&label, &Utc::now(), &id, &self.agent_id, &self.tenant_id()],
                )
                .await?;
            scoped.finish().await?;
            if n == 0 {
                eyre::bail!("Session not found: {session_id}");
            }
            Ok(())
        })
    }

    fn get_context_usage(&self, session_id: &str) -> Result<SessionContextUsage> {
        let session = self
            .get(session_id)?
            .ok_or_else(|| eyre::eyre!("Session not found: {session_id}"))?;
        Ok(context_usage_for(&session))
    }

    fn get_session_tool_calls(&self, session_id: &str) -> Result<Vec<ToolCallRecord>> {
        let session = self
            .get(session_id)?
            .ok_or_else(|| eyre::eyre!("Session not found: {session_id}"))?;
        Ok(tool_calls_for(&session))
    }

    fn redact_secrets_in_session(&self, session_id: &str, dry_run: bool) -> Result<RedactionStats> {
        let id = match parse_uuid(session_id) {
            Ok(id) => id,
            Err(_) => return Ok(RedactionStats::default()),
        };
        let session_id_owned = session_id.to_string();
        self.block_on(async move {
            let mut conn = self.pool.get().await?;
            let tx = conn.transaction().await?;
            self.bind_tenant(&tx).await?;
            let Some(cur) = tx
                .query_opt(
                    "SELECT messages, message_count FROM sessions
                     WHERE id = $1 AND agent_id = $2 AND tenant_id = $3
                     FOR UPDATE",
                    &[&id, &self.agent_id, &self.tenant_id()],
                )
                .await?
            else {
                // A missing session yields empty stats so a sweep can race a
                // concurrent deletion harmlessly.
                tx.commit().await?;
                return self
                    .redact_archive_into(&session_id_owned, dry_run, RedactionStats::default())
                    .await;
            };
            let messages_json: serde_json::Value = cur.get(0);
            let mut messages: Vec<SessionMessage> =
                serde_json::from_value(messages_json).context("decode messages for redaction")?;
            let stats = redact_messages_in_place(&mut messages);
            if !dry_run && stats.messages_redacted > 0 {
                let json = serde_json::to_value(&messages).context("encode redacted messages")?;
                tx.execute(
                    "UPDATE sessions SET messages = $1, revision = revision + 1
                     WHERE id = $2 AND tenant_id = $3",
                    &[&json, &id, &self.tenant_id()],
                )
                .await?;
            }
            tx.commit().await?;
            self.redact_archive_into(&session_id_owned, dry_run, stats)
                .await
        })
    }

    fn redact_archive_secrets(&self, session_id: &str, dry_run: bool) -> Result<usize> {
        let id = parse_uuid(session_id)?;
        self.block_on(async move {
            let mut conn = self.pool.get().await?;
            let tx = conn.transaction().await?;
            self.bind_tenant(&tx).await?;
            let rows = tx
                .query(
                    "SELECT seq, content, tool_data FROM session_archive
                     WHERE session_id = $1 AND tenant_id = $2 ORDER BY seq ASC FOR UPDATE",
                    &[&id, &self.tenant_id()],
                )
                .await?;
            let mut redacted = 0usize;
            for row in &rows {
                let seq: i64 = row.get(0);
                let content: String = row.get(1);
                let tool_data: Option<serde_json::Value> = row.get(2);
                let mut msg = SessionMessage {
                    id: String::new(),
                    role: MessageRole::User,
                    content,
                    tool_data,
                    timestamp: Utc::now(),
                    tokens: 0,
                    external_id: None,
                    attachments: None,
                    trust_level: None,
                    actor: None,
                };
                if redact_message_secrets(&mut msg) {
                    redacted += 1;
                    if !dry_run {
                        tx.execute(
                            "UPDATE session_archive SET content = $1, tool_data = $2
                             WHERE session_id = $3 AND seq = $4 AND tenant_id = $5",
                            &[&msg.content, &msg.tool_data, &id, &seq, &self.tenant_id()],
                        )
                        .await?;
                    }
                }
            }
            tx.commit().await?;
            Ok(redacted)
        })
    }

    fn delete_message(&self, session_id: &str, message_id: &str) -> Result<()> {
        let id = parse_uuid(session_id)?;
        let message_id = message_id.to_string();
        self.block_on(async move {
            let mut conn = self.pool.get().await?;
            let tx = conn.transaction().await?;
            self.bind_tenant(&tx).await?;
            let Some(cur) = tx
                .query_opt(
                    "SELECT messages, total_tokens FROM sessions
                     WHERE id = $1 AND agent_id = $2 AND tenant_id = $3
                     FOR UPDATE",
                    &[&id, &self.agent_id, &self.tenant_id()],
                )
                .await?
            else {
                eyre::bail!("Session not found: {session_id}");
            };
            let messages_json: serde_json::Value = cur.get(0);
            let mut messages: Vec<SessionMessage> =
                serde_json::from_value(messages_json).context("decode messages for delete")?;
            let before = messages.len();
            let removed_tokens: u64 = messages
                .iter()
                .filter(|m| m.id == message_id)
                .map(|m| m.tokens as u64)
                .sum();
            messages.retain(|m| m.id != message_id);
            if messages.len() == before {
                eyre::bail!("Message not found: {message_id}");
            }
            let total_tokens: i64 = cur.get(1);
            let new_total = (total_tokens as u64).saturating_sub(removed_tokens) as i64;
            let count = messages.len() as i32;
            let json = serde_json::to_value(&messages).context("encode messages after delete")?;
            tx.execute(
                "UPDATE sessions SET messages = $1, message_count = $2, total_tokens = $3,
                     updated_at = $4, revision = revision + 1
                 WHERE id = $5 AND tenant_id = $6",
                &[
                    &json,
                    &count,
                    &new_total,
                    &Utc::now(),
                    &id,
                    &self.tenant_id(),
                ],
            )
            .await?;
            tx.commit().await?;
            Ok(())
        })
    }

    fn truncate_from_message(&self, session_id: &str, message_id: &str) -> Result<()> {
        let id = parse_uuid(session_id)?;
        let message_id = message_id.to_string();
        self.block_on(async move {
            let mut conn = self.pool.get().await?;
            let tx = conn.transaction().await?;
            self.bind_tenant(&tx).await?;
            let Some(cur) = tx
                .query_opt(
                    "SELECT messages, total_tokens FROM sessions
                     WHERE id = $1 AND agent_id = $2 AND tenant_id = $3
                     FOR UPDATE",
                    &[&id, &self.agent_id, &self.tenant_id()],
                )
                .await?
            else {
                eyre::bail!("Session not found: {session_id}");
            };
            let messages_json: serde_json::Value = cur.get(0);
            let mut messages: Vec<SessionMessage> =
                serde_json::from_value(messages_json).context("decode messages for truncate")?;
            let start_idx = messages
                .iter()
                .position(|m| m.id == message_id)
                .ok_or_else(|| eyre::eyre!("Message not found: {message_id}"))?;
            let removed_tokens: u64 = messages[start_idx..].iter().map(|m| m.tokens as u64).sum();
            messages.truncate(start_idx);
            let total_tokens: i64 = cur.get(1);
            let new_total = (total_tokens as u64).saturating_sub(removed_tokens) as i64;
            let count = messages.len() as i32;
            let json = serde_json::to_value(&messages).context("encode messages after truncate")?;
            tx.execute(
                "UPDATE sessions SET messages = $1, message_count = $2, total_tokens = $3,
                     updated_at = $4, revision = revision + 1
                 WHERE id = $5 AND tenant_id = $6",
                &[
                    &json,
                    &count,
                    &new_total,
                    &Utc::now(),
                    &id,
                    &self.tenant_id(),
                ],
            )
            .await?;
            tx.commit().await?;
            Ok(())
        })
    }

    fn search_sessions(&self, query: &str, limit: usize) -> Result<Vec<SessionSearchResult>> {
        let terms = search_terms(query);
        if terms.is_empty() {
            return Ok(Vec::new());
        }
        let query = query.to_string();
        let fetch = (limit as i64) * 4 + 16;
        self.block_on(async move {
            let mut conn = self.pool.get().await?;
            let scoped = self.scope(&mut conn).await?;
            // FTS over the archive content. `plainto_tsquery` ANDs terms; rank
            // by ts_rank then recency, mirroring the file store's coverage +
            // recency ordering. Over-fetch then re-rank/cap in Rust so the
            // per-result excerpt (surrounding turns) can be built.
            let rows = scoped
                .query(
                    "SELECT a.session_id, a.seq, a.message_id, a.role, a.content, a.ts, s.label,
                            ts_rank(a.content_tsv, plainto_tsquery('english', $2)) AS rank
                     FROM session_archive a
                     JOIN sessions s ON s.id = a.session_id AND s.tenant_id = a.tenant_id
                     WHERE s.agent_id = $1 AND s.tenant_id = $4
                       AND a.content_tsv @@ plainto_tsquery('english', $2)
                     ORDER BY rank DESC, a.ts DESC
                     LIMIT $3",
                    &[&self.agent_id, &query, &fetch, &self.tenant_id()],
                )
                .await?;

            let mut scored: Vec<(usize, SessionSearchResult)> = Vec::new();
            for row in &rows {
                let session_uuid: Uuid = row.get(0);
                let session_id = session_uuid.to_string();
                let content: String = row.get(4);
                let score = coordination_score(&content.to_lowercase(), &terms);
                let role = role_from_str(row.get::<_, &str>(3));
                let excerpt = self
                    .build_excerpt(&scoped, session_uuid, row.get::<_, i64>(1))
                    .await?;
                scored.push((
                    score.max(1),
                    SessionSearchResult {
                        session_id,
                        label: row.get(6),
                        message_id: row.get(2),
                        role,
                        excerpt,
                        timestamp: row.get(5),
                    },
                ));
            }
            scoped.finish().await?;
            Ok(rank_search_results(scored, limit))
        })
    }

    fn append_to_archive(&self, session_id: &str, message: &SessionMessage) -> Result<()> {
        let id = parse_uuid(session_id)?;
        let message = message.clone();
        self.block_on(async move {
            let mut conn = self.pool.get().await?;
            let scoped = self.scope(&mut conn).await?;
            let content = crate::pg_text::strip_nuls_str(&message.content);
            let tool_data = message.tool_data.map(|mut v| {
                crate::pg_text::strip_nuls_json(&mut v);
                v
            });
            // DO NOTHING: a CAS-losing post-turn save can replay an archive
            // append for a message another writer already archived. No
            // target, for the same reason as `get_or_create`: a hub keeps
            // the single-tenant message index beside the tenant-first one,
            // and the primary key's `seq` never conflicts.
            scoped
                .execute(
                    "INSERT INTO session_archive
                     (tenant_id, session_id, message_id, role, content, tool_data, ts, tokens)
                     VALUES ($8, $1, $2, $3, $4, $5, $6, $7)
                     ON CONFLICT DO NOTHING",
                    &[
                        &id,
                        &message.id,
                        &role_to_str(&message.role),
                        &content.as_ref(),
                        &tool_data,
                        &message.timestamp,
                        &(message.tokens as i32),
                        &self.tenant_id(),
                    ],
                )
                .await?;
            scoped.finish().await
        })
    }

    fn read_archive_page(
        &self,
        session_id: &str,
        before_id: Option<&str>,
        limit: usize,
    ) -> Result<ArchivePage> {
        let id = parse_uuid(session_id)?;
        let before_id = before_id.map(str::to_string);
        let session_id_owned = session_id.to_string();
        self.block_on(async move {
            let mut conn = self.pool.get().await?;
            let scoped = self.scope(&mut conn).await?;
            // Has the archive been seeded for this session? Empty archive but a
            // populated live transcript means a session that predates archive
            // seeding — fall back to the live messages (mirrors the file store).
            let archived: i64 = scoped
                .query_one(
                    "SELECT COUNT(*)::BIGINT FROM session_archive
                     WHERE session_id = $1 AND tenant_id = $2",
                    &[&id, &self.tenant_id()],
                )
                .await?
                .get(0);
            if archived == 0 {
                let row = scoped
                    .query_opt(
                        &format!(
                            "SELECT {SESSION_COLUMNS} FROM sessions
                             WHERE id = $1 AND agent_id = $2 AND tenant_id = $3"
                        ),
                        &[&id, &self.agent_id, &self.tenant_id()],
                    )
                    .await?;
                scoped.finish().await?;
                let entries: Vec<TranscriptEntry> = match row {
                    Some(ref r) => row_to_session(r)?
                        .messages
                        .iter()
                        .map(|m| message_to_transcript_entry(&session_id_owned, m))
                        .collect(),
                    None => Vec::new(),
                };
                return paginate_entries(entries, before_id.as_deref(), limit);
            }

            // Resolve the cursor's archive seq (entries are returned in
            // chronological order, so "before_id" means strictly-lower seq).
            let cursor_seq: Option<i64> = match before_id.as_deref() {
                Some(mid) => scoped
                    .query_opt(
                        "SELECT seq FROM session_archive
                         WHERE session_id = $1 AND message_id = $2 AND tenant_id = $3
                         ORDER BY seq ASC LIMIT 1",
                        &[&id, &mid, &self.tenant_id()],
                    )
                    .await?
                    .map(|r| r.get::<_, i64>(0)),
                None => None,
            };

            let limit_i = limit as i64;
            // Fetch the newest `limit` rows below the cursor, then reverse to
            // chronological. `has_more` is whether an older row remains.
            let rows = match cursor_seq {
                Some(seq) => {
                    scoped
                        .query(
                            "SELECT seq, message_id, role, content, tool_data, ts, tokens
                             FROM session_archive
                             WHERE session_id = $1 AND seq < $2 AND tenant_id = $4
                             ORDER BY seq DESC LIMIT $3",
                            &[&id, &seq, &(limit_i + 1), &self.tenant_id()],
                        )
                        .await?
                }
                None => {
                    scoped
                        .query(
                            "SELECT seq, message_id, role, content, tool_data, ts, tokens
                             FROM session_archive
                             WHERE session_id = $1 AND tenant_id = $3
                             ORDER BY seq DESC LIMIT $2",
                            &[&id, &(limit_i + 1), &self.tenant_id()],
                        )
                        .await?
                }
            };
            scoped.finish().await?;
            let has_more = rows.len() as i64 > limit_i;
            let take = rows.len().min(limit);
            let mut entries: Vec<TranscriptEntry> = rows
                .iter()
                .take(take)
                .map(|r| archive_row_to_entry(&session_id_owned, r))
                .collect();
            entries.reverse();
            Ok(ArchivePage {
                messages: entries,
                has_more,
            })
        })
    }

    fn read_archive_message(
        &self,
        session_id: &str,
        message_id: &str,
    ) -> Result<Option<TranscriptEntry>> {
        let Ok(id) = parse_uuid(session_id) else {
            return Ok(None);
        };
        let message_id = message_id.to_string();
        self.block_on(async move {
            let mut conn = self.pool.get().await?;
            let scoped = self.scope(&mut conn).await?;
            let row = scoped
                .query_opt(
                    "SELECT a.seq, a.message_id, a.role, a.content, a.tool_data, a.ts, a.tokens
                     FROM session_archive a
                     JOIN sessions s ON s.id = a.session_id AND s.tenant_id = a.tenant_id
                     WHERE a.session_id = $1 AND a.message_id = $2
                       AND s.agent_id = $3 AND a.tenant_id = $4",
                    &[&id, &message_id, &self.agent_id, &self.tenant_id()],
                )
                .await?;
            scoped.finish().await?;
            Ok(row.map(|r| archive_row_to_entry(&id.to_string(), &r)))
        })
    }
}

impl PostgresSessionStore {
    /// The shared body of `save`/`save_refreshing`: persist the session and
    /// return the row's post-write revision so callers can refresh their CAS
    /// baseline without a follow-up read.
    fn save_returning_revision(&self, session: &Session) -> Result<i64> {
        let session = session.clone();
        self.block_on(async move {
            let mut conn = self.pool.get().await?;
            let tx = conn.transaction().await?;
            self.bind_tenant(&tx).await?;
            let row = SessionRow::from_session(&session)?;

            // Lock the existing row (if any) and read the columns the merge
            // needs, so the append-preserving resolution is atomic.
            let existing = tx
                .query_opt(
                    "SELECT revision, messages, message_count, total_tokens, sealed, seq, \
                     prev_session_id, metadata, compaction_history \
                     FROM sessions WHERE id = $1 AND tenant_id = $2 FOR UPDATE",
                    &[&row.id, &self.tenant_id()],
                )
                .await?;

            // A shell (from list/find: empty messages but a non-None
            // cached_message_count) must never overwrite the stored transcript
            // — mirrors JsonlSessionStore::write_to_disk's shell guard.
            let is_shell = session.cached_message_count.is_some();

            // Resolve the contested columns. Uncontended (revision matches)
            // applies the incoming row exactly so compaction (which reduces
            // messages) and edits land. A shell or a CAS-miss preserves the
            // stored structural columns (sealed/seq/prev_session_id/metadata/
            // compaction_history) and recomputes count/tokens from the
            // effective message set. Scalar fields (label/model_override)
            // always take the incoming value so renames land from shells.
            let incoming = || {
                (
                    row.messages.clone(),
                    row.message_count,
                    row.total_tokens,
                    row.sealed,
                    row.seq,
                    row.prev_session_id,
                    row.metadata.clone(),
                    row.compaction_history.clone(),
                )
            };
            let (
                messages,
                count,
                total_tokens,
                sealed,
                seq,
                prev_session_id,
                metadata,
                compaction_history,
            ) = match existing.as_ref() {
                None => incoming(),
                Some(cur) if is_shell => {
                    warn_discarded_metadata(&session, &row.metadata, cur, "shell save");
                    (
                        cur.get(1),
                        cur.get(2),
                        cur.get(3),
                        cur.get(4),
                        cur.get(5),
                        cur.get(6),
                        cur.get(7),
                        cur.get(8),
                    )
                }
                Some(cur) if session.loaded_revision == Some(cur.get::<_, i64>(0)) => incoming(),
                Some(cur) => {
                    warn_discarded_metadata(&session, &row.metadata, cur, "stale-baseline save");
                    let stored: Vec<SessionMessage> =
                        serde_json::from_value(cur.get(1)).context("decode stored messages")?;
                    let known: std::collections::HashSet<&str> =
                        stored.iter().map(|m| m.id.as_str()).collect();
                    // New ids land at the end, so a summary from a
                    // CAS-losing compaction trails recent turns until the
                    // next uncontended compaction; no message is ever lost.
                    let mut merged = stored.clone();
                    merged.extend(
                        session
                            .messages
                            .iter()
                            .filter(|m| !known.contains(m.id.as_str()))
                            .cloned(),
                    );
                    let total: i64 = merged.iter().map(|m| i64::from(m.tokens)).sum();
                    let count = merged.len() as i32;
                    let json = serde_json::to_value(&merged).context("encode merged messages")?;
                    (
                        json,
                        count,
                        total,
                        cur.get(4),
                        cur.get(5),
                        cur.get(6),
                        cur.get(7),
                        cur.get(8),
                    )
                }
            };

            let saved = tx
                .query_one(
                    "INSERT INTO sessions
                 (id, tenant_id, agent_id, channel, owner, label, kind, context_type, created_at,
                  updated_at, total_tokens, message_count, model_override, compaction_history,
                  metadata, messages, sealed, seq, prev_session_id, revision)
                 VALUES ($1, $19, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15,
                         $16, $17, $18, 0)
                 ON CONFLICT (id) DO UPDATE SET
                     label = EXCLUDED.label,
                     kind = EXCLUDED.kind,
                     context_type = EXCLUDED.context_type,
                     updated_at = EXCLUDED.updated_at,
                     total_tokens = EXCLUDED.total_tokens,
                     message_count = EXCLUDED.message_count,
                     model_override = EXCLUDED.model_override,
                     compaction_history = EXCLUDED.compaction_history,
                     metadata = EXCLUDED.metadata,
                     messages = EXCLUDED.messages,
                     sealed = EXCLUDED.sealed,
                     seq = EXCLUDED.seq,
                     prev_session_id = EXCLUDED.prev_session_id,
                     revision = sessions.revision + 1
                 RETURNING revision",
                    &[
                        &row.id,
                        &self.agent_id,
                        &session.channel,
                        &session.owner,
                        &session.label,
                        &kind_to_str(&session.kind),
                        &session.context_type.as_ref().map(context_type_to_str),
                        &session.created_at,
                        &session.updated_at,
                        &total_tokens,
                        &count,
                        &session.model_override,
                        &compaction_history,
                        &metadata,
                        &messages,
                        &sealed,
                        &seq,
                        &prev_session_id,
                        &self.tenant_id(),
                    ],
                )
                .await?;
            let revision: i64 = saved.get(0);
            tx.commit().await?;
            Ok(revision)
        })
    }

    /// Redact the archive and fold its count into the live-message stats.
    /// Split out so `redact_secrets_in_session` (already inside `block_on`)
    /// can chain it without nesting another runtime call.
    async fn redact_archive_into(
        &self,
        session_id: &str,
        dry_run: bool,
        mut stats: RedactionStats,
    ) -> Result<RedactionStats> {
        let id = parse_uuid(session_id)?;
        let mut conn = self.pool.get().await?;
        let tx = conn.transaction().await?;
        self.bind_tenant(&tx).await?;
        let rows = tx
            .query(
                "SELECT seq, content, tool_data FROM session_archive
                 WHERE session_id = $1 AND tenant_id = $2 ORDER BY seq ASC FOR UPDATE",
                &[&id, &self.tenant_id()],
            )
            .await?;
        let mut redacted = 0usize;
        for row in &rows {
            let seq: i64 = row.get(0);
            let content: String = row.get(1);
            let tool_data: Option<serde_json::Value> = row.get(2);
            let mut msg = SessionMessage {
                id: String::new(),
                role: MessageRole::User,
                content,
                tool_data,
                timestamp: Utc::now(),
                tokens: 0,
                external_id: None,
                attachments: None,
                trust_level: None,
                actor: None,
            };
            if redact_message_secrets(&mut msg) {
                redacted += 1;
                if !dry_run {
                    tx.execute(
                        "UPDATE session_archive SET content = $1, tool_data = $2
                         WHERE session_id = $3 AND seq = $4 AND tenant_id = $5",
                        &[&msg.content, &msg.tool_data, &id, &seq, &self.tenant_id()],
                    )
                    .await?;
                }
            }
        }
        tx.commit().await?;
        stats.archive_messages_redacted = redacted;
        Ok(stats)
    }

    /// Build a search excerpt: the matched archive row plus the ±2 turns
    /// around it *within the session*, mirroring the file store's
    /// surrounding-context excerpt. `seq` is a global `BIGSERIAL`, so a
    /// session's rows are monotonic but NOT contiguous once sessions
    /// interleave; window by per-session position (`row_number`), not by raw
    /// `seq` arithmetic, or the excerpt collapses to just the hit line.
    async fn build_excerpt(
        &self,
        scoped: &Scoped<'_>,
        session_id: Uuid,
        hit_seq: i64,
    ) -> Result<String> {
        const RADIUS: i64 = 2;
        let rows = scoped
            .query(
                "WITH ordered AS (
                     SELECT seq, role, content,
                            row_number() OVER (ORDER BY seq) AS rn
                     FROM session_archive WHERE session_id = $1 AND tenant_id = $4
                 ),
                 hit AS (SELECT rn FROM ordered WHERE seq = $2)
                 SELECT o.seq, o.role, o.content
                 FROM ordered o, hit
                 WHERE o.rn BETWEEN hit.rn - $3 AND hit.rn + $3
                 ORDER BY o.seq ASC",
                &[&session_id, &hit_seq, &RADIUS, &self.tenant_id()],
            )
            .await?;
        Ok(format_excerpt_rows(rows.iter().map(|r| {
            (
                r.get::<_, i64>(0) == hit_seq,
                r.get::<_, String>(1),
                r.get::<_, String>(2),
            )
        })))
    }
}

fn role_from_str(s: &str) -> MessageRole {
    match s {
        "assistant" => MessageRole::Assistant,
        "system" => MessageRole::System,
        "tool" => MessageRole::Tool,
        _ => MessageRole::User,
    }
}

fn archive_row_to_entry(session_id: &str, r: &tokio_postgres::Row) -> TranscriptEntry {
    let msg = SessionMessage {
        id: r.get(1),
        role: role_from_str(r.get::<_, &str>(2)),
        content: r.get(3),
        tool_data: r.get(4),
        timestamp: r.get(5),
        tokens: r.get::<_, i32>(6) as u32,
        external_id: None,
        attachments: None,
        trust_level: None,
        actor: None,
    };
    message_to_transcript_entry(session_id, &msg)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use workforce_postgres::test_support::{now_micros, with_test_pool};

    pub(crate) async fn with_embedded_pg_pool<F, Fut>(test: F)
    where
        F: FnOnce(workforce_postgres::PostgresPool) -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        with_test_pool(PostgresSessionStore::build_pool, test).await;
    }

    async fn with_store<F, Fut>(test: F)
    where
        F: FnOnce(Arc<PostgresSessionStore>) -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        with_embedded_pg_pool(|pool| async move {
            let store = PostgresSessionStore::with_pool(pool, "agent-a")
                .await
                .expect("construct store (runs migrations)");
            test(store).await;
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn reassigning_owners_keeps_both_on_a_shared_channel_in_one_transaction() {
        with_store(|store| async move {
            crate::session::assert_reassign_owner_contract(store.as_ref());
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn sessions_are_counted_per_owner() {
        with_store(|store| async move {
            crate::session::assert_owners_contract(store.as_ref());
        })
        .await;
    }

    fn user_msg(content: &str) -> SessionMessage {
        SessionMessage {
            id: Uuid::new_v4().to_string(),
            role: MessageRole::User,
            content: content.to_string(),
            tool_data: None,
            timestamp: now_micros(),
            tokens: crate::session::estimate_tokens(content),
            external_id: None,
            attachments: None,
            trust_level: None,
            actor: None,
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn create_find_get_round_trip() {
        with_store(|store| async move {
            let s: Arc<dyn SessionStore> = store;
            assert!(s.find("slack:C1", "u1").unwrap().is_none());

            let created = s.get_or_create("slack:C1", "u1", "agent-a").unwrap();
            assert_eq!(created.channel, "slack:C1");
            assert_eq!(created.owner, "u1");
            assert_eq!(created.label.as_deref(), Some("slack:C1:u1"));
            assert!(created.loaded_revision.is_some());

            let found = s.find("slack:C1", "u1").unwrap().expect("active session");
            assert_eq!(found.id, created.id);
            // find returns a shell: empty messages, loaded_revision set.
            assert!(found.messages.is_empty());
            assert!(found.loaded_revision.is_some());

            let got = s.get(&created.id).unwrap().expect("get by id");
            assert_eq!(got.id, created.id);
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn save_and_read_transcript() {
        with_store(|store| async move {
            let s: Arc<dyn SessionStore> = store;
            let mut sess = s.get_or_create("cli", "u", "agent-a").unwrap();
            sess.messages.push(user_msg("hello there"));
            sess.messages.push(user_msg("second turn"));
            sess.total_tokens = sess.estimated_total_tokens() as u64;
            s.save(&sess).unwrap();

            let got = s.get(&sess.id).unwrap().expect("session");
            assert_eq!(got.messages.len(), 2);
            assert_eq!(got.messages[0].content, "hello there");

            let transcript = s.read_transcript(&sess.id).unwrap();
            assert_eq!(transcript.len(), 2);

            let stats = s.session_stats(&sess.id).unwrap();
            assert_eq!(stats.message_count, 2);

            let export = s.export_session(&sess.id).unwrap();
            assert_eq!(export.id, sess.id);
            assert_eq!(export.channel, "cli");
            assert_eq!(export.transcript.len(), 2);
            assert_eq!(export.stats.message_count, 2);
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn list_returns_shells_with_counts() {
        with_store(|store| async move {
            let s: Arc<dyn SessionStore> = store;
            let mut a = s.get_or_create("slack:A", "u", "agent-a").unwrap();
            a.messages.push(user_msg("one"));
            a.messages.push(user_msg("two"));
            s.save(&a).unwrap();

            let listed = s.list(None, 10).unwrap();
            assert_eq!(listed.len(), 1);
            assert!(listed[0].messages.is_empty(), "list returns shells");
            assert_eq!(listed[0].message_count(), 2);
            assert!(listed[0].loaded_revision.is_some());

            let by_channel = s.list(Some("slack:A"), 10).unwrap();
            assert_eq!(by_channel.len(), 1);
            assert!(s.list(Some("nope"), 10).unwrap().is_empty());
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn list_since_filters_by_cutoff() {
        with_store(|store| async move {
            let s: Arc<dyn SessionStore> = store;
            let old = s.get_or_create("slack:A", "u", "agent-a").unwrap();
            // Force an old updated_at by saving with a backdated timestamp.
            let mut old = old;
            old.updated_at = now_micros() - chrono::Duration::days(30);
            s.save(&old).unwrap();

            let cutoff = now_micros() - chrono::Duration::days(1);
            assert!(s.list_since("agent-a", cutoff, 10).unwrap().is_empty());

            let mut fresh = s.get_or_create("slack:B", "u", "agent-a").unwrap();
            fresh.updated_at = now_micros();
            s.save(&fresh).unwrap();
            let recent = s.list_since("agent-a", cutoff, 10).unwrap();
            assert_eq!(recent.len(), 1);
            assert_eq!(recent[0].channel, "slack:B");
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn model_and_label_overrides() {
        with_store(|store| async move {
            let s: Arc<dyn SessionStore> = store;
            let sess = s.get_or_create("cli", "u", "agent-a").unwrap();
            s.set_session_model(&sess.id, Some("claude-opus-4-8".into()))
                .unwrap();
            s.set_session_label(&sess.id, Some("renamed".into()))
                .unwrap();
            let got = s.get(&sess.id).unwrap().unwrap();
            assert_eq!(got.model_override.as_deref(), Some("claude-opus-4-8"));
            assert_eq!(got.label.as_deref(), Some("renamed"));

            s.set_session_model(&sess.id, None).unwrap();
            assert!(s.get(&sess.id).unwrap().unwrap().model_override.is_none());
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn reset_clears_messages_keeps_session() {
        with_store(|store| async move {
            let s: Arc<dyn SessionStore> = store;
            let mut sess = s.get_or_create("cli", "u", "agent-a").unwrap();
            sess.messages.push(user_msg("hi"));
            sess.total_tokens = 5;
            s.save(&sess).unwrap();
            s.reset_session(&sess.id).unwrap();
            let got = s.get(&sess.id).unwrap().unwrap();
            assert!(got.messages.is_empty());
            assert_eq!(got.total_tokens, 0);
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn delete_message_and_truncate() {
        with_store(|store| async move {
            let s: Arc<dyn SessionStore> = store;
            let mut sess = s.get_or_create("cli", "u", "agent-a").unwrap();
            let m0 = user_msg("a");
            let m1 = user_msg("b");
            let m2 = user_msg("c");
            let (id0, id1, id2) = (m0.id.clone(), m1.id.clone(), m2.id.clone());
            sess.messages = vec![m0, m1, m2];
            sess.total_tokens = sess.estimated_total_tokens() as u64;
            s.save(&sess).unwrap();

            s.delete_message(&sess.id, &id1).unwrap();
            let after = s.get(&sess.id).unwrap().unwrap();
            assert_eq!(after.messages.len(), 2);
            assert!(after.messages.iter().all(|m| m.id != id1));
            assert!(s.delete_message(&sess.id, "missing").is_err());

            s.truncate_from_message(&sess.id, &id2).unwrap();
            let after = s.get(&sess.id).unwrap().unwrap();
            assert_eq!(after.messages.len(), 1);
            assert_eq!(after.messages[0].id, id0);
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn archive_paging_orders_chronologically() {
        with_store(|store| async move {
            let s: Arc<dyn SessionStore> = store;
            let sess = s.get_or_create("cli", "u", "agent-a").unwrap();
            let mut ids = Vec::new();
            for i in 0..5 {
                let m = user_msg(&format!("turn {i}"));
                ids.push(m.id.clone());
                s.append_to_archive(&sess.id, &m).unwrap();
            }
            let page = s.read_archive_page(&sess.id, None, 2).unwrap();
            assert_eq!(page.messages.len(), 2);
            assert!(page.has_more);
            // Newest page, chronological within the page.
            assert_eq!(page.messages[0].content, "turn 3");
            assert_eq!(page.messages[1].content, "turn 4");

            let older = s.read_archive_page(&sess.id, Some(&ids[3]), 2).unwrap();
            assert_eq!(older.messages.len(), 2);
            assert_eq!(older.messages[0].content, "turn 1");
            assert_eq!(older.messages[1].content, "turn 2");
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn thread_paging_walks_prev_session() {
        with_store(|store| async move {
            let s: Arc<dyn SessionStore> = store;
            let first = s.get_or_create("cli", "u", "agent-a").unwrap();
            for i in 0..3 {
                s.append_to_archive(&first.id, &user_msg(&format!("old {i}")))
                    .unwrap();
            }
            // Mint a successor linked via prev_session_id.
            let mut succ = Session::new("cli", "u", "agent-a");
            succ.set_seq(1);
            succ.set_prev_session_id(&first.id);
            s.seal_and_insert_successor(
                &{
                    let mut sealed = s.get(&first.id).unwrap().unwrap();
                    sealed.seal();
                    sealed
                },
                &succ,
            )
            .unwrap();
            for i in 0..3 {
                s.append_to_archive(&succ.id, &user_msg(&format!("new {i}")))
                    .unwrap();
            }

            let page = s.read_thread_page(&succ.id, None, None, 10).unwrap();
            let contents: Vec<&str> = page.messages.iter().map(|m| m.content.as_str()).collect();
            assert_eq!(
                contents,
                vec!["old 0", "old 1", "old 2", "new 0", "new 1", "new 2"]
            );
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn search_finds_archived_content() {
        with_store(|store| async move {
            let s: Arc<dyn SessionStore> = store;
            let sess = s.get_or_create("cli", "u", "agent-a").unwrap();
            s.append_to_archive(&sess.id, &user_msg("the quick brown fox"))
                .unwrap();
            s.append_to_archive(&sess.id, &user_msg("unrelated chatter"))
                .unwrap();

            let hits = s.search_sessions("brown fox", 10).unwrap();
            assert_eq!(hits.len(), 1);
            assert_eq!(hits[0].session_id, sess.id);
            assert!(hits[0].excerpt.contains("quick brown fox"));

            assert!(s.search_sessions("nonexistentterm", 10).unwrap().is_empty());
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn search_finds_a_sent_message_through_the_generated_tsvector_and_reads_it_back() {
        with_store(|store| async move {
            let s: Arc<dyn SessionStore> = store;
            let sess = s.get_or_create("slack:D1", "u", "agent-a").unwrap();
            let lyrics = format!("{} marmalade skies", "verse ".repeat(2000));
            let sent = SessionMessage::tool(
                "messaging.send",
                &serde_json::json!({ "channel": "D1", "text": lyrics }),
                &serde_json::json!({ "ok": true }),
                true,
                12,
                None,
            );
            s.append_to_archive(&sess.id, &user_msg("send Sam the song"))
                .unwrap();
            s.append_to_archive(&sess.id, &sent).unwrap();

            let hits = s.search_sessions("marmalade", 10).unwrap();
            assert_eq!(hits.len(), 1, "{hits:?}");
            assert_eq!(hits[0].message_id, sent.id);
            assert_eq!(hits[0].role, MessageRole::Tool);
            assert!(hits[0]
                .excerpt
                .contains("» [tool] Tool 'messaging.send' completed in 12ms"));

            let read = s
                .read_archive_message(&hits[0].session_id, &hits[0].message_id)
                .unwrap()
                .expect("a hit reads back");
            assert_eq!(read.content, sent.content);
            assert!(read.content.ends_with("marmalade skies"));
            assert_eq!(read.session_id, sess.id);
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn read_archive_message_is_scoped_to_the_agent_and_known_ids() {
        with_embedded_pg_pool(|pool| async move {
            let a = PostgresSessionStore::with_pool(pool.clone(), "agent-a")
                .await
                .unwrap();
            let b = PostgresSessionStore::with_shared_pool(pool, "agent-b");
            let sa: Arc<dyn SessionStore> = a;
            let sb: Arc<dyn SessionStore> = b;
            let sess = sa.get_or_create("cli", "u", "agent-a").unwrap();
            let msg = user_msg("archived words");
            sa.append_to_archive(&sess.id, &msg).unwrap();

            assert!(sa
                .read_archive_message(&sess.id, &msg.id)
                .unwrap()
                .is_some());
            assert!(sb
                .read_archive_message(&sess.id, &msg.id)
                .unwrap()
                .is_none());
            assert!(sa.read_archive_message(&sess.id, "nope").unwrap().is_none());
            assert!(sa
                .read_archive_message("not-a-uuid", &msg.id)
                .unwrap()
                .is_none());
            assert!(sa
                .read_archive_message(&Uuid::new_v4().to_string(), &msg.id)
                .unwrap()
                .is_none());
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn search_excerpt_spans_neighbours_across_interleaved_sessions() {
        // `seq` is a global BIGSERIAL: appending to two sessions alternately
        // makes each session's rows monotonic but non-contiguous. The excerpt
        // must still include the ±neighbour turns within the session, not just
        // the hit line — windowing by raw seq would collapse it.
        with_store(|store| async move {
            let s: Arc<dyn SessionStore> = store;
            let a = s.get_or_create("cli", "ua", "agent-a").unwrap();
            let b = s.get_or_create("cli", "ub", "agent-a").unwrap();
            // Pad each gap with several b-rows so a's three turns land at raw
            // seqs far more than RADIUS (2) apart — e.g. 1, 6, 11. A
            // `seq BETWEEN hit-2 AND hit+2` window would then return only the
            // hit, so this guards the per-session-position windowing.
            s.append_to_archive(&a.id, &user_msg("alpha before"))
                .unwrap();
            for i in 0..4 {
                s.append_to_archive(&b.id, &user_msg(&format!("noise {i}")))
                    .unwrap();
            }
            s.append_to_archive(&a.id, &user_msg("alpha needle here"))
                .unwrap();
            for i in 4..8 {
                s.append_to_archive(&b.id, &user_msg(&format!("noise {i}")))
                    .unwrap();
            }
            s.append_to_archive(&a.id, &user_msg("alpha after"))
                .unwrap();

            let hits = s.search_sessions("needle", 10).unwrap();
            assert_eq!(hits.len(), 1);
            assert_eq!(hits[0].session_id, a.id);
            // The neighbour turns from the SAME session must appear.
            assert!(
                hits[0].excerpt.contains("alpha before"),
                "preceding turn missing: {}",
                hits[0].excerpt
            );
            assert!(
                hits[0].excerpt.contains("alpha after"),
                "following turn missing: {}",
                hits[0].excerpt
            );
            // ...and the interleaved other session's turns must NOT leak in.
            assert!(!hits[0].excerpt.contains("noise"));
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn redaction_scrubs_live_and_archive() {
        with_store(|store| async move {
            let s: Arc<dyn SessionStore> = store;
            let mut sess = s.get_or_create("cli", "u", "agent-a").unwrap();
            let secret = "AKIAIOSFODNN7EXAMPLE";
            sess.messages.push(user_msg(&format!("token is {secret}")));
            s.save(&sess).unwrap();
            s.append_to_archive(&sess.id, &user_msg(&format!("archived {secret}")))
                .unwrap();

            let dry = s.redact_secrets_in_session(&sess.id, true).unwrap();
            assert!(dry.changed());
            // Dry run leaves the secret in place.
            assert!(s.get(&sess.id).unwrap().unwrap().messages[0]
                .content
                .contains(secret));

            let stats = s.redact_secrets_in_session(&sess.id, false).unwrap();
            assert!(stats.messages_redacted >= 1);
            assert!(stats.archive_messages_redacted >= 1);
            assert!(!s.get(&sess.id).unwrap().unwrap().messages[0]
                .content
                .contains(secret));
            let page = s.read_archive_page(&sess.id, None, 10).unwrap();
            assert!(page.messages.iter().all(|m| !m.content.contains(secret)));
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn tool_calls_and_context_usage() {
        with_store(|store| async move {
            let s: Arc<dyn SessionStore> = store;
            let mut sess = s.get_or_create("cli", "u", "agent-a").unwrap();
            let mut tool = user_msg("");
            tool.role = MessageRole::Tool;
            tool.tool_data = Some(serde_json::json!({
                "name": "Read", "input": {"path": "x"}, "output": "ok",
                "success": true, "duration_ms": 12
            }));
            sess.messages.push(tool);
            s.save(&sess).unwrap();

            let calls = s.get_session_tool_calls(&sess.id).unwrap();
            assert_eq!(calls.len(), 1);
            assert_eq!(calls[0].tool_name, "Read");

            let usage = s.get_context_usage(&sess.id).unwrap();
            assert_eq!(usage.message_count, 1);
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn cache_stats_are_zero() {
        with_store(|store| async move {
            let s: Arc<dyn SessionStore> = store;
            let stats = s.cache_stats();
            assert_eq!(stats.sessions, 0);
            assert_eq!(stats.messages, 0);
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn concurrent_get_or_create_yields_one_active() {
        with_store(|store| async move {
            let s: Arc<dyn SessionStore> = store;
            let s1 = Arc::clone(&s);
            let s2 = Arc::clone(&s);
            let h1 = tokio::task::spawn_blocking(move || {
                s1.get_or_create("slack:race", "u", "agent-a").unwrap().id
            });
            let h2 = tokio::task::spawn_blocking(move || {
                s2.get_or_create("slack:race", "u", "agent-a").unwrap().id
            });
            let (id1, id2) = (h1.await.unwrap(), h2.await.unwrap());
            assert_eq!(id1, id2, "both creators converge on one active session");
            let listed = s.list(Some("slack:race"), 10).unwrap();
            assert_eq!(listed.len(), 1, "exactly one row exists");
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn concurrent_saves_preserve_both_turns() {
        with_store(|store| async move {
            let s: Arc<dyn SessionStore> = store;
            let base = s.get_or_create("cli", "u", "agent-a").unwrap();

            // Two writers each load the same revision, append a distinct turn,
            // and save. The append-preserving merge must keep both.
            let mut w1 = s.get(&base.id).unwrap().unwrap();
            let mut w2 = s.get(&base.id).unwrap().unwrap();
            let m1 = user_msg("from writer one");
            let m2 = user_msg("from writer two");
            let (id1, id2) = (m1.id.clone(), m2.id.clone());
            w1.messages.push(m1);
            w2.messages.push(m2);

            s.save(&w1).unwrap();
            // w2 still holds the pre-w1 revision → CAS miss → append-merge.
            s.save(&w2).unwrap();

            let got = s.get(&base.id).unwrap().unwrap();
            let ids: Vec<&str> = got.messages.iter().map(|m| m.id.as_str()).collect();
            assert!(ids.contains(&id1.as_str()), "writer one's turn survived");
            assert!(ids.contains(&id2.as_str()), "writer two's turn survived");
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn nul_in_content_survives_save_and_archive() {
        with_store(|store| async move {
            let s: Arc<dyn SessionStore> = store;
            let mut sess = s.get_or_create("cli", "u", "agent-a").unwrap();
            sess.messages.push(user_msg("tool spat a \0 nul"));
            s.save(&sess).unwrap();
            s.append_to_archive(&sess.id, &sess.messages[0]).unwrap();

            let got = s.get(&sess.id).unwrap().unwrap();
            assert_eq!(got.messages[0].content, "tool spat a  nul");

            // Replaying the same archive append (a CAS-miss retry) is a no-op.
            s.append_to_archive(&sess.id, &sess.messages[0]).unwrap();
            let page = s.read_archive_page(&sess.id, None, 10).unwrap();
            assert_eq!(page.messages.len(), 1, "double append inserted once");
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn turn_claim_is_exclusive_reentrant_and_stealable() {
        with_embedded_pg_pool(|pool| async move {
            PostgresSessionStore::with_pool(pool.clone(), "agent-a")
                .await
                .expect("run migrations");
            // Distinct owner identities model two hub processes serving the
            // same agent on the shared pool.
            let mine = PostgresSessionStore::with_shared_pool_as(pool.clone(), "agent-a", "hub-1");
            let theirs = PostgresSessionStore::with_shared_pool_as(pool, "agent-a", "hub-2");

            assert!(
                mine.try_claim_turn("s1", "t1", 60).unwrap(),
                "first claim wins"
            );
            assert!(
                !theirs.try_claim_turn("s1", "t2", 60).unwrap(),
                "another instance is refused while the claim is live"
            );
            assert!(
                mine.try_claim_turn("s1", "t1", 60).unwrap(),
                "the holder renews with the same claim id"
            );
            assert!(
                mine.try_claim_turn("s1", "t3", 60).unwrap(),
                "the holding instance supersedes its own claim for a new turn"
            );

            // The superseded turn's release must not drop the live claim.
            mine.release_turn_claim("s1", "t1").unwrap();
            assert!(
                !theirs.try_claim_turn("s1", "t4", 60).unwrap(),
                "stale release left the successor's claim intact"
            );

            mine.release_turn_claim("s1", "t3").unwrap();
            assert!(
                theirs.try_claim_turn("s1", "t5", 0).unwrap(),
                "released claim is free for another instance"
            );
            assert!(
                mine.try_claim_turn("s1", "t6", 60).unwrap(),
                "an expired claim is stolen"
            );

            assert!(
                theirs.try_claim_turn("s2", "u1", 60).unwrap(),
                "claims are per-session"
            );
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn dead_holders_claims_are_stolen_via_stale_heartbeat() {
        with_embedded_pg_pool(|pool| async move {
            PostgresSessionStore::with_pool(pool.clone(), "agent-a")
                .await
                .expect("run migrations");
            let dead =
                PostgresSessionStore::with_shared_pool_as(pool.clone(), "agent-a", "hub-dead");
            let live =
                PostgresSessionStore::with_shared_pool_as(pool.clone(), "agent-a", "hub-live");

            assert!(dead.try_claim_turn("s1", "t1", 900).unwrap());

            // No heartbeat row for the holder: only TTL frees the claim, so a
            // mixed-fleet older binary can never be insta-stolen.
            assert!(
                !live.try_claim_turn("s1", "t2", 900).unwrap(),
                "absent heartbeat row falls back to TTL"
            );

            // A fresh heartbeat protects the claim.
            dead.heartbeat_instance().unwrap();
            assert!(
                !live.try_claim_turn("s1", "t2", 900).unwrap(),
                "fresh heartbeat keeps the claim"
            );

            // Age the heartbeat past the staleness window: the owner is dead.
            {
                let conn = pool.get().await.unwrap();
                conn.execute(
                    "UPDATE instance_heartbeats
                     SET last_seen = now() - make_interval(secs => $1::double precision)
                     WHERE instance_id = 'hub-dead'",
                    &[&((HEARTBEAT_STALE_SECS + 5) as f64)],
                )
                .await
                .unwrap();
            }
            assert!(
                live.try_claim_turn("s1", "t2", 900).unwrap(),
                "stale heartbeat marks the owner dead and frees its claim"
            );
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn heartbeat_upserts_and_prunes_ancient_rows() {
        with_embedded_pg_pool(|pool| async move {
            PostgresSessionStore::with_pool(pool.clone(), "agent-a")
                .await
                .expect("run migrations");
            let store = PostgresSessionStore::with_shared_pool_as(pool.clone(), "agent-a", "hub-1");

            store.heartbeat_instance().unwrap();
            store.heartbeat_instance().unwrap();

            {
                let conn = pool.get().await.unwrap();
                conn.execute(
                    "INSERT INTO instance_heartbeats (instance_id, last_seen)
                     VALUES ('hub-ancient', now() - interval '2 days')",
                    &[],
                )
                .await
                .unwrap();
            }
            store.heartbeat_instance().unwrap();

            let conn = pool.get().await.unwrap();
            let rows = conn
                .query("SELECT instance_id FROM instance_heartbeats", &[])
                .await
                .unwrap();
            let ids: Vec<String> = rows.iter().map(|r| r.get(0)).collect();
            assert_eq!(
                ids,
                vec!["hub-1".to_string()],
                "ancient row pruned, own row upserted once"
            );
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn turn_claim_holder_reports_owner_and_expiry() {
        with_embedded_pg_pool(|pool| async move {
            PostgresSessionStore::with_pool(pool.clone(), "agent-a")
                .await
                .expect("run migrations");
            let mine = PostgresSessionStore::with_shared_pool_as(pool.clone(), "agent-a", "hub-1");
            let theirs = PostgresSessionStore::with_shared_pool_as(pool, "agent-a", "hub-2");

            assert_eq!(theirs.turn_claim_holder("s1").unwrap(), None);
            assert!(mine.try_claim_turn("s1", "t1", 900).unwrap());

            let (owner, remaining) = theirs.turn_claim_holder("s1").unwrap().expect("held");
            assert_eq!(owner, "hub-1");
            assert!(remaining > 0 && remaining <= 900, "remaining={remaining}");

            mine.release_turn_claim("s1", "t1").unwrap();
            assert_eq!(theirs.turn_claim_holder("s1").unwrap(), None);
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn renew_turn_claim_extends_only_the_live_claim() {
        with_embedded_pg_pool(|pool| async move {
            PostgresSessionStore::with_pool(pool.clone(), "agent-a")
                .await
                .expect("run migrations");
            let mine = PostgresSessionStore::with_shared_pool_as(pool.clone(), "agent-a", "hub-1");
            let theirs = PostgresSessionStore::with_shared_pool_as(pool, "agent-a", "hub-2");

            assert!(mine.try_claim_turn("s1", "t1", 60).unwrap());
            assert!(mine.renew_turn_claim("s1", "t1", 900).unwrap());
            let (_, remaining) = theirs.turn_claim_holder("s1").unwrap().expect("held");
            assert!(remaining > 60, "renewal extended the claim: {remaining}");
            assert!(
                !theirs.renew_turn_claim("s1", "t1", 900).unwrap(),
                "another instance cannot renew a claim it does not own"
            );

            assert!(mine.try_claim_turn("s1", "t2", 900).unwrap());
            assert!(
                !mine.renew_turn_claim("s1", "t1", 900).unwrap(),
                "a superseded claim id no longer renews"
            );
            mine.release_turn_claim("s1", "t1").unwrap();
            assert!(
                theirs.turn_claim_holder("s1").unwrap().is_some(),
                "the successor's claim survives the stale renewal and release"
            );

            mine.release_turn_claim("s1", "t2").unwrap();
            assert!(!mine.renew_turn_claim("s1", "t2", 900).unwrap());
            assert_eq!(
                theirs.turn_claim_holder("s1").unwrap(),
                None,
                "renewing a released claim does not resurrect it"
            );
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn release_owned_turn_claims_releases_only_own() {
        with_embedded_pg_pool(|pool| async move {
            PostgresSessionStore::with_pool(pool.clone(), "agent-a")
                .await
                .expect("run migrations");
            let mine = PostgresSessionStore::with_shared_pool_as(pool.clone(), "agent-a", "hub-1");
            let theirs = PostgresSessionStore::with_shared_pool_as(pool, "agent-b", "hub-2");

            assert!(mine.try_claim_turn("s1", "t1", 900).unwrap());
            assert!(mine.try_claim_turn("s2", "t2", 900).unwrap());
            assert!(theirs.try_claim_turn("s3", "t3", 900).unwrap());

            assert_eq!(mine.release_owned_turn_claims().unwrap(), 2);
            assert_eq!(
                theirs.turn_claim_holder("s3").unwrap().map(|(o, _)| o),
                Some("hub-2".to_string()),
                "another instance's claim survives the bulk release"
            );
            assert_eq!(mine.release_owned_turn_claims().unwrap(), 0, "idempotent");
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn save_refreshing_keeps_later_saves_uncontended() {
        with_store(|store| async move {
            let s: Arc<dyn SessionStore> = store;
            let mut sess = s.get_or_create("cli", "u", "agent-a").unwrap();
            sess.messages.push(user_msg("turn one"));
            sess.messages.push(user_msg("turn two"));
            s.save_refreshing(&mut sess).unwrap();

            // With the refreshed baseline the next save is uncontended, so a
            // message-reducing rewrite (compaction) lands instead of being
            // append-merged back together.
            sess.messages = vec![user_msg("summary of both turns")];
            s.save(&sess).unwrap();

            let got = s.get(&sess.id).unwrap().unwrap();
            assert_eq!(got.messages.len(), 1, "compaction landed exactly");
            assert_eq!(got.messages[0].content, "summary of both turns");

            // A plain save leaves the baseline stale: the same rewrite now
            // CAS-misses and append-merges instead of replacing.
            let mut stale = sess.clone();
            stale.messages.push(user_msg("post-summary turn"));
            s.save(&stale).unwrap();
            let got = s.get(&sess.id).unwrap().unwrap();
            assert_eq!(
                got.messages.len(),
                2,
                "stale-baseline save append-merged rather than replaced"
            );
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn stale_save_cannot_unseal_a_rolled_over_session() {
        with_store(|store| async move {
            let s: Arc<dyn SessionStore> = store;
            let mut active = s.get_or_create("cli", "u", "agent-a").unwrap();
            // A second writer holds the session at its pre-rollover revision.
            let mut stale = s.get(&active.id).unwrap().unwrap();

            // Roll over: seal the predecessor and insert a successor atomically.
            let mut successor = Session::new(&active.channel, &active.owner, &active.agent_id);
            successor.set_seq(active.seq().saturating_add(1));
            successor.set_prev_session_id(&active.id);
            active.seal();
            s.seal_and_insert_successor(&active, &successor).unwrap();

            // The stale writer appends a turn and saves against the now-sealed
            // predecessor: it must not error on the partial-unique index and
            // must not resurrect the predecessor as active.
            stale
                .messages
                .push(user_msg("late turn on a sealed session"));
            s.save(&stale).unwrap();

            let live = s.find("cli", "u").unwrap().unwrap();
            assert_eq!(
                live.id, successor.id,
                "successor stays the sole active session"
            );
            assert_eq!(
                s.list(Some("cli"), 10).unwrap().len(),
                2,
                "predecessor stays sealed alongside the successor"
            );
            let pred = s.get(&active.id).unwrap().unwrap();
            assert!(
                pred.messages
                    .iter()
                    .any(|m| m.content == "late turn on a sealed session"),
                "the stale append still landed on the predecessor — no data loss"
            );
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn start_fresh_keeps_one_active_session_under_the_partial_unique_index() {
        with_store(|store| async move {
            let s: Arc<dyn SessionStore> = store;
            let mut first = s.get_or_create("mcp:u", "u", "agent-a").unwrap();
            first.messages.push(user_msg("before the reset"));
            first.set_seq(5);
            s.save(&first).unwrap();

            let second = s.start_fresh("mcp:u", "u", "agent-a").unwrap();
            let third = s.start_fresh("mcp:u", "u", "agent-a").unwrap();
            assert_eq!((second.seq(), third.seq()), (6, 7));
            assert!(second.prev_session_id().is_none());

            let live = s.find("mcp:u", "u").unwrap().unwrap();
            assert_eq!(
                live.id, third.id,
                "the newest successor is the sole active row"
            );
            assert_eq!(live.message_count(), 0);

            let sealed = s.get(&first.id).unwrap().unwrap();
            assert!(sealed.is_sealed());
            assert_eq!(
                sealed.seq(),
                5,
                "sealing through a shell keeps its stored metadata"
            );
            assert_eq!(sealed.messages.len(), 1);
            assert!(s.get(&second.id).unwrap().unwrap().is_sealed());
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn save_from_spawn_blocking_does_not_panic() {
        // The redaction sweep drives store methods inside spawn_blocking; the
        // block_on shim must tolerate a blocking-pool caller.
        with_store(|store| async move {
            let s: Arc<dyn SessionStore> = store;
            let sess = s.get_or_create("cli", "u", "agent-a").unwrap();
            let s2 = Arc::clone(&s);
            let id = sess.id.clone();
            let report = tokio::task::spawn_blocking(move || {
                let mut sess = s2.get(&id).unwrap().unwrap();
                sess.messages
                    .push(user_msg("written from a blocking thread"));
                s2.save(&sess).unwrap();
                s2.redact_secrets_in_session(&id, false).unwrap();
                s2.get(&id).unwrap().unwrap().messages.len()
            })
            .await
            .expect("spawn_blocking task completes without panic");
            assert_eq!(report, 1);
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn saving_a_shell_does_not_clobber_messages() {
        with_store(|store| async move {
            let s: Arc<dyn SessionStore> = store;
            let mut sess = s.get_or_create("cli", "u", "agent-a").unwrap();
            sess.messages.push(user_msg("real content"));
            s.save(&sess).unwrap();

            // A shell from list() has empty messages + cached_message_count.
            let mut shell = s.list(None, 10).unwrap().into_iter().next().unwrap();
            assert!(shell.messages.is_empty());
            assert!(shell.cached_message_count.is_some());
            shell.label = Some("renamed via shell".into());
            s.save(&shell).unwrap();

            let got = s.get(&sess.id).unwrap().unwrap();
            assert_eq!(got.messages.len(), 1, "shell save preserved the transcript");
            assert_eq!(got.messages[0].content, "real content");
            assert_eq!(got.label.as_deref(), Some("renamed via shell"));
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn migrations_apply_owner_updated_index() {
        with_embedded_pg_pool(|pool| async move {
            // Store construction runs migrations; V002 must land its index.
            let _store = PostgresSessionStore::with_pool(pool.clone(), "agent-a")
                .await
                .expect("migrations apply");
            let conn = pool.get().await.expect("conn");
            let row = conn
                .query_one(
                    "SELECT COUNT(*) FROM pg_indexes
                     WHERE schemaname = 'sessions' AND indexname = 'sessions_owner_updated'",
                    &[],
                )
                .await
                .expect("catalog query");
            assert_eq!(row.get::<_, i64>(0), 1, "V002 owner+updated index exists");
        })
        .await;
    }

    /// V011 is additive: the tenant-first unique indexes stand beside the
    /// ones a hub replica on the previous binary names in its `ON CONFLICT`
    /// clauses, so a rolling deploy never fails a replica with 42P10. Only a
    /// tenanted deployment's own set may retire the old ones. `turn_claims`
    /// is the exception: its upsert names the primary key, which must be
    /// the table's one unique index for DO UPDATE to arbitrate every race.
    #[tokio::test(flavor = "multi_thread")]
    async fn migrations_keep_the_old_unique_targets_beside_the_tenant_ones() {
        with_embedded_pg_pool(|pool| async move {
            run_session_migrations(&pool).await.expect("run migrations");
            let conn = pool.get().await.expect("conn");
            async fn unique_indexes(conn: &Object, table: &str) -> Vec<String> {
                conn.query(
                    "SELECT i.relname FROM pg_index x
                     JOIN pg_class i ON i.oid = x.indexrelid
                     JOIN pg_class t ON t.oid = x.indrelid
                     JOIN pg_namespace n ON n.oid = t.relnamespace
                     WHERE n.nspname = 'sessions' AND t.relname = $1 AND x.indisunique
                     ORDER BY i.relname",
                    &[&table],
                )
                .await
                .expect("catalog query")
                .iter()
                .map(|r| r.get(0))
                .collect()
            }
            assert_eq!(
                unique_indexes(&conn, "sessions").await,
                [
                    "sessions_active_uniq",
                    "sessions_pkey",
                    "sessions_tenant_active_uniq"
                ]
            );
            assert_eq!(
                unique_indexes(&conn, "session_archive").await,
                [
                    "session_archive_message_uniq",
                    "session_archive_pkey",
                    "session_archive_tenant_message_uniq"
                ]
            );
            assert_eq!(
                unique_indexes(&conn, "turn_claims").await,
                ["turn_claims_pkey"],
                "the upsert's one arbiter"
            );
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn scoped_by_agent_id() {
        with_embedded_pg_pool(|pool| async move {
            let a = PostgresSessionStore::with_pool(pool.clone(), "agent-a")
                .await
                .unwrap();
            let b = PostgresSessionStore::with_shared_pool(pool, "agent-b");
            let sa: Arc<dyn SessionStore> = a;
            let sb: Arc<dyn SessionStore> = b;
            let created = sa.get_or_create("cli", "u", "agent-a").unwrap();
            // agent-b cannot see agent-a's session by id or in listings.
            assert!(sb.get(&created.id).unwrap().is_none());
            assert!(sb.list(None, 10).unwrap().is_empty());
            assert!(sa.get(&created.id).unwrap().is_some());
        })
        .await;
    }

    /// Two tenants sharing one conversation key is what a tenanted
    /// deployment holds once its own set (the edge's V002) has retired the
    /// single-tenant unique targets V011 keeps for the hub's rolling
    /// deploy. The shared set alone cannot allow it, so the test retires
    /// them the way that set does before asking the store to.
    #[tokio::test(flavor = "multi_thread")]
    async fn tenants_sharing_an_agent_channel_and_owner_each_get_their_own_session() {
        with_embedded_pg_pool(|pool| async move {
            PostgresSessionStore::with_pool(pool.clone(), "agent-a")
                .await
                .expect("run migrations");
            pool.get()
                .await
                .expect("conn")
                .batch_execute(
                    "DROP INDEX sessions_active_uniq;
                     DROP INDEX session_archive_message_uniq;
                     ALTER TABLE turn_claims
                         DROP CONSTRAINT turn_claims_pkey,
                         ADD CONSTRAINT turn_claims_pkey
                             PRIMARY KEY (tenant_id, agent_id, session_id)",
                )
                .await
                .expect("retire the single-tenant targets as a tenanted set does");
            let a: Arc<dyn SessionStore> =
                PostgresSessionStore::with_shared_pool_in_tenant(pool.clone(), "a", "agent-a");
            let b: Arc<dyn SessionStore> =
                PostgresSessionStore::with_shared_pool_in_tenant(pool, "b", "agent-a");

            let in_a = a.get_or_create("slack:C1", "u1", "agent-a").unwrap();
            let in_b = b.get_or_create("slack:C1", "u1", "agent-a").unwrap();
            assert_ne!(
                in_a.id, in_b.id,
                "the active-session invariant is per tenant"
            );

            assert_eq!(a.find("slack:C1", "u1").unwrap().unwrap().id, in_a.id);
            assert_eq!(b.find("slack:C1", "u1").unwrap().unwrap().id, in_b.id);
            assert!(a.get(&in_b.id).unwrap().is_none());
            assert!(b.get(&in_a.id).unwrap().is_none());

            let listed: Vec<String> = a
                .list(None, 10)
                .unwrap()
                .into_iter()
                .map(|s| s.id)
                .collect();
            assert_eq!(listed, vec![in_a.id.clone()]);
            let listed: Vec<String> = b
                .list(None, 10)
                .unwrap()
                .into_iter()
                .map(|s| s.id)
                .collect();
            assert_eq!(listed, vec![in_b.id.clone()]);

            a.append_to_archive(&in_a.id, &user_msg("only tenant a knows"))
                .unwrap();
            assert_eq!(a.search_sessions("knows", 10).unwrap().len(), 1);
            assert!(b.search_sessions("knows", 10).unwrap().is_empty());
            assert!(b
                .read_archive_page(&in_a.id, None, 10)
                .unwrap()
                .messages
                .is_empty());

            assert!(a.try_claim_turn(&in_a.id, "t1", 60).unwrap());
            assert!(
                b.try_claim_turn(&in_a.id, "t2", 60).unwrap(),
                "claims on the same session id do not collide across tenants"
            );
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_unscoped_store_lives_in_the_default_tenant() {
        with_embedded_pg_pool(|pool| async move {
            let store = PostgresSessionStore::with_pool(pool.clone(), "agent-a")
                .await
                .expect("run migrations");
            let sess = store.get_or_create("cli", "u", "agent-a").unwrap();
            store
                .append_to_archive(&sess.id, &user_msg("hello"))
                .unwrap();
            assert!(store.try_claim_turn(&sess.id, "t1", 60).unwrap());
            store
                .journal_append(
                    &sess.id,
                    &JournalEntry {
                        call: "c1".into(),
                        at: Utc::now(),
                        step: crate::session::JournalStep::Pending {
                            tool: "t".into(),
                            input: serde_json::json!({}),
                            plan: serde_json::json!({}),
                            actor: None,
                        },
                    },
                )
                .unwrap();

            let conn = pool.get().await.unwrap();
            for table in ["sessions", "session_archive", "turn_claims", "turn_journal"] {
                let tenants = conn
                    .query(&format!("SELECT DISTINCT tenant_id FROM {table}"), &[])
                    .await
                    .unwrap();
                let tenants: Vec<String> = tenants.iter().map(|r| r.get(0)).collect();
                assert_eq!(tenants, vec!["default".to_string()], "{table}");
            }

            assert_eq!(store.get(&sess.id).unwrap().unwrap().id, sess.id);
            assert_eq!(
                store
                    .read_archive_page(&sess.id, None, 10)
                    .unwrap()
                    .messages
                    .len(),
                1
            );
            assert!(store.turn_claim_holder(&sess.id).unwrap().is_some());
            assert_eq!(store.journal(&sess.id).unwrap().len(), 1);
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_tenant_setting_is_visible_inside_a_scoped_method_and_gone_after_it() {
        // The smallest pool the migration runner can run on (its lock
        // connection plus the apply connection), so that borrowing every
        // connection afterwards is cheap and is certain to include the one
        // the scoped method ran on — the one a leaked setting would ride.
        const POOL_SIZE: usize = 2;
        with_test_pool(
            |cfg| {
                let mut cfg = cfg.clone();
                cfg.pool_size = POOL_SIZE;
                PostgresSessionStore::build_pool(&cfg)
            },
            |pool| async move {
                run_session_migrations(&pool).await.expect("run migrations");
                let scoped = PostgresSessionStore::with_shared_pool_in_tenant(
                    pool.clone(),
                    "acme",
                    "agent-a",
                );
                let unscoped = PostgresSessionStore::with_shared_pool(pool.clone(), "agent-a");

                assert_eq!(
                    scoped.tenant_setting_as_seen_by_a_query().await.unwrap(),
                    Some("acme".to_string())
                );

                let mut held = Vec::with_capacity(POOL_SIZE);
                for _ in 0..POOL_SIZE {
                    held.push(pool.get().await.unwrap());
                }
                for conn in &held {
                    // Once a session has seen the placeholder Postgres keeps
                    // it defined and reports its reset value, the empty
                    // string, rather than NULL; either way no tenant remains.
                    let after: Option<String> = conn
                        .query_one("SELECT current_setting('app.tenant_id', true)", &[])
                        .await
                        .unwrap()
                        .get(0);
                    assert_eq!(
                        after.unwrap_or_default(),
                        "",
                        "the setting ended with the transaction"
                    );
                }
                drop(held);

                assert_eq!(
                    unscoped.tenant_setting_as_seen_by_a_query().await.unwrap(),
                    None,
                    "an unscoped store publishes nothing"
                );
            },
        )
        .await;
    }

    fn assistant_msg(content: &str) -> SessionMessage {
        SessionMessage {
            role: MessageRole::Assistant,
            ..user_msg(content)
        }
    }

    #[test]
    fn discarded_metadata_keys_names_additions_changes_and_removals() {
        let incoming = serde_json::json!({"kept": 1, "changed": "new", "added": true});
        let stored = serde_json::json!({"kept": 1, "changed": "old", "removed": true});
        assert_eq!(
            discarded_metadata_keys(&incoming, &stored),
            vec!["added", "changed", "removed"]
        );
        assert!(discarded_metadata_keys(&incoming, &incoming).is_empty());
        assert!(
            discarded_metadata_keys(&serde_json::Value::Null, &serde_json::Value::Null).is_empty()
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_postgres_turn_journal_keeps_its_contract_per_agent() {
        with_embedded_pg_pool(|pool| async move {
            let store = PostgresSessionStore::with_pool(pool.clone(), "agent-a")
                .await
                .expect("run migrations");
            let other = PostgresSessionStore::with_shared_pool_as(pool, "agent-b", "hub-1");
            let sess = store.get_or_create("slack:t:c:1", "u", "agent-a").unwrap();
            crate::session::assert_turn_journal_contract(store.as_ref(), &sess.id);

            store
                .journal_append(
                    &sess.id,
                    &JournalEntry {
                        call: "c1".into(),
                        at: Utc::now(),
                        step: crate::session::JournalStep::Pending {
                            tool: "t".into(),
                            input: serde_json::json!({}),
                            plan: serde_json::json!({}),
                            actor: None,
                        },
                    },
                )
                .unwrap();
            assert!(
                other.journal(&sess.id).unwrap().is_empty(),
                "journals are per agent"
            );
            store
                .pool
                .get()
                .await
                .unwrap()
                .execute(
                    "INSERT INTO turn_journal (agent_id, session_id, call_id, entry_key, entry)
                     VALUES ('agent-a', $1, 'c0', 'later', '{\"kind\": \"from_a_later_hub\"}')",
                    &[&sess.id],
                )
                .await
                .unwrap();
            assert_eq!(
                store.journal(&sess.id).unwrap().len(),
                1,
                "an entry this hub cannot read costs only itself"
            );
            store.delete(&sess.id).unwrap();
            assert!(store.journal(&sess.id).unwrap().is_empty());
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_finished_turn_is_never_resumable_even_when_its_save_lost_cas() {
        with_embedded_pg_pool(|pool| async move {
            PostgresSessionStore::with_pool(pool.clone(), "agent-a")
                .await
                .expect("run migrations");
            let mine = PostgresSessionStore::with_shared_pool_as(pool.clone(), "agent-a", "hub-1");
            let successor = PostgresSessionStore::with_shared_pool_as(pool, "agent-a", "hub-2");

            let mut sess = mine.get_or_create("slack:t:c:1", "u", "agent-a").unwrap();
            sess.messages.push(user_msg("what plugs do I need?"));
            mine.save_refreshing(&mut sess).unwrap();
            let message_id = sess.messages[0].id.clone();
            let token = mine
                .begin_turn(&sess.id, Some(&message_id), &TurnPrincipal::default())
                .unwrap();

            // The incident: another writer touches the row mid-turn, so the
            // turn's own post-run save is stale-baselined and its metadata is
            // discarded by the append-preserving merge.
            let mut concurrent = mine.get(&sess.id).unwrap().unwrap();
            concurrent.metadata = serde_json::json!({"channel_name": "DM with Kings"});
            mine.save(&concurrent).unwrap();

            sess.messages.push(assistant_msg("here are the plugs"));
            mine.save(&sess).unwrap();
            mine.end_turn(&sess.id, &token).unwrap();

            let got = mine.get(&sess.id).unwrap().unwrap();
            assert_eq!(
                got.metadata.get("channel_name").and_then(|v| v.as_str()),
                Some("DM with Kings"),
                "control: the turn's save really did lose CAS, so anything it carried in metadata was discarded"
            );
            assert_eq!(got.messages.len(), 2, "the answer still landed");
            assert!(
                successor.interrupted_turns().unwrap().is_empty(),
                "the turn finished; a successor must not re-run it"
            );
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_turn_record_is_outside_the_concurrency_baseline() {
        with_store(|store| async move {
            let s: Arc<dyn SessionStore> = store;
            let mut sess = s.get_or_create("slack:t:c:1", "u", "agent-a").unwrap();
            sess.messages.push(user_msg("first"));
            sess.messages.push(user_msg("second"));
            s.save_refreshing(&mut sess).unwrap();

            let message_id = sess.messages[0].id.clone();
            s.begin_turn(&sess.id, Some(&message_id), &TurnPrincipal::default())
                .unwrap();

            // Opening the record must not invalidate the turn's baseline: a
            // message-reducing rewrite still lands exactly, and the turn's own
            // metadata still applies.
            sess.messages = vec![user_msg("summary of both")];
            sess.metadata = serde_json::json!({"injected_lessons": ["l1"]});
            s.save(&sess).unwrap();

            let got = s.get(&sess.id).unwrap().unwrap();
            assert_eq!(got.messages.len(), 1, "the save was uncontended");
            assert!(got.metadata.get("injected_lessons").is_some());
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn turn_owned_metadata_survives_a_save_that_lost_cas() {
        with_store(|store| async move {
            let s: Arc<dyn SessionStore> = store;
            let mut sess = s.get_or_create("slack:t:c:1", "u", "agent-a").unwrap();
            sess.messages.push(user_msg("q"));
            s.save_refreshing(&mut sess).unwrap();

            // Someone else touches the row, so the turn's own save is about to
            // lose CAS and drop the metadata it carries.
            let mut concurrent = s.get(&sess.id).unwrap().unwrap();
            concurrent.metadata = serde_json::json!({"channel_name": "DM"});
            s.save(&concurrent).unwrap();

            sess.metadata = serde_json::json!({
                "observed_input_tokens": 4242,
                "round_cap_continuation": {"rounds": 12},
            });
            s.save(&sess).unwrap();
            assert!(
                s.get(&sess.id)
                    .unwrap()
                    .unwrap()
                    .metadata
                    .get("observed_input_tokens")
                    .is_none(),
                "control: the save really did lose its metadata"
            );

            // The narrow write is what makes the turn's own keys durable.
            let mut patch = serde_json::Map::new();
            patch.insert("observed_input_tokens".into(), serde_json::json!(4242));
            s.merge_metadata(&sess.id, patch, &["round_cap_continuation"])
                .unwrap();

            let got = s.get(&sess.id).unwrap().unwrap();
            assert_eq!(
                got.metadata.get("observed_input_tokens"),
                Some(&serde_json::json!(4242))
            );
            assert!(
                got.metadata.get("round_cap_continuation").is_none(),
                "a key absent from the finished turn is removed, not left stale"
            );
            assert_eq!(
                got.metadata.get("channel_name"),
                Some(&serde_json::json!("DM")),
                "keys the turn does not own are untouched"
            );
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn merging_metadata_leaves_a_live_turns_baseline_alone() {
        with_store(|store| async move {
            let s: Arc<dyn SessionStore> = store;
            let mut sess = s.get_or_create("slack:t:c:1", "u", "agent-a").unwrap();
            sess.messages.push(user_msg("one"));
            sess.messages.push(user_msg("two"));
            s.save_refreshing(&mut sess).unwrap();

            let mut patch = serde_json::Map::new();
            patch.insert("observed_input_tokens".into(), serde_json::json!(7));
            s.merge_metadata(&sess.id, patch, &[]).unwrap();

            // No revision bump, so a turn holding the pre-merge baseline still
            // applies exactly — the merge cannot cause the loss it prevents.
            sess.messages = vec![user_msg("compacted")];
            s.save(&sess).unwrap();
            assert_eq!(s.get(&sess.id).unwrap().unwrap().messages.len(), 1);
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_stale_token_never_clears_the_record_that_replaced_it() {
        with_embedded_pg_pool(|pool| async move {
            PostgresSessionStore::with_pool(pool.clone(), "agent-a")
                .await
                .expect("run migrations");
            let mine = PostgresSessionStore::with_shared_pool_as(pool.clone(), "agent-a", "hub-1");
            let successor = PostgresSessionStore::with_shared_pool_as(pool, "agent-a", "hub-2");

            let sess = mine.get_or_create("slack:t:c:1", "u", "agent-a").unwrap();
            let first = mine
                .begin_turn(&sess.id, Some("msg-a"), &TurnPrincipal::default())
                .unwrap();
            let second = mine
                .begin_turn(&sess.id, Some("msg-b"), &TurnPrincipal::default())
                .unwrap();
            assert_ne!(first, second);

            // The superseded turn's teardown lands after its successor opened
            // a record: it must clear nothing.
            mine.end_turn(&sess.id, &first).unwrap();
            let interrupted = successor.interrupted_turns().unwrap();
            assert_eq!(interrupted.len(), 1);
            assert_eq!(interrupted[0].message_id.as_deref(), Some("msg-b"));
            assert_eq!(interrupted[0].token, second);

            mine.end_turn(&sess.id, &second).unwrap();
            assert!(successor.interrupted_turns().unwrap().is_empty());
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn pg_end_turn_clears_turn_principal() {
        with_embedded_pg_pool(|pool| async move {
            let mine = PostgresSessionStore::with_pool(pool.clone(), "agent-a")
                .await
                .expect("run migrations");
            let successor =
                PostgresSessionStore::with_shared_pool_as(pool.clone(), "agent-a", "hub-2");
            let sess = mine.get_or_create("slack:t:c:1", "u", "agent-a").unwrap();
            let principal = TurnPrincipal {
                run_as: Some("6f1c1c3e-2a51-4a8e-9a57-0d6b1c9f2b11".into()),
                bill_to: Some("system:reports".into()),
            };

            let token = mine
                .begin_turn(&sess.id, Some("msg-a"), &principal)
                .unwrap();
            let interrupted = successor.interrupted_turns().unwrap();
            assert_eq!(interrupted[0].principal, principal);

            mine.end_turn(&sess.id, &token).unwrap();
            let conn = pool.get().await.unwrap();
            let row = conn
                .query_one(
                    "SELECT turn_run_as, turn_bill_to FROM sessions WHERE id = $1",
                    &[&Uuid::parse_str(&sess.id).unwrap()],
                )
                .await
                .unwrap();
            assert_eq!(row.get::<_, Option<String>>(0), None);
            assert_eq!(row.get::<_, Option<String>>(1), None);

            mine.begin_turn(&sess.id, None, &TurnPrincipal::default())
                .unwrap();
            assert_eq!(
                successor.interrupted_turns().unwrap()[0].principal,
                TurnPrincipal::default(),
                "a turn without a principal never inherits the last one's"
            );
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_principal_left_by_another_turn_is_never_resumed_as() {
        with_embedded_pg_pool(|pool| async move {
            let mine = PostgresSessionStore::with_pool(pool.clone(), "agent-a")
                .await
                .expect("run migrations");
            let successor =
                PostgresSessionStore::with_shared_pool_as(pool.clone(), "agent-a", "hub-2");
            let sess = mine.get_or_create("slack:t:c:1", "u", "agent-a").unwrap();
            let alice = TurnPrincipal {
                run_as: Some("6f1c1c3e-2a51-4a8e-9a57-0d6b1c9f2b11".into()),
                bill_to: None,
            };
            mine.begin_turn(&sess.id, Some("msg-a"), &alice).unwrap();

            // A replica that predates the principal opens the next turn: it
            // replaces the token and leaves alice's principal behind.
            pool.get()
                .await
                .unwrap()
                .execute(
                    "UPDATE sessions
                        SET turn_instance = 'hub-old', turn_started_at = now(),
                            turn_message_id = 'msg-b', turn_token = 'old-token'
                      WHERE id = $1",
                    &[&Uuid::parse_str(&sess.id).unwrap()],
                )
                .await
                .unwrap();

            let interrupted = successor.interrupted_turns().unwrap();
            assert_eq!(interrupted[0].token, "old-token");
            assert_eq!(
                interrupted[0].principal,
                TurnPrincipal::default(),
                "the old replica's turn resumes as the owner, never as alice"
            );
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_record_predating_the_token_column_is_only_ever_replaced() {
        with_embedded_pg_pool(|pool| async move {
            let store = PostgresSessionStore::with_pool(pool.clone(), "agent-a")
                .await
                .expect("run migrations");
            let sess = store.get_or_create("slack:t:c:1", "u", "agent-a").unwrap();

            // A record written before V008: instance set, token NULL.
            let conn = pool.get().await.unwrap();
            conn.execute(
                "UPDATE sessions SET turn_instance = 'hub-old', turn_started_at = now(),
                                     turn_token = NULL WHERE id = $1",
                &[&Uuid::parse_str(&sess.id).unwrap()],
            )
            .await
            .unwrap();

            let successor = PostgresSessionStore::with_shared_pool_as(pool, "agent-a", "hub-2");
            let interrupted = successor.interrupted_turns().unwrap();
            assert_eq!(interrupted.len(), 1);
            assert_eq!(interrupted[0].token, "", "a NULL token reads as empty");
            successor.end_turn(&sess.id, "").unwrap();
            assert_eq!(
                successor.interrupted_turns().unwrap().len(),
                1,
                "an empty token matches no record, so the sweep cannot clear it"
            );

            // The next turn on the session replaces it outright.
            let token = store
                .begin_turn(&sess.id, Some("msg-a"), &TurnPrincipal::default())
                .unwrap();
            store.end_turn(&sess.id, &token).unwrap();
            assert!(successor.interrupted_turns().unwrap().is_empty());
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_unfinished_turn_is_reported_to_a_successor_with_what_it_answered() {
        with_embedded_pg_pool(|pool| async move {
            PostgresSessionStore::with_pool(pool.clone(), "agent-a")
                .await
                .expect("run migrations");
            let dead = PostgresSessionStore::with_shared_pool_as(pool.clone(), "agent-a", "hub-1");
            let successor = PostgresSessionStore::with_shared_pool_as(pool, "agent-a", "hub-2");

            let mut sess = dead.get_or_create("slack:t:c:1", "u", "agent-a").unwrap();
            sess.messages.push(user_msg("what plugs do I need?"));
            dead.save_refreshing(&mut sess).unwrap();
            let message_id = sess.messages[0].id.clone();
            dead.begin_turn(&sess.id, Some(&message_id), &TurnPrincipal::default())
                .unwrap();

            assert!(
                dead.interrupted_turns().unwrap().is_empty(),
                "the instance running the turn never sweeps it"
            );
            let interrupted = successor.interrupted_turns().unwrap();
            assert_eq!(interrupted.len(), 1);
            assert_eq!(interrupted[0].session_id, sess.id);
            assert_eq!(
                interrupted[0].message_id.as_deref(),
                Some(message_id.as_str())
            );
            assert!(interrupted[0].started_at.is_some());

            successor.end_turn(&sess.id, &interrupted[0].token).unwrap();
            assert!(successor.interrupted_turns().unwrap().is_empty());
        })
        .await;
    }
}
