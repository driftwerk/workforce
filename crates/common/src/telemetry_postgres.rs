//! Postgres backend for [`crate::telemetry::TelemetryStore`].
//!
//! Phase 1B-2 of the multi-store migration roadmap (see
//! `docs/POSTGRES.md`). Mirrors every method of the SQLite backend at
//! `crate::persistence::SqliteStore`. Selected at runtime via
//! `[telemetry] backend = "postgres"` in `workforce.toml`; the
//! `TelemetryStore` trait extracted in Phase 1A means call sites are
//! unchanged regardless of which backend is in use.
//!
//! ## Async-to-sync bridge
//!
//! `TelemetryStore` is a synchronous trait (matching the historical
//! `SqliteStore` API). `tokio_postgres` is async. We bridge with
//! [`tokio::task::block_in_place`] + `Handle::current().block_on(...)`,
//! which yields the calling worker thread to the runtime for the
//! duration of the query. This requires a multi-threaded tokio runtime
//! — which the workforce hub always uses (`tokio::main(flavor =
//! "multi_thread")`) — and is the same pattern used by `sqlx`'s sync
//! API surface.
//!
//! ## Type translations
//!
//! - SQLite `TEXT` (RFC3339 timestamps) → Postgres `TIMESTAMPTZ` with
//!   `chrono::DateTime<Utc>` parsing/formatting at the binding layer.
//!   The row type fields stay `String` (RFC3339) so the public API is
//!   unchanged.
//! - SQLite `INTEGER` (boolean as 0/1) → Postgres `BOOLEAN` with
//!   native `bool` binding.
//! - `u64` token counts → `i64` (`BIGINT`) with `as` casts. Workforce
//!   token counts never approach `i64::MAX` so this is safe in
//!   practice.

use std::collections::HashMap;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use eyre::{Context, Result};

use crate::activity_log::{query_activities_postgres, ActivityQuery, ActivityRecord};
use crate::admin::{AuditEntry, AuditOutcome};
use crate::cost::TokenHistoryPoint;
use crate::persistence::{
    tier_column, AgentModelPreference, AgentUsageSummary, CacheStatusEntry, CostDetailFilter,
    CostDetailPage, CostDetailRow, CostRecord, CostRecordView, CostScope, CostSplitRow,
    DailyUsageRow, EfficiencyTrendPoint, InstanceFilter, MeteredSpend, ModelComparison,
    PolicyProposalRow, RoundStats, RoundTelemetryRecord, ScheduledJobHistoryPage, ScheduledJobRow,
    StoredModelPricing, TaskHistoryEntry, TaskHistoryFilter, TaskHistoryPage, TaskRow,
    TaskTelemetryRecord, TaskTrace, TaskUsageRow, TicketCostSummary, ToolCostEntry,
    ToolTelemetryRecord, ToolTurn, WorkflowInstanceRow,
};
use crate::postgres::{
    MigrationRun, MigrationRunner, MigrationSet, PostgresConfig, PostgresError, PostgresPool,
};
use crate::security_audit::{SecurityEvent, SecurityEventFilter, SecurityEventRow, SecurityStats};
use crate::subscription::Billing;
use crate::telemetry::TelemetryStore;

mod embedded {
    refinery::embed_migrations!("migrations/telemetry");
}

/// Postgres backend for the telemetry / cost / audit / workflow store.
pub struct PostgresTelemetryStore {
    pool: PostgresPool,
}

impl PostgresTelemetryStore {
    /// Schema this store owns. All telemetry tables live under it.
    pub const SCHEMA: &'static str = "telemetry";

    /// Build a [`PostgresPool`] suitable for telemetry queries: every
    /// connection it hands out has its `search_path` pinned to the
    /// telemetry schema, so unqualified table names always resolve.
    /// Used by [`Self::new`] and reusable by callers (e.g. the
    /// migration tool) that need raw pool access.
    pub fn build_pool(config: &PostgresConfig) -> Result<PostgresPool, PostgresError> {
        PostgresPool::from_config_with_session_init(
            config,
            &["SET search_path TO telemetry, public"],
        )
    }

    /// Build a pool from config, run pending migrations, and return a
    /// ready-to-use store. Caller is responsible for ensuring the
    /// configured Postgres user has `CREATE` on the database
    /// (refinery creates its own `refinery_schema_history` table on
    /// first run).
    pub async fn new(config: &PostgresConfig) -> Result<Arc<Self>> {
        let pool = Self::build_pool(config).context("building telemetry pool")?;
        Self::with_pool(pool).await
    }

    /// Variant that takes an already-built pool. The caller is
    /// responsible for using [`Self::build_pool`] (or matching the
    /// `search_path` configuration manually); otherwise unqualified
    /// table names won't resolve. Used by tests against the embedded
    /// Postgres harness.
    pub async fn with_pool(pool: PostgresPool) -> Result<Arc<Self>> {
        run_telemetry_migrations(&pool).await?;
        Ok(Arc::new(Self { pool }))
    }

    /// The store as an offline reader opens it: no migration is run and
    /// every transaction is read-only, so a newer binary on the host can
    /// neither change the schema under a running hub nor write.
    pub fn read_only(config: &PostgresConfig) -> Result<Arc<Self>> {
        let pool = Self::read_only_pool(config).context("building read-only telemetry pool")?;
        Ok(Arc::new(Self { pool }))
    }

    /// A telemetry-pinned pool whose transactions are all read-only.
    pub fn read_only_pool(config: &PostgresConfig) -> Result<PostgresPool, PostgresError> {
        PostgresPool::from_config_with_session_init(
            config,
            &[
                "SET search_path TO telemetry, public",
                workforce_postgres::READ_ONLY_SESSION,
            ],
        )
    }

    /// Run a future that returns `Result<T>` on the current Tokio
    /// runtime, blocking the calling worker thread for the duration.
    /// See the module-level "async-to-sync bridge" note.
    fn block_on<F, T>(&self, fut: F) -> Result<T>
    where
        F: std::future::Future<Output = Result<T>>,
    {
        tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(fut))
    }

    /// Full-row content write, applied only where `condition` (on `$14`,
    /// bound to `guard`) still holds. Moves the version on.
    fn update_workflow_instance_where<G>(
        &self,
        row: &WorkflowInstanceRow,
        condition: &str,
        guard: G,
    ) -> Result<bool>
    where
        G: tokio_postgres::types::ToSql + Sync + Send,
    {
        let row = row.clone();
        let sql = format!(
            "UPDATE workflow_instances SET
               workflow_id = $2, status = $3, current_step_id = $4,
               step_history = $5, variables = $6, trigger_json = $7,
               agent_id = $8, created_at = $9, updated_at = $10,
               parent_instance_id = $11, channel_id = $12, thread_id = $13,
               version = version + 1
             WHERE id = $1 AND {condition}"
        );
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let created_at = parse_rfc3339(&row.created_at)?;
            let updated_at = parse_rfc3339(&row.updated_at)?;
            let changed = conn
                .execute(
                    sql.as_str(),
                    &[
                        &row.id,
                        &row.workflow_id,
                        &row.status,
                        &row.current_step_id,
                        &row.step_history,
                        &row.variables,
                        &row.trigger_json,
                        &row.agent_id,
                        &created_at,
                        &updated_at,
                        &row.parent_instance_id,
                        &row.channel_id,
                        &row.thread_id,
                        &guard,
                    ],
                )
                .await?;
            Ok(changed == 1)
        })
    }

    /// Apply `change` to every signal row naming `from`, in one transaction.
    fn change_identity_signals(
        &self,
        change: fn(&crate::telemetry::SignalSource) -> crate::telemetry::Change,
        from: &str,
        to: Option<&str>,
    ) -> Result<crate::telemetry::IdentitySignals> {
        let (from, to) = (from.to_string(), to.map(str::to_string));
        self.block_on(async move {
            let mut conn = self.pool.get().await?;
            let tx = conn.transaction().await?;
            let mut changed = crate::telemetry::IdentitySignals::default();
            for source in &crate::telemetry::SIGNAL_SOURCES {
                let change = change(source);
                let sql = source.change_sql(change, '$');
                let rows = match (change, &to) {
                    (crate::telemetry::Change::Rename, Some(to)) => {
                        tx.execute(&sql, &[&from, to]).await?
                    }
                    (crate::telemetry::Change::Rename, None) => {
                        eyre::bail!("a rename needs the identity to name")
                    }
                    _ => tx.execute(&sql, &[&from]).await?,
                };
                changed = changed.add(source.signal, usize::try_from(rows).unwrap_or_default());
            }
            tx.commit().await?;
            Ok(changed)
        })
    }

    /// Run `delete` — bound to the cutoff, [`PRUNE_BATCH`] and, when given,
    /// `prefix` — a batch at a time ([`prune_in_batches`]); returns the rows
    /// removed. Each batch commits alone, and rows another replica's prune
    /// holds are skipped rather than waited on.
    ///
    /// [`PRUNE_BATCH`]: crate::telemetry::PRUNE_BATCH
    /// [`prune_in_batches`]: crate::telemetry::prune_in_batches
    fn prune(
        &self,
        delete: &'static str,
        before: DateTime<Utc>,
        prefix: Option<&'static str>,
    ) -> Result<usize> {
        let batch = crate::telemetry::PRUNE_BATCH as i64;
        let mut params: Vec<&(dyn tokio_postgres::types::ToSql + Sync)> = vec![&before, &batch];
        params.extend(
            prefix
                .as_ref()
                .map(|p| p as &(dyn tokio_postgres::types::ToSql + Sync)),
        );
        crate::telemetry::prune_in_batches(|| {
            self.block_on(async {
                let conn = self.pool.get().await?;
                Ok(conn.execute(delete, &params).await? as usize)
            })
        })
    }
}

/// Apply the embedded telemetry migration set (V001..) against a pool.
///
/// Idempotent — refinery records applied versions and skips them. Exposed
/// so other stores that live in the `telemetry` schema (e.g. the Artifacts
/// v2 `KnowledgeStore`, DW-549, whose tables ship in `V005__artifacts.sql`)
/// can guarantee their tables exist without constructing a full
/// [`PostgresTelemetryStore`]. The pool's `search_path` must include
/// `telemetry` (use [`PostgresTelemetryStore::build_pool`]).
pub async fn run_telemetry_migrations(pool: &PostgresPool) -> Result<()> {
    let migration_set = MigrationSet::new(
        PostgresTelemetryStore::SCHEMA,
        Box::new(EmbeddedTelemetryMigrations) as Box<dyn MigrationRun>,
    );
    MigrationRunner::run(pool, &[migration_set])
        .await
        .context("running telemetry migrations")?;
    Ok(())
}

struct EmbeddedTelemetryMigrations;

#[async_trait::async_trait]
impl MigrationRun for EmbeddedTelemetryMigrations {
    async fn run(&self, client: &mut tokio_postgres::Client) -> Result<(), PostgresError> {
        embedded::migrations::runner()
            .run_async(client)
            .await
            .map_err(PostgresError::from)?;
        Ok(())
    }
}

// ─── Helpers ───────────────────────────────────────────────────────────

fn parse_rfc3339(s: &str) -> Result<DateTime<Utc>> {
    Ok(DateTime::parse_from_rfc3339(s)
        .with_context(|| format!("invalid RFC3339 timestamp: {s}"))?
        .with_timezone(&Utc))
}

fn parse_rfc3339_opt(s: Option<&str>) -> Result<Option<DateTime<Utc>>> {
    s.map(parse_rfc3339).transpose()
}

/// Builds an `AND`-joined `WHERE` clause and a positional `$N`-bound
/// parameter list for a Postgres query, in lock-step. Each `add`
/// appends one predicate (with a single `?` placeholder rewritten to
/// `$N`) and one bound parameter — so the rendered SQL and parameter
/// list can never drift out of sync. Mirrors the SQLite `WhereBuilder`
/// in `persistence.rs`, but borrows its values rather than owning them
/// because tokio_postgres binds expect `&'a (dyn ToSql + Sync)`.
struct PgWhereBuilder<'a> {
    parts: Vec<String>,
    params: Vec<&'a (dyn tokio_postgres::types::ToSql + Sync)>,
}

impl<'a> PgWhereBuilder<'a> {
    fn new() -> Self {
        Self {
            parts: Vec::new(),
            params: Vec::new(),
        }
    }

    /// Push one predicate. The string must contain exactly one `?`
    /// placeholder, which is renumbered to its 1-based parameter index
    /// (`$N` in Postgres syntax).
    fn add(&mut self, predicate: &str, value: &'a (dyn tokio_postgres::types::ToSql + Sync)) {
        debug_assert_eq!(
            predicate.matches('?').count(),
            1,
            "PgWhereBuilder predicates must contain exactly one '?' placeholder"
        );
        let idx = self.params.len() + 1;
        self.parts
            .push(predicate.replacen('?', &format!("${idx}"), 1));
        self.params.push(value);
    }

    /// Push a predicate that has no bound parameters (e.g. an `IN`
    /// clause with literal values).
    fn add_literal(&mut self, predicate: &str) {
        debug_assert_eq!(
            predicate.matches('?').count(),
            0,
            "add_literal predicates must not contain '?' placeholders"
        );
        self.parts.push(predicate.to_string());
    }

    fn where_clause(&self) -> String {
        if self.parts.is_empty() {
            "1=1".to_string()
        } else {
            self.parts.join(" AND ")
        }
    }

    fn as_slice(&self) -> &[&'a (dyn tokio_postgres::types::ToSql + Sync)] {
        &self.params
    }
}

/// The time window and scope every cost query over `cost_records` shares.
fn pg_cost_scope_filter<'a>(
    from: &'a DateTime<Utc>,
    to: &'a DateTime<Utc>,
    scope: &'a CostScope<'a>,
) -> PgWhereBuilder<'a> {
    let mut filter = PgWhereBuilder::new();
    filter.add("timestamp >= ?", from);
    filter.add("timestamp <= ?", to);
    if let Some(agent_id) = &scope.agent_id {
        filter.add("agent_id = ?", agent_id);
    }
    if let Some(user_id) = &scope.user_id {
        filter.add("identity_id = ?", user_id);
    }
    filter
}

fn fmt_rfc3339(dt: DateTime<Utc>) -> String {
    dt.to_rfc3339()
}

fn fmt_rfc3339_opt(dt: Option<DateTime<Utc>>) -> Option<String> {
    dt.map(fmt_rfc3339)
}

fn audit_outcome_str(o: AuditOutcome) -> &'static str {
    match o {
        AuditOutcome::Success => "success",
        AuditOutcome::Denied => "denied",
        AuditOutcome::Error => "error",
    }
}

fn audit_outcome_from_str(s: &str) -> AuditOutcome {
    match s {
        "denied" => AuditOutcome::Denied,
        "error" => AuditOutcome::Error,
        _ => AuditOutcome::Success,
    }
}

// ─── Trait impl ────────────────────────────────────────────────────────

impl TelemetryStore for PostgresTelemetryStore {
    // ─── Tasks ───

    fn save_task(&self, task: &TaskRow) -> Result<()> {
        let task = task.clone();
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let created_at = parse_rfc3339(&task.created_at)?;
            let updated_at = parse_rfc3339(&task.updated_at)?;
            let started_at = parse_rfc3339_opt(task.started_at.as_deref())?;
            let completed_at = parse_rfc3339_opt(task.completed_at.as_deref())?;
            conn.execute(
                "INSERT INTO tasks
                 (id, title, description, status, priority, assignee, team_id, labels,
                  parent_id, result_json, created_at, updated_at, started_at, completed_at)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14)
                 ON CONFLICT (id) DO UPDATE SET
                     title = EXCLUDED.title,
                     description = EXCLUDED.description,
                     status = EXCLUDED.status,
                     priority = EXCLUDED.priority,
                     assignee = EXCLUDED.assignee,
                     team_id = EXCLUDED.team_id,
                     labels = EXCLUDED.labels,
                     parent_id = EXCLUDED.parent_id,
                     result_json = EXCLUDED.result_json,
                     updated_at = EXCLUDED.updated_at,
                     started_at = EXCLUDED.started_at,
                     completed_at = EXCLUDED.completed_at",
                &[
                    &task.id,
                    &task.title,
                    &task.description,
                    &task.status,
                    &task.priority,
                    &task.assignee,
                    &task.team_id,
                    &task.labels,
                    &task.parent_id,
                    &task.result_json,
                    &created_at,
                    &updated_at,
                    &started_at,
                    &completed_at,
                ],
            )
            .await?;
            Ok(())
        })
    }

    fn get_task(&self, id: &str) -> Result<Option<TaskRow>> {
        let id = id.to_string();
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let row = conn
                .query_opt(
                    "SELECT id, title, description, status, priority, assignee, team_id, labels,
                            parent_id, result_json, created_at, updated_at, started_at, completed_at
                     FROM tasks WHERE id = $1",
                    &[&id],
                )
                .await?;
            Ok(row.map(row_to_task))
        })
    }

    fn list_tasks(&self, status: Option<&str>) -> Result<Vec<TaskRow>> {
        let status = status.map(str::to_string);
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let rows = match status {
                Some(s) => {
                    conn.query(
                        "SELECT id, title, description, status, priority, assignee, team_id, labels,
                                parent_id, result_json, created_at, updated_at, started_at, completed_at
                         FROM tasks WHERE status = $1 ORDER BY created_at DESC",
                        &[&s],
                    )
                    .await?
                }
                None => {
                    conn.query(
                        "SELECT id, title, description, status, priority, assignee, team_id, labels,
                                parent_id, result_json, created_at, updated_at, started_at, completed_at
                         FROM tasks ORDER BY created_at DESC",
                        &[],
                    )
                    .await?
                }
            };
            Ok(rows.into_iter().map(row_to_task).collect())
        })
    }

    fn update_task_status(&self, id: &str, status: &str) -> Result<()> {
        let id = id.to_string();
        let status = status.to_string();
        self.block_on(async move {
            let conn = self.pool.get().await?;
            conn.execute(
                "UPDATE tasks SET status = $1, updated_at = $2 WHERE id = $3",
                &[&status, &Utc::now(), &id],
            )
            .await?;
            Ok(())
        })
    }

    // ─── Audit log ───

    fn record_audit(
        &self,
        actor: &str,
        action: &str,
        resource: &str,
        detail: Option<&str>,
        outcome: AuditOutcome,
    ) -> Result<()> {
        let actor = actor.to_string();
        let action = action.to_string();
        let resource = resource.to_string();
        let detail = detail.map(str::to_string);
        let outcome = audit_outcome_str(outcome);
        self.block_on(async move {
            let conn = self.pool.get().await?;
            conn.execute(
                "INSERT INTO audit_log (timestamp, actor, action, resource, detail, outcome)
                 VALUES ($1, $2, $3, $4, $5, $6)",
                &[&Utc::now(), &actor, &action, &resource, &detail, &outcome],
            )
            .await?;
            Ok(())
        })
    }

    fn recent_audit(&self, limit: usize) -> Result<Vec<AuditEntry>> {
        let limit = limit as i64;
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let rows = conn
                .query(
                    "SELECT timestamp, actor, action, resource, detail, outcome
                     FROM audit_log ORDER BY id DESC LIMIT $1",
                    &[&limit],
                )
                .await?;
            Ok(rows.into_iter().map(row_to_audit).collect())
        })
    }

    fn search_audit(&self, query: &str, limit: usize) -> Result<Vec<AuditEntry>> {
        let pattern = format!("%{query}%");
        let limit = limit as i64;
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let rows = conn
                .query(
                    "SELECT timestamp, actor, action, resource, detail, outcome
                     FROM audit_log
                     WHERE actor LIKE $1 OR action LIKE $1 OR resource LIKE $1 OR detail LIKE $1
                     ORDER BY id DESC LIMIT $2",
                    &[&pattern, &limit],
                )
                .await?;
            Ok(rows.into_iter().map(row_to_audit).collect())
        })
    }

    // ─── Cost records ───

    fn record_cost(&self, record: &CostRecord) -> Result<bool> {
        let record = record.clone();
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let inserted = conn
                .execute(
                    "INSERT INTO cost_records
                 (timestamp, agent_id, model, input_tokens, output_tokens,
                  cache_creation_input_tokens, cache_read_input_tokens, cost_usd,
                  team_id, task_id, task_type, ticket_id, identity_id, description,
                  origin, billing, payer, credential_tier, ledger_key)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14,
                         $15, $16, $17, $18, $19)
                 ON CONFLICT (ledger_key) WHERE ledger_key IS NOT NULL DO NOTHING",
                    &[
                        &Utc::now(),
                        &record.agent_id,
                        &record.model,
                        &(record.input_tokens as i64),
                        &(record.output_tokens as i64),
                        &(record.cache_creation_input_tokens as i64),
                        &(record.cache_read_input_tokens as i64),
                        &record.cost_usd,
                        &record.team_id,
                        &record.task_id,
                        &record.task_type,
                        &record.ticket_id,
                        &record.identity_id,
                        &record.description,
                        &record.origin_str(),
                        &record.billing.as_str(),
                        &record.payer,
                        &record.credential_tier_str(),
                        &record.ledger_key,
                    ],
                )
                .await?;
            Ok(inserted > 0)
        })
    }

    fn total_cost(&self, from: DateTime<Utc>, to: DateTime<Utc>) -> Result<f64> {
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let row = conn
                .query_one(
                    "SELECT COALESCE(SUM(cost_usd), 0.0) FROM cost_records
                     WHERE timestamp >= $1 AND timestamp <= $2",
                    &[&from, &to],
                )
                .await?;
            Ok(row.get::<_, f64>(0))
        })
    }

    fn cost_by_agent(&self, from: DateTime<Utc>, to: DateTime<Utc>) -> Result<Vec<(String, f64)>> {
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let rows = conn
                .query(
                    "SELECT agent_id, SUM(cost_usd) FROM cost_records
                     WHERE timestamp >= $1 AND timestamp <= $2
                     GROUP BY agent_id ORDER BY SUM(cost_usd) DESC",
                    &[&from, &to],
                )
                .await?;
            Ok(rows
                .into_iter()
                .map(|r| (r.get::<_, String>(0), r.get::<_, f64>(1)))
                .collect())
        })
    }

    fn cost_by_model(&self, from: DateTime<Utc>, to: DateTime<Utc>) -> Result<Vec<(String, f64)>> {
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let rows = conn
                .query(
                    "SELECT model, SUM(cost_usd) FROM cost_records
                     WHERE timestamp >= $1 AND timestamp <= $2
                     GROUP BY model ORDER BY SUM(cost_usd) DESC",
                    &[&from, &to],
                )
                .await?;
            Ok(rows
                .into_iter()
                .map(|r| (r.get::<_, String>(0), r.get::<_, f64>(1)))
                .collect())
        })
    }

    fn cost_token_totals(
        &self,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<(u64, u64, usize)> {
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let row = conn
                .query_one(
                    "SELECT COALESCE(SUM(input_tokens), 0)::BIGINT,
                            COALESCE(SUM(output_tokens), 0)::BIGINT,
                            COUNT(*)::BIGINT
                     FROM cost_records WHERE timestamp >= $1 AND timestamp <= $2",
                    &[&from, &to],
                )
                .await?;
            Ok((
                row.get::<_, i64>(0) as u64,
                row.get::<_, i64>(1) as u64,
                row.get::<_, i64>(2) as usize,
            ))
        })
    }

    fn cost_token_history_by_day(
        &self,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<Vec<TokenHistoryPoint>> {
        self.block_on(async move {
            let conn = self.pool.get().await?;
            // `date_trunc('day', ts)` on a TIMESTAMPTZ uses the session timezone,
            // so go via `AT TIME ZONE 'UTC'` to guarantee UTC-midnight buckets
            // regardless of the Postgres session config. The SQLite path
            // truncates RFC3339 strings, which is always UTC.
            let rows = conn
                .query(
                    "SELECT (date_trunc('day', timestamp AT TIME ZONE 'UTC') AT TIME ZONE 'UTC') AS day,
                            COALESCE(SUM(input_tokens), 0)::BIGINT,
                            COALESCE(SUM(output_tokens), 0)::BIGINT,
                            COALESCE(SUM(cache_creation_input_tokens), 0)::BIGINT,
                            COALESCE(SUM(cache_read_input_tokens), 0)::BIGINT
                     FROM cost_records
                     WHERE timestamp >= $1 AND timestamp <= $2
                     GROUP BY day
                     ORDER BY day ASC",
                    &[&from, &to],
                )
                .await?;
            Ok(rows
                .into_iter()
                .map(|r| TokenHistoryPoint {
                    bucket_start: r.get::<_, DateTime<Utc>>(0),
                    input_tokens: r.get::<_, i64>(1) as u64,
                    output_tokens: r.get::<_, i64>(2) as u64,
                    cache_creation_input_tokens: r.get::<_, i64>(3) as u64,
                    cache_read_input_tokens: r.get::<_, i64>(4) as u64,
                })
                .collect())
        })
    }

    fn cost_token_history_by_month(
        &self,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<Vec<TokenHistoryPoint>> {
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let rows = conn
                .query(
                    "SELECT (date_trunc('month', timestamp AT TIME ZONE 'UTC') AT TIME ZONE 'UTC') AS month,
                            COALESCE(SUM(input_tokens), 0)::BIGINT,
                            COALESCE(SUM(output_tokens), 0)::BIGINT,
                            COALESCE(SUM(cache_creation_input_tokens), 0)::BIGINT,
                            COALESCE(SUM(cache_read_input_tokens), 0)::BIGINT
                     FROM cost_records
                     WHERE timestamp >= $1 AND timestamp <= $2
                     GROUP BY month
                     ORDER BY month ASC",
                    &[&from, &to],
                )
                .await?;
            Ok(rows
                .into_iter()
                .map(|r| TokenHistoryPoint {
                    bucket_start: r.get::<_, DateTime<Utc>>(0),
                    input_tokens: r.get::<_, i64>(1) as u64,
                    output_tokens: r.get::<_, i64>(2) as u64,
                    cache_creation_input_tokens: r.get::<_, i64>(3) as u64,
                    cache_read_input_tokens: r.get::<_, i64>(4) as u64,
                })
                .collect())
        })
    }

    fn cost_by_provider(
        &self,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<Vec<(String, f64)>> {
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let rows = conn
                .query(
                    "SELECT COALESCE(provider, 'unknown'), SUM(cost_usd)
                     FROM task_telemetry
                     WHERE completed_at >= $1 AND completed_at <= $2
                     GROUP BY provider ORDER BY SUM(cost_usd) DESC",
                    &[&from, &to],
                )
                .await?;
            Ok(rows
                .into_iter()
                .map(|r| (r.get::<_, String>(0), r.get::<_, f64>(1)))
                .collect())
        })
    }

    fn cost_detail_records(
        &self,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
        filter: CostDetailFilter<'_>,
        limit: u32,
        offset: u32,
    ) -> Result<CostDetailPage> {
        let billing = filter.billing.map(Billing::as_str);
        let users_scope = CostScope {
            user_id: None,
            ..filter.scope
        };
        let agents_scope = CostScope::default();
        self.block_on(async move {
            let conn = self.pool.get().await?;

            let mut page_filter = pg_cost_scope_filter(&from, &to, &filter.scope);
            if let Some(billing) = &billing {
                page_filter.add("billing = ?", billing);
            }

            let total: u64 = conn
                .query_one(
                    &format!(
                        "SELECT COUNT(*)::BIGINT FROM cost_records WHERE {}",
                        page_filter.where_clause()
                    ),
                    page_filter.as_slice(),
                )
                .await?
                .get::<_, i64>(0) as u64;

            let data_sql = format!(
                "SELECT id, timestamp, agent_id, model, input_tokens, output_tokens,
                        cache_creation_input_tokens, cache_read_input_tokens, cost_usd,
                        team_id, task_id, task_type, ticket_id, identity_id, description,
                        origin, billing, payer, credential_tier
                 FROM cost_records
                 WHERE {}
                 ORDER BY timestamp DESC
                 LIMIT {limit} OFFSET {offset}",
                page_filter.where_clause()
            );
            let records: Vec<CostDetailRow> = conn
                .query(&data_sql, page_filter.as_slice())
                .await?
                .into_iter()
                .map(|r| CostDetailRow {
                    id: r.get::<_, i64>(0),
                    timestamp: fmt_rfc3339(r.get::<_, DateTime<Utc>>(1)),
                    agent_id: r.get(2),
                    model: r.get(3),
                    input_tokens: r.get::<_, i64>(4) as u64,
                    output_tokens: r.get::<_, i64>(5) as u64,
                    cache_creation_input_tokens: r.get::<_, i64>(6) as u64,
                    cache_read_input_tokens: r.get::<_, i64>(7) as u64,
                    cost_usd: r.get(8),
                    team_id: r.get(9),
                    task_id: r.get(10),
                    task_type: r.get(11),
                    ticket_id: r.get(12),
                    user_id: r.get(13),
                    description: r.get(14),
                    origin: r.get(15),
                    billing: Billing::from_column(r.get(16)),
                    payer: r.get(17),
                    credential_tier: tier_column(r.get(18)),
                })
                .collect();

            // Distinct users in time + agent range. The user filter is
            // intentionally dropped so the dropdown stays complete when a
            // user is already selected.
            let udist = pg_cost_scope_filter(&from, &to, &users_scope);
            let users_sql = format!(
                "SELECT DISTINCT identity_id FROM cost_records
                 WHERE {} AND identity_id IS NOT NULL
                 ORDER BY identity_id",
                udist.where_clause()
            );
            let distinct_users: Vec<String> = conn
                .query(&users_sql, udist.as_slice())
                .await?
                .into_iter()
                .map(|r| r.get::<_, String>(0))
                .collect();

            // Distinct agents in the time range, ignoring both filters.
            let adist = pg_cost_scope_filter(&from, &to, &agents_scope);
            let distinct_agents: Vec<String> = conn
                .query(
                    &format!(
                        "SELECT DISTINCT agent_id FROM cost_records
                         WHERE {}
                         ORDER BY agent_id",
                        adist.where_clause()
                    ),
                    adist.as_slice(),
                )
                .await?
                .into_iter()
                .map(|r| r.get::<_, String>(0))
                .collect();

            Ok(CostDetailPage {
                records,
                total,
                distinct_users,
                distinct_agents,
            })
        })
    }

    fn cost_split(
        &self,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
        scope: CostScope<'_>,
    ) -> Result<Vec<CostSplitRow>> {
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let filter = pg_cost_scope_filter(&from, &to, &scope);
            let sql = format!(
                "SELECT agent_id, billing, origin, payer, credential_tier,
                        COUNT(*)::BIGINT, SUM(cost_usd)
                 FROM cost_records
                 WHERE {}
                 GROUP BY agent_id, billing, origin, payer, credential_tier
                 ORDER BY agent_id, billing, origin, payer, credential_tier",
                filter.where_clause()
            );
            Ok(conn
                .query(&sql, filter.as_slice())
                .await?
                .into_iter()
                .map(|r| CostSplitRow {
                    agent_id: r.get(0),
                    billing: Billing::from_column(r.get(1)),
                    origin: r.get(2),
                    payer: r.get(3),
                    credential_tier: tier_column(r.get(4)),
                    calls: r.get::<_, i64>(5) as u64,
                    cost_usd: r.get(6),
                })
                .collect())
        })
    }

    fn scheduled_job_history(
        &self,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
        scope: CostScope<'_>,
        limit: u32,
        offset: u32,
    ) -> Result<ScheduledJobHistoryPage> {
        let agent_id = scope.agent_id.map(String::from);
        let user_id = scope.user_id.map(String::from);
        self.block_on(async move {
            let conn = self.pool.get().await?;

            let mut filter = PgWhereBuilder::new();
            filter.add_literal("t.provider IN ('job_runner', 'scheduler')");
            filter.add("t.completed_at >= ?", &from);
            filter.add("t.completed_at <= ?", &to);
            if let Some(ref aid) = agent_id {
                filter.add("t.agent_id = ?", aid);
            }
            if let Some(ref user_id) = user_id {
                filter.add("t.identity_id = ?", user_id);
            }

            let total: u64 = conn
                .query_one(
                    &format!(
                        "SELECT COUNT(*)::BIGINT FROM task_telemetry t WHERE {}",
                        filter.where_clause()
                    ),
                    filter.as_slice(),
                )
                .await?
                .get::<_, i64>(0) as u64;

            let data_sql = format!(
                "SELECT t.id, t.agent_id, t.task_type, t.task_id, t.description, t.success,
                        t.error_type, t.duration_ms, t.started_at, t.completed_at, t.identity_id,
                        COALESCE(c.total_cost, 0.0)::FLOAT8 AS cost_usd
                 FROM task_telemetry t
                 LEFT JOIN (
                     SELECT task_id, SUM(cost_usd) AS total_cost
                     FROM cost_records
                     WHERE task_id IS NOT NULL
                     GROUP BY task_id
                 ) c ON c.task_id = t.task_id
                 WHERE {}
                 ORDER BY t.completed_at DESC
                 LIMIT {limit} OFFSET {offset}",
                filter.where_clause()
            );
            let records: Vec<ScheduledJobRow> = conn
                .query(&data_sql, filter.as_slice())
                .await?
                .into_iter()
                .map(|r| ScheduledJobRow {
                    id: r.get::<_, i64>(0),
                    agent_id: r.get(1),
                    task_type: r.get(2),
                    task_id: r.get(3),
                    description: r.get(4),
                    success: r.get(5),
                    error_type: r.get(6),
                    duration_ms: r.get::<_, i64>(7) as u64,
                    started_at: fmt_rfc3339(r.get::<_, DateTime<Utc>>(8)),
                    completed_at: fmt_rfc3339(r.get::<_, DateTime<Utc>>(9)),
                    user_id: r.get(10),
                    cost_usd: r.get::<_, f64>(11),
                })
                .collect();

            Ok(ScheduledJobHistoryPage { records, total })
        })
    }

    // ─── Team / agent usage ───

    fn get_team_usage_summary(
        &self,
        team_id: &str,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<Vec<AgentUsageSummary>> {
        let team_id = team_id.to_string();
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let rows = conn
                .query(
                    "SELECT agent_id, COUNT(*)::BIGINT, SUM(input_tokens)::BIGINT,
                            SUM(output_tokens)::BIGINT, SUM(cost_usd)::FLOAT8,
                            AVG(rounds)::FLOAT8, AVG(duration_ms)::FLOAT8,
                            AVG(CASE WHEN success THEN 1.0 ELSE 0.0 END)::FLOAT8,
                            SUM(cache_creation_input_tokens)::BIGINT,
                            SUM(cache_read_input_tokens)::BIGINT
                     FROM task_telemetry
                     WHERE team_id = $1 AND completed_at >= $2 AND completed_at <= $3
                     GROUP BY agent_id
                     ORDER BY SUM(cost_usd) DESC",
                    &[&team_id, &from, &to],
                )
                .await?;
            Ok(rows.into_iter().map(row_to_agent_usage).collect())
        })
    }

    fn get_team_cost(&self, team_id: &str, from: DateTime<Utc>, to: DateTime<Utc>) -> Result<f64> {
        let team_id = team_id.to_string();
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let row = conn
                .query_one(
                    "SELECT COALESCE(SUM(cost_usd), 0.0) FROM cost_records
                     WHERE team_id = $1 AND timestamp >= $2 AND timestamp <= $3",
                    &[&team_id, &from, &to],
                )
                .await?;
            Ok(row.get::<_, f64>(0))
        })
    }

    fn cost_by_team(&self, from: DateTime<Utc>, to: DateTime<Utc>) -> Result<Vec<(String, f64)>> {
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let rows = conn
                .query(
                    "SELECT team_id, SUM(cost_usd) FROM cost_records
                     WHERE team_id IS NOT NULL AND timestamp >= $1 AND timestamp <= $2
                     GROUP BY team_id ORDER BY SUM(cost_usd) DESC",
                    &[&from, &to],
                )
                .await?;
            Ok(rows
                .into_iter()
                .map(|r| (r.get::<_, String>(0), r.get::<_, f64>(1)))
                .collect())
        })
    }

    // ─── Telemetry recording ───

    fn record_task_telemetry(
        &self,
        task: &TaskTelemetryRecord,
        rounds: &[RoundTelemetryRecord],
        tools: &[ToolTelemetryRecord],
    ) -> Result<i64> {
        let task = task.clone();
        let rounds = rounds.to_vec();
        let tools = tools.to_vec();
        self.block_on(async move {
            let mut conn = self.pool.get().await?;
            let tx = conn.transaction().await?;
            let started_at = parse_rfc3339(&task.started_at)?;
            let completed_at = parse_rfc3339(&task.completed_at)?;
            let row = tx
                .query_one(
                    "INSERT INTO task_telemetry
                     (agent_id, model, provider, input_tokens, output_tokens,
                      cache_creation_input_tokens, cache_read_input_tokens,
                      cost_usd, rounds, duration_ms, tool_invocation_count,
                      success, error_type, retry_count, task_type, task_id,
                      started_at, completed_at, forecast_cost_usd, forecast_rounds, forecast_confidence,
                      team_id, session_id, model_switches, ticket_id, identity_id, description)
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15,
                             $16, $17, $18, $19, $20, $21, $22, $23, $24, $25, $26, $27)
                     RETURNING id",
                    &[
                        &task.agent_id,
                        &task.model,
                        &task.provider,
                        &(task.input_tokens as i64),
                        &(task.output_tokens as i64),
                        &(task.cache_creation_input_tokens as i64),
                        &(task.cache_read_input_tokens as i64),
                        &task.cost_usd,
                        &(task.rounds as i64),
                        &(task.duration_ms as i64),
                        &(task.tool_invocation_count as i64),
                        &task.success,
                        &task.error_type,
                        &(task.retry_count as i64),
                        &task.task_type,
                        &task.task_id,
                        &started_at,
                        &completed_at,
                        &task.forecast_cost_usd,
                        &task.forecast_rounds.map(|r| r as i64),
                        &task.forecast_confidence,
                        &task.team_id,
                        &task.session_id,
                        &task.model_switches,
                        &task.ticket_id,
                        &task.identity_id,
                        &task.description,
                    ],
                )
                .await?;
            let telemetry_id: i64 = row.get(0);

            for r in &rounds {
                tx.execute(
                    "INSERT INTO round_telemetry
                     (task_telemetry_id, round_number, input_tokens, output_tokens,
                      latency_ms, cache_creation_tokens, cache_read_tokens,
                      prompt_size_bytes, response_size_bytes, tool_count, stop_reason)
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)",
                    &[
                        &telemetry_id,
                        &(r.round_number as i64),
                        &(r.input_tokens as i64),
                        &(r.output_tokens as i64),
                        &(r.latency_ms as i64),
                        &(r.cache_creation_tokens as i64),
                        &(r.cache_read_tokens as i64),
                        &(r.prompt_size_bytes as i64),
                        &(r.response_size_bytes as i64),
                        &(r.tool_count as i64),
                        &r.stop_reason,
                    ],
                )
                .await?;
            }

            for t in &tools {
                tx.execute(
                    "INSERT INTO tool_telemetry
                     (task_telemetry_id, round_number, tool_name, success,
                      duration_ms, policy_decision, input_size_bytes, output_size_bytes,
                      denial_reason)
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
                    &[
                        &telemetry_id,
                        &(t.round_number as i64),
                        &t.tool_name,
                        &t.success,
                        &(t.duration_ms as i64),
                        &t.policy_decision,
                        &(t.input_size_bytes as i64),
                        &(t.output_size_bytes as i64),
                        &t.denial_reason,
                    ],
                )
                .await?;
            }

            tx.commit().await?;
            Ok(telemetry_id)
        })
    }

    fn get_agent_usage_summary(
        &self,
        agent_id: &str,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
        group_by: &str,
    ) -> Result<Vec<AgentUsageSummary>> {
        let agent_id = agent_id.to_string();
        let group_col = match group_by {
            "task_type" => "COALESCE(task_type, 'unknown')",
            "provider" => "COALESCE(provider, 'unknown')",
            _ => "model",
        };
        let sql = format!(
            "SELECT {group_col} AS group_key,
                    COUNT(*)::BIGINT,
                    SUM(input_tokens)::BIGINT,
                    SUM(output_tokens)::BIGINT,
                    SUM(cost_usd)::FLOAT8,
                    AVG(rounds)::FLOAT8,
                    AVG(duration_ms)::FLOAT8,
                    AVG(CASE WHEN success THEN 1.0 ELSE 0.0 END)::FLOAT8,
                    SUM(cache_creation_input_tokens)::BIGINT,
                    SUM(cache_read_input_tokens)::BIGINT
             FROM task_telemetry
             WHERE agent_id = $1 AND completed_at >= $2 AND completed_at <= $3
             GROUP BY {group_col}
             ORDER BY SUM(cost_usd) DESC"
        );
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let rows = conn.query(&sql, &[&agent_id, &from, &to]).await?;
            Ok(rows.into_iter().map(row_to_agent_usage).collect())
        })
    }

    fn compare_models_for_agent(
        &self,
        agent_id: &str,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<Vec<ModelComparison>> {
        self.compare_models_filtered("agent_id", agent_id.to_string(), from, to)
    }

    fn get_task_history(
        &self,
        agent_id: &str,
        limit: usize,
        task_type: Option<&str>,
        model: Option<&str>,
    ) -> Result<Vec<TaskHistoryEntry>> {
        self.get_task_history_filtered(agent_id, limit, task_type, model, None, None)
    }

    fn get_task_history_filtered(
        &self,
        agent_id: &str,
        limit: usize,
        task_type: Option<&str>,
        model: Option<&str>,
        since: Option<&str>,
        sort_by: Option<&str>,
    ) -> Result<Vec<TaskHistoryEntry>> {
        let agent_id = agent_id.to_string();
        let task_type = task_type.map(str::to_string);
        let model = model.map(str::to_string);
        let since = parse_rfc3339_opt(since)?;
        let order_clause = match sort_by {
            Some("cost") => "cost_usd DESC",
            Some("duration") => "duration_ms DESC",
            Some("tokens") => "(input_tokens + output_tokens) DESC",
            _ => "completed_at DESC",
        };
        let sql = format!(
            "SELECT {TASK_HISTORY_COLUMNS}
             FROM task_telemetry
             WHERE agent_id = $1
               AND ($2::TEXT IS NULL OR task_type = $2)
               AND ($3::TEXT IS NULL OR model = $3)
               AND ($4::TIMESTAMPTZ IS NULL OR completed_at >= $4)
             ORDER BY {order_clause}
             LIMIT $5"
        );
        let limit = limit as i64;
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let rows = conn
                .query(&sql, &[&agent_id, &task_type, &model, &since, &limit])
                .await?;
            Ok(rows.into_iter().map(row_to_task_history).collect())
        })
    }

    // ─── Agent model preferences ───

    fn upsert_agent_model_preference(&self, pref: &AgentModelPreference) -> Result<()> {
        let pref = pref.clone();
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let updated_at = parse_rfc3339(&pref.updated_at)?;
            conn.execute(
                "INSERT INTO agent_model_preferences (agent_id, task_type, preferred_model, reason, updated_at)
                 VALUES ($1, $2, $3, $4, $5)
                 ON CONFLICT (agent_id, task_type) DO UPDATE SET
                     preferred_model = EXCLUDED.preferred_model,
                     reason = EXCLUDED.reason,
                     updated_at = EXCLUDED.updated_at",
                &[
                    &pref.agent_id,
                    &pref.task_type,
                    &pref.preferred_model,
                    &pref.reason,
                    &updated_at,
                ],
            )
            .await?;
            Ok(())
        })
    }

    fn get_agent_model_preference(
        &self,
        agent_id: &str,
        task_type: &str,
    ) -> Result<Option<AgentModelPreference>> {
        let agent_id = agent_id.to_string();
        let task_type = task_type.to_string();
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let row = conn
                .query_opt(
                    "SELECT agent_id, task_type, preferred_model, reason, updated_at
                     FROM agent_model_preferences
                     WHERE agent_id = $1 AND task_type = $2",
                    &[&agent_id, &task_type],
                )
                .await?;
            Ok(row.map(|r| AgentModelPreference {
                agent_id: r.get(0),
                task_type: r.get(1),
                preferred_model: r.get(2),
                reason: r.get(3),
                updated_at: fmt_rfc3339(r.get::<_, DateTime<Utc>>(4)),
            }))
        })
    }

    // ─── Model health ───

    fn record_model_health_event(
        &self,
        model: &str,
        event_type: &str,
        detail: Option<&str>,
        agent_id: Option<&str>,
    ) -> Result<()> {
        let model = model.to_string();
        let event_type = event_type.to_string();
        let detail = detail.map(str::to_string);
        let agent_id = agent_id.map(str::to_string);
        self.block_on(async move {
            let conn = self.pool.get().await?;
            conn.execute(
                "INSERT INTO model_health_events (timestamp, model, event_type, detail, agent_id)
                 VALUES ($1, $2, $3, $4, $5)",
                &[&Utc::now(), &model, &event_type, &detail, &agent_id],
            )
            .await?;
            Ok(())
        })
    }

    fn record_lesson_event(&self, event: &crate::telemetry::LessonEvent) -> Result<()> {
        let event = event.clone();
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let tags =
                serde_json::to_string(&event.trigger_tags).unwrap_or_else(|_| "[]".to_string());
            conn.execute(
                "INSERT INTO lesson_events
                    (timestamp, agent_id, identity_id, lesson_id, event_type, learning_kind,
                     recall_score, importance_delta, confidence_after, applied_after, session_id, trigger_tags)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)",
                &[
                    &event.timestamp,
                    &event.agent_id,
                    &event.identity_id,
                    &event.lesson_id,
                    &event.event_type,
                    &event.learning_kind,
                    &event.recall_score,
                    &event.importance_delta,
                    &event.confidence_after,
                    &event.applied_after,
                    &event.session_id,
                    &tags,
                ],
            )
            .await?;
            Ok(())
        })
    }

    fn lesson_events_in_range(
        &self,
        agent_id: Option<&str>,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<Vec<crate::telemetry::LessonEvent>> {
        let agent_id = agent_id.map(str::to_string);
        self.block_on(async move {
            let conn = self.pool.get().await?;
            const COLS: &str = "timestamp, agent_id, identity_id, lesson_id, event_type, learning_kind, \
                 recall_score, importance_delta, confidence_after, applied_after, session_id, trigger_tags";
            let rows = if let Some(agent) = &agent_id {
                conn.query(
                    &format!(
                        "SELECT {COLS} FROM lesson_events
                         WHERE timestamp >= $1 AND timestamp <= $2 AND agent_id = $3
                         ORDER BY timestamp ASC"
                    ),
                    &[&from, &to, agent],
                )
                .await?
            } else {
                conn.query(
                    &format!(
                        "SELECT {COLS} FROM lesson_events
                         WHERE timestamp >= $1 AND timestamp <= $2
                         ORDER BY timestamp ASC"
                    ),
                    &[&from, &to],
                )
                .await?
            };
            Ok(rows
                .into_iter()
                .map(|r| {
                    let tags_raw: Option<String> = r.get(11);
                    crate::telemetry::LessonEvent {
                        timestamp: r.get(0),
                        agent_id: r.get(1),
                        identity_id: r.get(2),
                        lesson_id: r.get(3),
                        event_type: r.get(4),
                        learning_kind: r.get(5),
                        recall_score: r.get(6),
                        importance_delta: r.get(7),
                        confidence_after: r.get(8),
                        applied_after: r.get(9),
                        session_id: r.get(10),
                        trigger_tags: tags_raw
                            .and_then(|s| serde_json::from_str::<Vec<String>>(&s).ok())
                            .unwrap_or_default(),
                    }
                })
                .collect())
        })
    }

    fn record_relationship_event(
        &self,
        event: &crate::telemetry::RelationshipEvent,
    ) -> Result<bool> {
        let event = event.clone();
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let affected = conn
                .execute(
                    "INSERT INTO relationship_events
                        (timestamp, agent_id, identity_id, session_id, message_id, valence, intensity,
                         emotion_label, reaction, is_key_moment, confidence, source, signals)
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)
                     ON CONFLICT (session_id, message_id) DO NOTHING",
                    &[
                        &event.timestamp,
                        &event.agent_id,
                        &event.identity_id,
                        &event.session_id,
                        &event.message_id,
                        &event.valence,
                        &event.intensity,
                        &event.emotion_label,
                        &event.reaction,
                        &event.is_key_moment,
                        &event.confidence,
                        &event.source,
                        &event.signals,
                    ],
                )
                .await?;
            Ok(affected > 0)
        })
    }

    fn relationship_events_in_range(
        &self,
        agent_id: Option<&str>,
        identity_id: Option<&str>,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<Vec<crate::telemetry::RelationshipEvent>> {
        let agent_id = agent_id.map(str::to_string);
        let identity_id = identity_id.map(str::to_string);
        self.block_on(async move {
            let conn = self.pool.get().await?;
            const COLS: &str =
                "timestamp, agent_id, identity_id, session_id, message_id, valence, \
                 intensity, emotion_label, reaction, is_key_moment, confidence, source, signals";
            let rows = conn
                .query(
                    &format!(
                        "SELECT {COLS} FROM relationship_events
                         WHERE timestamp >= $1 AND timestamp <= $2
                           AND ($3::text IS NULL OR agent_id = $3)
                           AND ($4::text IS NULL OR identity_id = $4)
                         ORDER BY timestamp ASC"
                    ),
                    &[&from, &to, &agent_id, &identity_id],
                )
                .await?;
            Ok(rows
                .into_iter()
                .map(|r| crate::telemetry::RelationshipEvent {
                    timestamp: r.get(0),
                    agent_id: r.get(1),
                    identity_id: r.get(2),
                    session_id: r.get(3),
                    message_id: r.get(4),
                    valence: r.get(5),
                    intensity: r.get(6),
                    emotion_label: r.get(7),
                    reaction: r.get(8),
                    is_key_moment: r.get(9),
                    confidence: r.get(10),
                    source: r.get(11),
                    signals: r.get(12),
                })
                .collect())
        })
    }

    fn get_relationship_cursor(
        &self,
        session_id: &str,
    ) -> Result<Option<crate::telemetry::RelationshipCursor>> {
        let session_id = session_id.to_string();
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let row = conn
                .query_opt(
                    "SELECT session_id, agent_id, last_message_ts, last_message_id, updated_at
                     FROM relationship_cursor WHERE session_id = $1",
                    &[&session_id],
                )
                .await?;
            Ok(row.map(|r| crate::telemetry::RelationshipCursor {
                session_id: r.get(0),
                agent_id: r.get(1),
                last_message_ts: r.get(2),
                last_message_id: r.get(3),
                updated_at: r.get(4),
            }))
        })
    }

    fn upsert_relationship_cursor(
        &self,
        cursor: &crate::telemetry::RelationshipCursor,
    ) -> Result<()> {
        let cursor = cursor.clone();
        self.block_on(async move {
            let conn = self.pool.get().await?;
            conn.execute(
                "INSERT INTO relationship_cursor
                    (session_id, agent_id, last_message_ts, last_message_id, updated_at)
                 VALUES ($1, $2, $3, $4, $5)
                 ON CONFLICT (session_id) DO UPDATE SET
                    agent_id = excluded.agent_id,
                    last_message_ts = excluded.last_message_ts,
                    last_message_id = excluded.last_message_id,
                    updated_at = excluded.updated_at",
                &[
                    &cursor.session_id,
                    &cursor.agent_id,
                    &cursor.last_message_ts,
                    &cursor.last_message_id,
                    &cursor.updated_at,
                ],
            )
            .await?;
            Ok(())
        })
    }

    // ─── Cache and tool stats ───

    fn get_cache_status(
        &self,
        agent_id: &str,
        from: DateTime<Utc>,
    ) -> Result<Vec<CacheStatusEntry>> {
        let agent_id = agent_id.to_string();
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let rows = conn
                .query(
                    "SELECT model,
                            SUM(cache_creation_input_tokens)::BIGINT,
                            SUM(cache_read_input_tokens)::BIGINT,
                            SUM(input_tokens)::BIGINT,
                            COUNT(*)::BIGINT
                     FROM task_telemetry
                     WHERE agent_id = $1 AND completed_at >= $2
                     GROUP BY model",
                    &[&agent_id, &from],
                )
                .await?;
            Ok(rows
                .into_iter()
                .map(|r| CacheStatusEntry {
                    model: r.get(0),
                    total_cache_creation_tokens: r.get::<_, i64>(1) as u64,
                    total_cache_read_tokens: r.get::<_, i64>(2) as u64,
                    total_input_tokens: r.get::<_, i64>(3) as u64,
                    task_count: r.get::<_, i64>(4) as u64,
                })
                .collect())
        })
    }

    fn get_round_cache_stats(&self, agent_id: &str, from: DateTime<Utc>) -> Result<(u64, u64)> {
        let agent_id = agent_id.to_string();
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let row = conn
                .query_one(
                    "SELECT COUNT(*)::BIGINT,
                            SUM(CASE WHEN r.cache_read_tokens > 0 THEN 1 ELSE 0 END)::BIGINT
                     FROM round_telemetry r
                     JOIN task_telemetry t ON r.task_telemetry_id = t.id
                     WHERE t.agent_id = $1 AND t.completed_at >= $2",
                    &[&agent_id, &from],
                )
                .await?;
            let total = row.get::<_, i64>(0) as u64;
            let hits = row.try_get::<_, i64>(1).unwrap_or(0) as u64;
            Ok((total, hits))
        })
    }

    fn get_tool_cost_breakdown(
        &self,
        agent_id: &str,
        from: DateTime<Utc>,
        limit: usize,
    ) -> Result<Vec<ToolCostEntry>> {
        self.tool_breakdown_filtered("agent_id", agent_id.to_string(), from, limit)
    }

    fn agent_tool_turns(
        &self,
        agent_id: &str,
        from: DateTime<Utc>,
        limit: usize,
    ) -> Result<Vec<ToolTurn>> {
        let agent_id = agent_id.to_string();
        let limit = limit as i64;
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let rows = conn
                .query(
                    "SELECT t.started_at,
                            COALESCE(ARRAY_AGG(DISTINCT tt.tool_name)
                                     FILTER (WHERE tt.tool_name IS NOT NULL), '{}')
                     FROM task_telemetry t
                     LEFT JOIN tool_telemetry tt ON tt.task_telemetry_id = t.id
                     WHERE t.agent_id = $1 AND t.completed_at >= $2
                       AND (t.task_type IS NULL OR t.task_type NOT LIKE 'job:%')
                     GROUP BY t.id
                     ORDER BY t.started_at
                     LIMIT $3",
                    &[&agent_id, &from, &limit],
                )
                .await?;
            Ok(rows
                .into_iter()
                .map(|row| ToolTurn {
                    started_at: row.get(0),
                    tools: row.get(1),
                })
                .collect())
        })
    }

    fn get_routing_history(
        &self,
        agent_id: &str,
        since: DateTime<Utc>,
        limit: usize,
    ) -> Result<Vec<TaskHistoryEntry>> {
        let agent_id = agent_id.to_string();
        let limit = limit as i64;
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let sql = format!(
                "SELECT {TASK_HISTORY_COLUMNS}
                 FROM task_telemetry
                 WHERE agent_id = $1
                   AND model_switches IS NOT NULL AND model_switches != '[]'
                   AND completed_at >= $2
                 ORDER BY completed_at DESC
                 LIMIT $3"
            );
            let rows = conn.query(&sql, &[&agent_id, &since, &limit]).await?;
            Ok(rows.into_iter().map(row_to_task_history).collect())
        })
    }

    // ─── Per-ticket cost ───

    fn cost_by_ticket(&self, ticket_id: &str) -> Result<TicketCostSummary> {
        let ticket_id = ticket_id.to_string();
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let row = conn
                .query_one(
                    "SELECT COALESCE(SUM(cost_usd), 0.0)::FLOAT8,
                            COALESCE(SUM(input_tokens), 0)::BIGINT,
                            COALESCE(SUM(output_tokens), 0)::BIGINT,
                            COALESCE(SUM(rounds), 0)::BIGINT,
                            COALESCE(SUM(duration_ms), 0)::BIGINT,
                            COUNT(*)::BIGINT,
                            STRING_AGG(DISTINCT model, ',')
                     FROM task_telemetry
                     WHERE ticket_id = $1",
                    &[&ticket_id],
                )
                .await?;
            let models_str: Option<String> = row.get(6);
            let models_used = models_str
                .map(|s| s.split(',').map(String::from).collect())
                .unwrap_or_default();
            Ok(TicketCostSummary {
                ticket_id: ticket_id.clone(),
                total_cost_usd: row.get(0),
                total_input_tokens: row.get::<_, i64>(1) as u64,
                total_output_tokens: row.get::<_, i64>(2) as u64,
                total_rounds: row.get::<_, i64>(3) as u64,
                total_duration_ms: row.get::<_, i64>(4) as u64,
                step_count: row.get::<_, i64>(5) as u64,
                models_used,
            })
        })
    }

    fn cost_by_tickets(
        &self,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<Vec<TicketCostSummary>> {
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let rows = conn
                .query(
                    "SELECT ticket_id,
                            COALESCE(SUM(cost_usd), 0.0)::FLOAT8,
                            COALESCE(SUM(input_tokens), 0)::BIGINT,
                            COALESCE(SUM(output_tokens), 0)::BIGINT,
                            COALESCE(SUM(rounds), 0)::BIGINT,
                            COALESCE(SUM(duration_ms), 0)::BIGINT,
                            COUNT(*)::BIGINT,
                            STRING_AGG(DISTINCT model, ',')
                     FROM task_telemetry
                     WHERE ticket_id IS NOT NULL
                       AND completed_at >= $1 AND completed_at <= $2
                     GROUP BY ticket_id
                     ORDER BY SUM(cost_usd) DESC",
                    &[&from, &to],
                )
                .await?;
            Ok(rows
                .into_iter()
                .map(|r| {
                    let models_str: Option<String> = r.get(7);
                    let models_used = models_str
                        .map(|s| s.split(',').map(String::from).collect())
                        .unwrap_or_default();
                    TicketCostSummary {
                        ticket_id: r.get(0),
                        total_cost_usd: r.get(1),
                        total_input_tokens: r.get::<_, i64>(2) as u64,
                        total_output_tokens: r.get::<_, i64>(3) as u64,
                        total_rounds: r.get::<_, i64>(4) as u64,
                        total_duration_ms: r.get::<_, i64>(5) as u64,
                        step_count: r.get::<_, i64>(6) as u64,
                        models_used,
                    }
                })
                .collect())
        })
    }

    // ─── Model pricing ───

    fn upsert_model_pricing(&self, pricing: &StoredModelPricing) -> Result<()> {
        let pricing = pricing.clone();
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let fetched_at = parse_rfc3339(&pricing.fetched_at)?;
            let updated_at = parse_rfc3339(&pricing.updated_at)?;
            conn.execute(
                "INSERT INTO model_pricing
                 (model, provider, input_per_million, output_per_million, source, fetched_at, updated_at)
                 VALUES ($1, $2, $3, $4, $5, $6, $7)
                 ON CONFLICT (model) DO UPDATE SET
                     provider = EXCLUDED.provider,
                     input_per_million = EXCLUDED.input_per_million,
                     output_per_million = EXCLUDED.output_per_million,
                     source = EXCLUDED.source,
                     fetched_at = EXCLUDED.fetched_at,
                     updated_at = EXCLUDED.updated_at",
                &[
                    &pricing.model,
                    &pricing.provider,
                    &pricing.input_per_million,
                    &pricing.output_per_million,
                    &pricing.source,
                    &fetched_at,
                    &updated_at,
                ],
            )
            .await?;
            Ok(())
        })
    }

    fn get_model_pricing(&self, model: &str) -> Result<Option<StoredModelPricing>> {
        let model = model.to_string();
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let row = conn
                .query_opt(
                    "SELECT model, provider, input_per_million, output_per_million,
                            source, fetched_at, updated_at
                     FROM model_pricing WHERE model = $1",
                    &[&model],
                )
                .await?;
            Ok(row.map(row_to_model_pricing))
        })
    }

    fn list_model_pricing(&self) -> Result<Vec<StoredModelPricing>> {
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let rows = conn
                .query(
                    "SELECT model, provider, input_per_million, output_per_million,
                            source, fetched_at, updated_at
                     FROM model_pricing ORDER BY model ASC",
                    &[],
                )
                .await?;
            Ok(rows.into_iter().map(row_to_model_pricing).collect())
        })
    }

    fn delete_model_pricing(&self, model: &str) -> Result<()> {
        let model = model.to_string();
        self.block_on(async move {
            let conn = self.pool.get().await?;
            conn.execute("DELETE FROM model_pricing WHERE model = $1", &[&model])
                .await?;
            Ok(())
        })
    }

    // ─── Policy proposals ───

    fn create_policy_proposal(
        &self,
        agent_id: &str,
        proposed_rule: &str,
        justification: &str,
    ) -> Result<i64> {
        let agent_id = agent_id.to_string();
        let proposed_rule = proposed_rule.to_string();
        let justification = justification.to_string();
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let row = conn
                .query_one(
                    "INSERT INTO policy_proposals (agent_id, proposed_rule, justification, status, created_at)
                     VALUES ($1, $2, $3, 'pending', $4)
                     RETURNING id",
                    &[&agent_id, &proposed_rule, &justification, &Utc::now()],
                )
                .await?;
            Ok(row.get::<_, i64>(0))
        })
    }

    fn list_policy_proposals(
        &self,
        status: Option<&str>,
        limit: usize,
    ) -> Result<Vec<PolicyProposalRow>> {
        let status = status.map(str::to_string);
        let limit = limit as i64;
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let rows = conn
                .query(
                    "SELECT id, agent_id, proposed_rule, justification, status,
                            reviewed_by, review_note, created_at, reviewed_at
                     FROM policy_proposals
                     WHERE ($1::TEXT IS NULL OR status = $1)
                     ORDER BY created_at DESC
                     LIMIT $2",
                    &[&status, &limit],
                )
                .await?;
            Ok(rows.into_iter().map(row_to_policy_proposal).collect())
        })
    }

    fn get_policy_proposal(&self, id: i64) -> Result<Option<PolicyProposalRow>> {
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let row = conn
                .query_opt(
                    "SELECT id, agent_id, proposed_rule, justification, status,
                            reviewed_by, review_note, created_at, reviewed_at
                     FROM policy_proposals WHERE id = $1",
                    &[&id],
                )
                .await?;
            Ok(row.map(row_to_policy_proposal))
        })
    }

    fn review_policy_proposal(
        &self,
        id: i64,
        status: &str,
        reviewed_by: &str,
        note: Option<&str>,
    ) -> Result<()> {
        let status = status.to_string();
        let reviewed_by = reviewed_by.to_string();
        let note = note.map(str::to_string);
        self.block_on(async move {
            let conn = self.pool.get().await?;
            conn.execute(
                "UPDATE policy_proposals
                 SET status = $1, reviewed_by = $2, review_note = $3, reviewed_at = $4
                 WHERE id = $5",
                &[&status, &reviewed_by, &note, &Utc::now(), &id],
            )
            .await?;
            Ok(())
        })
    }

    // ─── Round outliers / efficiency ───

    fn get_round_outliers(
        &self,
        agent_id: &str,
        since: DateTime<Utc>,
        threshold_multiplier: f64,
        limit: usize,
    ) -> Result<(RoundStats, Vec<TaskHistoryEntry>)> {
        self.round_outliers_filtered(
            Some(agent_id.to_string()),
            since,
            threshold_multiplier,
            limit,
        )
    }

    fn get_efficiency_trends(
        &self,
        agent_id: &str,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<Vec<EfficiencyTrendPoint>> {
        let agent_id = agent_id.to_string();
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let rows = conn
                .query(
                    "SELECT TO_CHAR(completed_at, 'YYYY-MM-DD') AS bucket,
                            COUNT(*)::BIGINT,
                            AVG(input_tokens)::FLOAT8,
                            AVG(output_tokens)::FLOAT8,
                            AVG(cost_usd)::FLOAT8,
                            AVG(rounds)::FLOAT8,
                            AVG(duration_ms)::FLOAT8,
                            AVG(CASE WHEN success THEN 1.0 ELSE 0.0 END)::FLOAT8,
                            AVG(cache_read_input_tokens)::FLOAT8
                     FROM task_telemetry
                     WHERE agent_id = $1 AND completed_at >= $2 AND completed_at <= $3
                     GROUP BY bucket
                     ORDER BY bucket ASC",
                    &[&agent_id, &from, &to],
                )
                .await?;
            Ok(rows
                .into_iter()
                .map(|r| EfficiencyTrendPoint {
                    bucket: r.get(0),
                    total_tasks: r.get::<_, i64>(1) as u64,
                    avg_input_tokens: r.get(2),
                    avg_output_tokens: r.get(3),
                    avg_cost: r.get(4),
                    avg_rounds: r.get(5),
                    avg_duration_ms: r.get(6),
                    success_rate: r.get(7),
                    avg_cache_read_tokens: r.get(8),
                })
                .collect())
        })
    }

    // ─── Workflow instances ───

    fn save_workflow_instance(&self, row: &WorkflowInstanceRow) -> Result<()> {
        let row = row.clone();
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let created_at = parse_rfc3339(&row.created_at)?;
            let updated_at = parse_rfc3339(&row.updated_at)?;
            conn.execute(
                "INSERT INTO workflow_instances
                 (id, workflow_id, status, current_step_id, step_history,
                  variables, trigger_json, agent_id, created_at, updated_at,
                  parent_instance_id, channel_id, thread_id)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)
                 ON CONFLICT (id) DO UPDATE SET
                     workflow_id = EXCLUDED.workflow_id,
                     status = EXCLUDED.status,
                     current_step_id = EXCLUDED.current_step_id,
                     step_history = EXCLUDED.step_history,
                     variables = EXCLUDED.variables,
                     trigger_json = EXCLUDED.trigger_json,
                     agent_id = EXCLUDED.agent_id,
                     updated_at = EXCLUDED.updated_at,
                     parent_instance_id = EXCLUDED.parent_instance_id,
                     channel_id = EXCLUDED.channel_id,
                     thread_id = EXCLUDED.thread_id,
                     version = workflow_instances.version + 1",
                &[
                    &row.id,
                    &row.workflow_id,
                    &row.status,
                    &row.current_step_id,
                    &row.step_history,
                    &row.variables,
                    &row.trigger_json,
                    &row.agent_id,
                    &created_at,
                    &updated_at,
                    &row.parent_instance_id,
                    &row.channel_id,
                    &row.thread_id,
                ],
            )
            .await?;
            Ok(())
        })
    }

    fn get_workflow_instance(&self, id: &str) -> Result<Option<WorkflowInstanceRow>> {
        let id = id.to_string();
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let row = conn
                .query_opt(
                    format!("SELECT {INSTANCE_COLUMNS} FROM workflow_instances WHERE id = $1")
                        .as_str(),
                    &[&id],
                )
                .await?;
            Ok(row.map(row_to_workflow_instance))
        })
    }

    fn get_workflow_instances(&self, ids: &[String]) -> Result<Vec<WorkflowInstanceRow>> {
        let ids = ids.to_vec();
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let rows = conn
                .query(
                    format!("SELECT {INSTANCE_COLUMNS} FROM workflow_instances WHERE id = ANY($1)")
                        .as_str(),
                    &[&ids],
                )
                .await?;
            Ok(rows.into_iter().map(row_to_workflow_instance).collect())
        })
    }

    fn try_claim_workflow_instance(&self, id: &str, owner: &str, ttl_secs: i64) -> Result<bool> {
        let id = id.to_string();
        let owner = owner.to_string();
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let changed = conn
                .execute(
                    "UPDATE workflow_instances
                     SET owner_instance = $2,
                         lease_expires_at = now() + make_interval(secs => $3::double precision)
                     WHERE id = $1
                       AND (owner_instance IS NULL
                            OR owner_instance = $2
                            OR lease_expires_at IS NULL
                            OR lease_expires_at < now())",
                    &[&id, &owner, &(ttl_secs as f64)],
                )
                .await?;
            Ok(changed == 1)
        })
    }

    fn renew_workflow_lease(&self, id: &str, owner: &str, ttl_secs: i64) -> Result<bool> {
        let id = id.to_string();
        let owner = owner.to_string();
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let changed = conn
                .execute(
                    "UPDATE workflow_instances
                     SET lease_expires_at = now() + make_interval(secs => $3::double precision)
                     WHERE id = $1 AND owner_instance = $2",
                    &[&id, &owner, &(ttl_secs as f64)],
                )
                .await?;
            Ok(changed == 1)
        })
    }

    fn release_workflow_instance(&self, id: &str, owner: &str) -> Result<()> {
        let id = id.to_string();
        let owner = owner.to_string();
        self.block_on(async move {
            let conn = self.pool.get().await?;
            conn.execute(
                "UPDATE workflow_instances
                 SET owner_instance = NULL, lease_expires_at = NULL
                 WHERE id = $1 AND owner_instance = $2",
                &[&id, &owner],
            )
            .await?;
            Ok(())
        })
    }

    fn save_workflow_instance_owned(&self, row: &WorkflowInstanceRow, owner: &str) -> Result<bool> {
        self.update_workflow_instance_where(row, "owner_instance = $14", owner.to_string())
    }

    fn save_workflow_instance_fenced(
        &self,
        row: &WorkflowInstanceRow,
        expected_version: i64,
    ) -> Result<bool> {
        self.update_workflow_instance_where(row, "version = $14", expected_version)
    }

    fn delete_workflow_instance(&self, id: &str) -> Result<bool> {
        let id = id.to_string();
        self.block_on(async move {
            let conn = self.pool.get().await?;
            Ok(conn
                .execute("DELETE FROM workflow_instances WHERE id = $1", &[&id])
                .await?
                > 0)
        })
    }

    fn identity_signals(&self, identity: &str) -> Result<crate::telemetry::IdentitySignals> {
        let identity = identity.to_string();
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let mut held = crate::telemetry::IdentitySignals::default();
            for source in &crate::telemetry::SIGNAL_SOURCES {
                let rows: i64 = conn
                    .query_one(&source.count_sql('$'), &[&identity])
                    .await?
                    .get(0);
                held = held.add(source.signal, usize::try_from(rows).unwrap_or_default());
            }
            Ok(held)
        })
    }

    fn identity_signal_holders(&self) -> Result<std::collections::BTreeMap<String, usize>> {
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let rows = conn
                .query(&crate::telemetry::signal_holders_sql(), &[])
                .await?;
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

    fn reassign_identity_signals(
        &self,
        from: &str,
        to: &str,
    ) -> Result<crate::telemetry::IdentitySignals> {
        self.change_identity_signals(|source| source.moved, from, Some(to))
    }

    fn forget_identity_signals(&self, identity: &str) -> Result<crate::telemetry::IdentitySignals> {
        self.change_identity_signals(|source| source.erased, identity, None)
    }

    fn reassign_identity_billing(&self, from: &str, to: &str) -> Result<usize> {
        let (from, to) = (from.to_string(), to.to_string());
        self.block_on(async move {
            let mut conn = self.pool.get().await?;
            let tx = conn.transaction().await?;
            let mut changed = 0;
            for names in crate::telemetry::BILLING_NAMES {
                changed += tx
                    .execute(&crate::telemetry::rename_sql(names, '$'), &[&from, &to])
                    .await?;
            }
            tx.commit().await?;
            Ok(usize::try_from(changed).unwrap_or_default())
        })
    }

    fn cancel_workflow_instance(
        &self,
        id: &str,
        status: &str,
        reason_key: &str,
        reason: &str,
    ) -> Result<()> {
        let id = id.to_string();
        let status = status.to_string();
        let reason_key = reason_key.to_string();
        let reason = reason.to_string();
        self.block_on(async move {
            let conn = self.pool.get().await?;
            conn.execute(
                "UPDATE workflow_instances
                 SET status = $2,
                     variables = jsonb_set(variables::jsonb, ARRAY[$3], to_jsonb($4::text))::text,
                     updated_at = now(),
                     version = version + 1
                 WHERE id = $1",
                &[&id, &status, &reason_key, &reason],
            )
            .await?;
            Ok(())
        })
    }

    fn list_workflow_instances(
        &self,
        status: Option<&str>,
        workflow_id: Option<&str>,
    ) -> Result<Vec<WorkflowInstanceRow>> {
        let status = status.map(str::to_string);
        let workflow_id = workflow_id.map(str::to_string);
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let rows = conn
                .query(
                    format!(
                        "SELECT {INSTANCE_COLUMNS} FROM workflow_instances
                         WHERE ($1::TEXT IS NULL OR status = $1)
                           AND ($2::TEXT IS NULL OR workflow_id = $2)
                         ORDER BY updated_at DESC"
                    )
                    .as_str(),
                    &[&status, &workflow_id],
                )
                .await?;
            Ok(rows.into_iter().map(row_to_workflow_instance).collect())
        })
    }

    fn list_workflow_instances_filtered(
        &self,
        filter: &InstanceFilter,
    ) -> Result<Vec<WorkflowInstanceRow>> {
        let filter = filter.clone();
        let limit = i64::try_from(filter.limit).unwrap_or(i64::MAX);
        let (agent, declaring) = filter
            .runs_as
            .clone()
            .map_or((None, Vec::new()), |runs_as| {
                (Some(runs_as.agent), runs_as.declaring)
            });
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let rows = conn
                .query(
                    format!(
                        "SELECT {INSTANCE_COLUMNS} FROM workflow_instances
                         WHERE ($1::TEXT[] IS NULL OR id = ANY($1))
                           AND ($2::TEXT[] IS NULL OR EXISTS (
                                 SELECT 1 FROM unnest($2::TEXT[]) AS wanted(id)
                                 WHERE workflow_id = wanted.id
                                    OR right(workflow_id, length(wanted.id) + 1) = '/' || wanted.id))
                           AND ($3::TEXT[] IS NULL OR status = ANY($3))
                           AND ($4::TIMESTAMPTZ IS NULL OR updated_at >= $4)
                           AND ($5::TEXT IS NULL OR agent_id = $5
                                OR (agent_id IS NULL AND workflow_id = ANY($6::TEXT[])))
                         ORDER BY updated_at DESC
                         LIMIT $7"
                    )
                    .as_str(),
                    &[
                        &filter.ids,
                        &filter.workflows,
                        &filter.statuses,
                        &filter.since,
                        &agent,
                        &declaring,
                        &limit,
                    ],
                )
                .await?;
            Ok(rows.into_iter().map(row_to_workflow_instance).collect())
        })
    }

    fn workflow_instances_after(
        &self,
        after: DateTime<Utc>,
        after_id: &str,
        limit: usize,
    ) -> Result<Vec<WorkflowInstanceRow>> {
        let after_id = after_id.to_string();
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let rows = conn
                .query(
                    format!(
                        "SELECT {INSTANCE_COLUMNS} FROM workflow_instances
                         WHERE (updated_at, id) > ($1, $2)
                         ORDER BY updated_at, id
                         LIMIT $3"
                    )
                    .as_str(),
                    &[&after, &after_id, &limit],
                )
                .await?;
            Ok(rows.into_iter().map(row_to_workflow_instance).collect())
        })
    }

    fn list_child_instances(&self, parent_id: &str) -> Result<Vec<WorkflowInstanceRow>> {
        let parent_id = parent_id.to_string();
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let rows = conn
                .query(
                    format!(
                        "SELECT {INSTANCE_COLUMNS} FROM workflow_instances
                         WHERE parent_instance_id = $1
                           AND status NOT IN ('completed', 'failed', 'exited', 'cancelled')
                         ORDER BY created_at ASC"
                    )
                    .as_str(),
                    &[&parent_id],
                )
                .await?;
            Ok(rows.into_iter().map(row_to_workflow_instance).collect())
        })
    }

    fn update_workflow_instance_status(
        &self,
        id: &str,
        status: &str,
        current_step_id: Option<&str>,
    ) -> Result<()> {
        let id = id.to_string();
        let status = status.to_string();
        let current_step_id = current_step_id.map(str::to_string);
        self.block_on(async move {
            let conn = self.pool.get().await?;
            conn.execute(
                "UPDATE workflow_instances
                 SET status = $1, current_step_id = $2, updated_at = $3, version = version + 1
                 WHERE id = $4",
                &[&status, &current_step_id, &Utc::now(), &id],
            )
            .await?;
            Ok(())
        })
    }

    fn find_instance_by_thread(
        &self,
        channel_id: &str,
        thread_id: &str,
        statuses: &[&str],
    ) -> Result<Option<WorkflowInstanceRow>> {
        if statuses.is_empty() {
            return Ok(None);
        }
        let channel_id = channel_id.to_string();
        let thread_id = thread_id.to_string();
        let statuses: Vec<String> = statuses.iter().map(|s| s.to_string()).collect();
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let row = conn
                .query_opt(
                    format!(
                        "SELECT {INSTANCE_COLUMNS} FROM workflow_instances
                         WHERE channel_id = $1 AND thread_id = $2 AND status = ANY($3)
                         ORDER BY updated_at DESC
                         LIMIT 1"
                    )
                    .as_str(),
                    &[&channel_id, &thread_id, &statuses],
                )
                .await?;
            Ok(row.map(row_to_workflow_instance))
        })
    }

    fn set_instance_thread(&self, id: &str, channel_id: &str, thread_id: &str) -> Result<()> {
        let id = id.to_string();
        let channel_id = channel_id.to_string();
        let thread_id = thread_id.to_string();
        self.block_on(async move {
            let conn = self.pool.get().await?;
            conn.execute(
                "UPDATE workflow_instances
                 SET channel_id = $1, thread_id = $2, version = version + 1
                 WHERE id = $3",
                &[&channel_id, &thread_id, &id],
            )
            .await?;
            Ok(())
        })
    }

    // ─── Activity history ───

    fn query_activities(&self, query: &ActivityQuery) -> Result<Vec<ActivityRecord>> {
        let agent_id = query.agent_id.clone();
        let start_time = query.start_time;
        let end_time = query.end_time;
        let verbose = query.verbose;
        self.block_on(async move {
            let conn = self.pool.get().await?;
            query_activities_postgres(&conn, agent_id.as_deref(), start_time, end_time, verbose)
                .await
        })
    }

    fn prune_activity(&self, before: DateTime<Utc>) -> Result<usize> {
        self.prune(
            "DELETE FROM task_telemetry WHERE id IN (
                 SELECT id FROM task_telemetry WHERE completed_at < $1
                 ORDER BY completed_at LIMIT $2 FOR UPDATE SKIP LOCKED)",
            before,
            None,
        )
    }

    fn prune_costs(&self, before: DateTime<Utc>) -> Result<usize> {
        self.prune(
            "DELETE FROM cost_records WHERE id IN (
                 SELECT id FROM cost_records
                 WHERE timestamp < $1 AND (task_id IS NULL OR NOT starts_with(task_id, $3))
                 ORDER BY timestamp LIMIT $2 FOR UPDATE SKIP LOCKED)",
            before,
            Some(crate::telemetry::JOB_TASK_PREFIX),
        )
    }

    // ─── Cost analytics row fetchers ───

    fn task_usage_rows(&self, from: DateTime<Utc>, to: DateTime<Utc>) -> Result<Vec<TaskUsageRow>> {
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let rows = conn
                .query(
                    "SELECT agent_id, model, input_tokens, output_tokens, cost_usd, rounds,
                            TO_CHAR(completed_at, 'YYYY-MM-DD') AS day,
                            COALESCE(provider, 'unknown')
                     FROM task_telemetry
                     WHERE completed_at >= $1 AND completed_at < $2
                     ORDER BY completed_at ASC",
                    &[&from, &to],
                )
                .await?;
            Ok(rows
                .into_iter()
                .map(|r| TaskUsageRow {
                    agent_id: r.get(0),
                    model: r.get(1),
                    input_tokens: r.get::<_, i64>(2) as u64,
                    output_tokens: r.get::<_, i64>(3) as u64,
                    cost_usd: r.get(4),
                    rounds: r.get::<_, i64>(5) as u32,
                    day: r.get(6),
                    provider: r.get(7),
                })
                .collect())
        })
    }

    fn daily_usage_rows(&self, since: DateTime<Utc>) -> Result<Vec<DailyUsageRow>> {
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let rows = conn
                .query(
                    "SELECT TO_CHAR(completed_at, 'YYYY-MM-DD') AS day,
                            COUNT(*)::BIGINT,
                            SUM(cost_usd)::FLOAT8,
                            SUM(rounds)::BIGINT
                     FROM task_telemetry
                     WHERE completed_at >= $1
                     GROUP BY day
                     ORDER BY day ASC",
                    &[&since],
                )
                .await?;
            Ok(rows
                .into_iter()
                .map(|r| DailyUsageRow {
                    day: r.get(0),
                    task_count: r.get::<_, i64>(1) as u64,
                    total_cost: r.get(2),
                    total_rounds: r.get::<_, i64>(3) as u64,
                })
                .collect())
        })
    }

    // ─── Security event audit log ───

    fn insert_security_event(&self, event: &SecurityEvent) -> Result<()> {
        let event = event.clone();
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let detail_str = serde_json::to_string(&event.detail)?;
            conn.execute(
                "INSERT INTO security_events
                 (timestamp, event_type, agent_id, session_id, severity, source, detail, content_hash)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
                &[
                    &Utc::now(),
                    &event.event_type,
                    &event.agent_id,
                    &event.session_id,
                    &event.severity,
                    &event.source,
                    &detail_str,
                    &event.content_hash,
                ],
            )
            .await?;
            Ok(())
        })
    }

    fn query_security_events(&self, filter: &SecurityEventFilter) -> Result<Vec<SecurityEventRow>> {
        let limit = filter.limit.unwrap_or(50) as i64;
        let since = parse_rfc3339_opt(filter.since.as_deref())?;
        let until = parse_rfc3339_opt(filter.until.as_deref())?;
        let event_type = filter.event_type.clone();
        let agent_id = filter.agent_id.clone();
        let severity = filter.severity.clone();
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let rows = conn
                .query(
                    "SELECT id, timestamp, event_type, agent_id, session_id,
                            severity, source, detail, content_hash
                     FROM security_events
                     WHERE ($1::TIMESTAMPTZ IS NULL OR timestamp >= $1)
                       AND ($2::TIMESTAMPTZ IS NULL OR timestamp <= $2)
                       AND ($3::TEXT IS NULL OR event_type = $3)
                       AND ($4::TEXT IS NULL OR agent_id = $4)
                       AND ($5::TEXT IS NULL OR severity = $5)
                     ORDER BY id DESC
                     LIMIT $6",
                    &[&since, &until, &event_type, &agent_id, &severity, &limit],
                )
                .await?;
            Ok(rows
                .into_iter()
                .map(|r| {
                    let detail_str: String = r.get(7);
                    let detail: serde_json::Value =
                        serde_json::from_str(&detail_str).unwrap_or(serde_json::Value::Null);
                    SecurityEventRow {
                        id: r.get(0),
                        timestamp: fmt_rfc3339(r.get::<_, DateTime<Utc>>(1)),
                        event_type: r.get(2),
                        agent_id: r.get(3),
                        session_id: r.get(4),
                        severity: r.get(5),
                        source: r.get(6),
                        detail,
                        content_hash: r.get(8),
                    }
                })
                .collect())
        })
    }

    fn security_event_stats(&self, since: &str) -> Result<SecurityStats> {
        let since_dt = parse_rfc3339(since)?;
        let since_owned = since.to_string();
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let total_row = conn
                .query_one(
                    "SELECT COUNT(*)::BIGINT FROM security_events WHERE timestamp >= $1",
                    &[&since_dt],
                )
                .await?;
            let total_events = total_row.get::<_, i64>(0) as u64;

            let by_type_rows = conn
                .query(
                    "SELECT event_type, COUNT(*)::BIGINT FROM security_events
                     WHERE timestamp >= $1 GROUP BY event_type",
                    &[&since_dt],
                )
                .await?;
            let by_type: HashMap<String, u64> = by_type_rows
                .into_iter()
                .map(|r| (r.get::<_, String>(0), r.get::<_, i64>(1) as u64))
                .collect();

            let by_severity_rows = conn
                .query(
                    "SELECT severity, COUNT(*)::BIGINT FROM security_events
                     WHERE timestamp >= $1 GROUP BY severity",
                    &[&since_dt],
                )
                .await?;
            let by_severity: HashMap<String, u64> = by_severity_rows
                .into_iter()
                .map(|r| (r.get::<_, String>(0), r.get::<_, i64>(1) as u64))
                .collect();

            Ok(SecurityStats {
                total_events,
                by_type,
                by_severity,
                since: since_owned,
            })
        })
    }

    fn cleanup_security_events(&self, before: &str) -> Result<usize> {
        let before_dt = parse_rfc3339(before)?;
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let deleted = conn
                .execute(
                    "DELETE FROM security_events WHERE timestamp < $1",
                    &[&before_dt],
                )
                .await?;
            Ok(deleted as usize)
        })
    }

    // ─── Per-identity stats slices ───
    fn cost_by_identity(
        &self,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<Vec<(String, f64)>> {
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let rows = conn
                .query(
                    "SELECT COALESCE(identity_id, 'unknown'), SUM(cost_usd)
                     FROM cost_records
                     WHERE timestamp >= $1 AND timestamp <= $2
                     GROUP BY COALESCE(identity_id, 'unknown')
                     ORDER BY SUM(cost_usd) DESC",
                    &[&from, &to],
                )
                .await?;
            Ok(rows
                .into_iter()
                .map(|r| (r.get::<_, String>(0), r.get::<_, f64>(1)))
                .collect())
        })
    }

    fn metered_spend(&self, from: DateTime<Utc>, to: DateTime<Utc>) -> Result<Vec<MeteredSpend>> {
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let rows = conn
                .query(
                    "SELECT agent_id, identity_id, SUM(cost_usd) FROM cost_records
                     WHERE timestamp >= $1 AND timestamp <= $2 AND billing = $3
                     GROUP BY agent_id, identity_id",
                    &[&from, &to, &Billing::Metered.as_str()],
                )
                .await?;
            Ok(rows
                .into_iter()
                .map(|r| MeteredSpend {
                    agent_id: r.get(0),
                    identity_id: r.get(1),
                    cost_usd: r.get(2),
                })
                .collect())
        })
    }

    fn metered_cost_by_task_prefix(&self, prefix: &str) -> Result<f64> {
        // Range predicate over `COLLATE "C"` so the btree serves the prefix
        // scan regardless of database collation (a plain LIKE on a non-C
        // collation cannot use the index). Prefixes are internal
        // (`scheduler:{uuid}:`) — no wildcard chars involved.
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let row = conn
                .query_one(
                    "SELECT COALESCE(SUM(cost_usd), 0.0) FROM cost_records
                     WHERE task_id COLLATE \"C\" >= $1 AND task_id COLLATE \"C\" < $2
                       AND billing = $3",
                    &[
                        &prefix,
                        &format!("{prefix}\u{10FFFF}"),
                        &Billing::Metered.as_str(),
                    ],
                )
                .await?;
            Ok(row.get::<_, f64>(0))
        })
    }

    fn cost_token_history_by_identity_day(
        &self,
        identity_id: &str,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<Vec<TokenHistoryPoint>> {
        let identity_id = identity_id.to_string();
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let rows = conn
                .query(
                    "SELECT (date_trunc('day', timestamp AT TIME ZONE 'UTC') AT TIME ZONE 'UTC') AS day,
                            COALESCE(SUM(input_tokens), 0)::BIGINT,
                            COALESCE(SUM(output_tokens), 0)::BIGINT,
                            COALESCE(SUM(cache_creation_input_tokens), 0)::BIGINT,
                            COALESCE(SUM(cache_read_input_tokens), 0)::BIGINT
                     FROM cost_records
                     WHERE identity_id = $1 AND timestamp >= $2 AND timestamp <= $3
                     GROUP BY day ORDER BY day ASC",
                    &[&identity_id, &from, &to],
                )
                .await?;
            Ok(rows.into_iter().map(token_history_point_from_row).collect())
        })
    }

    fn cost_token_history_by_agent_day(
        &self,
        agent_id: &str,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<Vec<TokenHistoryPoint>> {
        let agent_id = agent_id.to_string();
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let rows = conn
                .query(
                    "SELECT (date_trunc('day', timestamp AT TIME ZONE 'UTC') AT TIME ZONE 'UTC') AS day,
                            COALESCE(SUM(input_tokens), 0)::BIGINT,
                            COALESCE(SUM(output_tokens), 0)::BIGINT,
                            COALESCE(SUM(cache_creation_input_tokens), 0)::BIGINT,
                            COALESCE(SUM(cache_read_input_tokens), 0)::BIGINT
                     FROM cost_records
                     WHERE agent_id = $1 AND timestamp >= $2 AND timestamp <= $3
                     GROUP BY day ORDER BY day ASC",
                    &[&agent_id, &from, &to],
                )
                .await?;
            Ok(rows.into_iter().map(token_history_point_from_row).collect())
        })
    }

    fn get_identity_usage_summary(
        &self,
        identity_id: &str,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
        group_by: &str,
    ) -> Result<Vec<AgentUsageSummary>> {
        let group_col = match group_by {
            "model" => "model",
            "task_type" => "COALESCE(task_type, 'unknown')",
            "provider" => "provider",
            "agent" => "agent_id",
            other => return Err(eyre::eyre!("unsupported group_by '{other}'")),
        };
        let identity_id = identity_id.to_string();
        let sql = format!(
            "SELECT {group_col} as group_key,
                    COUNT(*)::BIGINT,
                    SUM(input_tokens)::BIGINT,
                    SUM(output_tokens)::BIGINT,
                    SUM(cost_usd)::FLOAT8,
                    AVG(rounds)::FLOAT8,
                    AVG(duration_ms)::FLOAT8,
                    AVG(CASE WHEN success THEN 1.0 ELSE 0.0 END)::FLOAT8,
                    SUM(cache_creation_input_tokens)::BIGINT,
                    SUM(cache_read_input_tokens)::BIGINT
             FROM task_telemetry
             WHERE identity_id = $1 AND completed_at >= $2 AND completed_at <= $3
             GROUP BY {group_col}
             ORDER BY SUM(cost_usd) DESC"
        );
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let rows = conn.query(&sql, &[&identity_id, &from, &to]).await?;
            Ok(rows.into_iter().map(row_to_agent_usage).collect())
        })
    }

    fn compare_models_for_identity(
        &self,
        identity_id: &str,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<Vec<ModelComparison>> {
        self.compare_models_filtered("identity_id", identity_id.to_string(), from, to)
    }

    fn tool_breakdown_for_identity(
        &self,
        identity_id: &str,
        from: DateTime<Utc>,
        limit: usize,
    ) -> Result<Vec<ToolCostEntry>> {
        self.tool_breakdown_filtered("identity_id", identity_id.to_string(), from, limit)
    }

    fn agent_identity_breakdown(
        &self,
        agent_id: &str,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<Vec<(String, f64)>> {
        let agent_id = agent_id.to_string();
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let rows = conn
                .query(
                    "SELECT COALESCE(identity_id, 'unknown'), SUM(cost_usd)
                     FROM task_telemetry
                     WHERE agent_id = $1 AND completed_at >= $2 AND completed_at <= $3
                     GROUP BY COALESCE(identity_id, 'unknown')
                     ORDER BY SUM(cost_usd) DESC",
                    &[&agent_id, &from, &to],
                )
                .await?;
            Ok(rows
                .into_iter()
                .map(|r| (r.get::<_, String>(0), r.get::<_, f64>(1)))
                .collect())
        })
    }

    fn cost_by_task_type(
        &self,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<Vec<(String, f64)>> {
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let rows = conn
                .query(
                    "SELECT COALESCE(task_type, 'unknown'), SUM(cost_usd)
                     FROM cost_records
                     WHERE timestamp >= $1 AND timestamp <= $2
                     GROUP BY COALESCE(task_type, 'unknown')
                     ORDER BY SUM(cost_usd) DESC",
                    &[&from, &to],
                )
                .await?;
            Ok(rows
                .into_iter()
                .map(|r| (r.get::<_, String>(0), r.get::<_, f64>(1)))
                .collect())
        })
    }

    fn task_history_paginated(&self, filter: &TaskHistoryFilter) -> Result<TaskHistoryPage> {
        let limit = crate::pagination::clamp_limit(Some(filter.limit));
        let identity_id = filter.identity_id.clone();
        let agent_id = filter.agent_id.clone();
        let task_type = filter.task_type.clone();
        let model = filter.model.clone();
        let since = filter.since;
        let until = filter.until;
        let cursor = filter.cursor;

        // Build the SQL + bind list in lock-step. Each `push_filter` call
        // either appends a `WHERE` fragment + placeholder (when `Some`) or
        // skips. Postgres row-value comparison `(a, b) < ($n, $m)` is
        // exactly the cursor semantics we want.
        let mut clauses: Vec<String> = Vec::new();
        let mut next_idx: usize = 1;
        let mut emit = |sql_template: &str| -> String {
            let placeholder = format!("${next_idx}");
            next_idx += 1;
            sql_template.replace("$$", &placeholder)
        };
        if identity_id.is_some() {
            clauses.push(emit("identity_id = $$"));
        }
        if agent_id.is_some() {
            clauses.push(emit("agent_id = $$"));
        }
        if task_type.is_some() {
            clauses.push(emit("task_type = $$"));
        }
        if model.is_some() {
            clauses.push(emit("model = $$"));
        }
        if since.is_some() {
            clauses.push(emit("completed_at >= $$"));
        }
        if until.is_some() {
            clauses.push(emit("completed_at <= $$"));
        }
        if cursor.is_some() {
            let ts_ph = format!("${next_idx}");
            next_idx += 1;
            let id_ph = format!("${next_idx}");
            next_idx += 1;
            clauses.push(format!("(completed_at, id) < ({ts_ph}, {id_ph})"));
        }
        let where_sql = if clauses.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", clauses.join(" AND "))
        };
        let limit_placeholder = format!("${next_idx}");
        let fetch = (limit + 1) as i64;
        let sql = format!(
            "SELECT {TASK_HISTORY_COLUMNS}
             FROM task_telemetry
             {where_sql}
             ORDER BY completed_at DESC, id DESC
             LIMIT {limit_placeholder}"
        );

        self.block_on(async move {
            let conn = self.pool.get().await?;
            let mut binds: Vec<&(dyn tokio_postgres::types::ToSql + Sync)> = Vec::new();
            if let Some(ref v) = identity_id {
                binds.push(v);
            }
            if let Some(ref v) = agent_id {
                binds.push(v);
            }
            if let Some(ref v) = task_type {
                binds.push(v);
            }
            if let Some(ref v) = model {
                binds.push(v);
            }
            if let Some(ref v) = since {
                binds.push(v);
            }
            if let Some(ref v) = until {
                binds.push(v);
            }
            // Cursor binds need stable storage local to this scope.
            let cursor_pair = cursor;
            if let Some((ref ts, ref id)) = cursor_pair {
                binds.push(ts);
                binds.push(id);
            }
            binds.push(&fetch);

            let rows = conn.query(&sql, &binds).await?;
            let mut entries: Vec<TaskHistoryEntry> =
                rows.into_iter().map(row_to_task_history).collect();
            let next_cursor = if entries.len() > limit {
                entries.truncate(limit);
                entries.last().map(|e| {
                    let ts = chrono::DateTime::parse_from_rfc3339(&e.completed_at)
                        .map(|t| t.with_timezone(&Utc))
                        .unwrap_or_else(|_| Utc::now());
                    crate::pagination::encode_cursor(ts, e.id)
                })
            } else {
                None
            };
            Ok(TaskHistoryPage {
                entries,
                next_cursor,
            })
        })
    }

    fn get_task_trace(&self, task_id: &str) -> Result<Option<TaskTrace>> {
        let task_id = task_id.to_string();
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let task_row = conn
                .query_opt(
                    "SELECT id, agent_id, model, provider, input_tokens, output_tokens,
                            cache_creation_input_tokens, cache_read_input_tokens, cost_usd,
                            rounds, duration_ms, tool_invocation_count, success, error_type,
                            retry_count, task_type, task_id, started_at, completed_at,
                            forecast_cost_usd, forecast_rounds, forecast_confidence,
                            team_id, session_id, model_switches, ticket_id, identity_id,
                            description
                     FROM task_telemetry WHERE task_id = $1
                     ORDER BY id DESC LIMIT 1",
                    &[&task_id],
                )
                .await?;
            let Some(r) = task_row else {
                return Ok(None);
            };
            let task_pk: i64 = r.get(0);
            let task = TaskTelemetryRecord {
                agent_id: r.get(1),
                model: r.get(2),
                provider: r.get(3),
                input_tokens: r.get::<_, i64>(4) as u64,
                output_tokens: r.get::<_, i64>(5) as u64,
                cache_creation_input_tokens: r.get::<_, i64>(6) as u64,
                cache_read_input_tokens: r.get::<_, i64>(7) as u64,
                cost_usd: r.get(8),
                rounds: r.get::<_, i64>(9) as u32,
                duration_ms: r.get::<_, i64>(10) as u64,
                tool_invocation_count: r.get::<_, i64>(11) as u32,
                success: r.get(12),
                error_type: r.get(13),
                retry_count: r.get::<_, i64>(14) as u32,
                task_type: r.get(15),
                task_id: r.get(16),
                started_at: fmt_rfc3339(r.get::<_, DateTime<Utc>>(17)),
                completed_at: fmt_rfc3339(r.get::<_, DateTime<Utc>>(18)),
                forecast_cost_usd: r.get(19),
                forecast_rounds: r.get::<_, Option<i64>>(20).map(|v| v as u32),
                forecast_confidence: r.get(21),
                team_id: r.get(22),
                session_id: r.get(23),
                model_switches: r.get(24),
                ticket_id: r.get(25),
                identity_id: r.get(26),
                description: r.get(27),
            };

            let round_rows = conn
                .query(
                    "SELECT round_number, input_tokens, output_tokens, latency_ms,
                            cache_creation_tokens, cache_read_tokens,
                            prompt_size_bytes, response_size_bytes, tool_count, stop_reason
                     FROM round_telemetry WHERE task_telemetry_id = $1
                     ORDER BY round_number ASC",
                    &[&task_pk],
                )
                .await?;
            let rounds: Vec<RoundTelemetryRecord> = round_rows
                .into_iter()
                .map(|r| RoundTelemetryRecord {
                    round_number: r.get::<_, i64>(0) as u32,
                    input_tokens: r.get::<_, i64>(1) as u64,
                    output_tokens: r.get::<_, i64>(2) as u64,
                    latency_ms: r.get::<_, i64>(3) as u64,
                    cache_creation_tokens: r.get::<_, i64>(4) as u64,
                    cache_read_tokens: r.get::<_, i64>(5) as u64,
                    prompt_size_bytes: r.get::<_, i64>(6) as u64,
                    response_size_bytes: r.get::<_, i64>(7) as u64,
                    tool_count: r.get::<_, i64>(8) as u32,
                    stop_reason: r.get(9),
                })
                .collect();

            let tool_rows = conn
                .query(
                    "SELECT round_number, tool_name, success, duration_ms,
                            policy_decision, input_size_bytes, output_size_bytes, denial_reason
                     FROM tool_telemetry WHERE task_telemetry_id = $1
                     ORDER BY round_number ASC, id ASC",
                    &[&task_pk],
                )
                .await?;
            let tools: Vec<ToolTelemetryRecord> = tool_rows
                .into_iter()
                .map(|r| ToolTelemetryRecord {
                    round_number: r.get::<_, i64>(0) as u32,
                    tool_name: r.get(1),
                    success: r.get(2),
                    duration_ms: r.get::<_, i64>(3) as u64,
                    policy_decision: r.get(4),
                    input_size_bytes: r.get::<_, i64>(5) as u64,
                    output_size_bytes: r.get::<_, i64>(6) as u64,
                    denial_reason: r.get(7),
                })
                .collect();

            let cost_rows = conn
                .query(
                    "SELECT timestamp, agent_id, model, input_tokens, output_tokens,
                            cache_creation_input_tokens, cache_read_input_tokens, cost_usd,
                            team_id, task_id, task_type, ticket_id, identity_id,
                            origin, billing, payer, credential_tier
                     FROM cost_records WHERE task_id = $1 ORDER BY timestamp ASC",
                    &[&task_id],
                )
                .await?;
            let cost_records: Vec<CostRecordView> = cost_rows
                .into_iter()
                .map(|r| CostRecordView {
                    timestamp: fmt_rfc3339(r.get::<_, DateTime<Utc>>(0)),
                    agent_id: r.get(1),
                    model: r.get(2),
                    input_tokens: r.get::<_, i64>(3) as u64,
                    output_tokens: r.get::<_, i64>(4) as u64,
                    cache_creation_input_tokens: r.get::<_, i64>(5) as u64,
                    cache_read_input_tokens: r.get::<_, i64>(6) as u64,
                    cost_usd: r.get(7),
                    team_id: r.get(8),
                    task_id: r.get(9),
                    task_type: r.get(10),
                    ticket_id: r.get(11),
                    identity_id: r.get(12),
                    origin: r.get(13),
                    billing: Billing::from_column(r.get(14)),
                    payer: r.get(15),
                    credential_tier: tier_column(r.get(16)),
                })
                .collect();

            Ok(Some(TaskTrace {
                task,
                rounds,
                tools,
                cost_records,
            }))
        })
    }

    // ─── Global (cross-agent) variants ───
    fn tool_breakdown_global(
        &self,
        from: DateTime<Utc>,
        limit: usize,
    ) -> Result<Vec<ToolCostEntry>> {
        let limit = limit as i64;
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let rows = conn
                .query(
                    "SELECT tt.tool_name,
                            COUNT(*)::BIGINT,
                            SUM(tt.duration_ms)::BIGINT,
                            AVG(tt.duration_ms)::FLOAT8,
                            SUM(tt.input_size_bytes)::BIGINT,
                            SUM(tt.output_size_bytes)::BIGINT,
                            SUM(CASE WHEN tt.success THEN 1 ELSE 0 END)::BIGINT
                     FROM tool_telemetry tt
                     JOIN task_telemetry t ON tt.task_telemetry_id = t.id
                     WHERE t.completed_at >= $1
                     GROUP BY tt.tool_name
                     ORDER BY COUNT(*) DESC
                     LIMIT $2",
                    &[&from, &limit],
                )
                .await?;
            Ok(rows.into_iter().map(row_to_tool_cost_entry).collect())
        })
    }

    fn efficiency_trends_global(
        &self,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<Vec<EfficiencyTrendPoint>> {
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let rows = conn
                .query(
                    "SELECT TO_CHAR(completed_at, 'YYYY-MM-DD') AS bucket,
                            COUNT(*)::BIGINT,
                            AVG(input_tokens)::FLOAT8,
                            AVG(output_tokens)::FLOAT8,
                            AVG(cost_usd)::FLOAT8,
                            AVG(rounds)::FLOAT8,
                            AVG(duration_ms)::FLOAT8,
                            AVG(CASE WHEN success THEN 1.0 ELSE 0.0 END)::FLOAT8,
                            AVG(cache_read_input_tokens)::FLOAT8
                     FROM task_telemetry
                     WHERE completed_at >= $1 AND completed_at <= $2
                     GROUP BY bucket
                     ORDER BY bucket ASC",
                    &[&from, &to],
                )
                .await?;
            Ok(rows
                .into_iter()
                .map(|r| EfficiencyTrendPoint {
                    bucket: r.get(0),
                    total_tasks: r.get::<_, i64>(1) as u64,
                    avg_input_tokens: r.get(2),
                    avg_output_tokens: r.get(3),
                    avg_cost: r.get(4),
                    avg_rounds: r.get(5),
                    avg_duration_ms: r.get(6),
                    success_rate: r.get(7),
                    avg_cache_read_tokens: r.get(8),
                })
                .collect())
        })
    }

    fn round_outliers_global(
        &self,
        since: DateTime<Utc>,
        threshold_multiplier: f64,
        limit: usize,
    ) -> Result<(RoundStats, Vec<TaskHistoryEntry>)> {
        self.round_outliers_filtered(None, since, threshold_multiplier, limit)
    }
}

impl PostgresTelemetryStore {
    fn compare_models_filtered(
        &self,
        filter_col: &'static str,
        filter_val: String,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<Vec<ModelComparison>> {
        let sql = format!(
            "SELECT model,
                    COUNT(*)::BIGINT,
                    AVG(input_tokens)::FLOAT8,
                    AVG(output_tokens)::FLOAT8,
                    AVG(cost_usd)::FLOAT8,
                    AVG(rounds)::FLOAT8,
                    AVG(duration_ms)::FLOAT8,
                    AVG(CASE WHEN success THEN 1.0 ELSE 0.0 END)::FLOAT8,
                    SUM(cost_usd)::FLOAT8,
                    AVG(cache_read_input_tokens)::FLOAT8,
                    SUM(cache_read_input_tokens)::FLOAT8
             FROM task_telemetry
             WHERE {filter_col} = $1 AND completed_at >= $2 AND completed_at <= $3
             GROUP BY model
             ORDER BY SUM(cost_usd) DESC"
        );
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let rows = conn.query(&sql, &[&filter_val, &from, &to]).await?;
            Ok(rows.into_iter().map(row_to_model_comparison).collect())
        })
    }

    fn tool_breakdown_filtered(
        &self,
        filter_col: &'static str,
        filter_val: String,
        from: DateTime<Utc>,
        limit: usize,
    ) -> Result<Vec<ToolCostEntry>> {
        let limit = limit as i64;
        let sql = format!(
            "SELECT tt.tool_name,
                    COUNT(*)::BIGINT,
                    SUM(tt.duration_ms)::BIGINT,
                    AVG(tt.duration_ms)::FLOAT8,
                    SUM(tt.input_size_bytes)::BIGINT,
                    SUM(tt.output_size_bytes)::BIGINT,
                    SUM(CASE WHEN tt.success THEN 1 ELSE 0 END)::BIGINT
             FROM tool_telemetry tt
             JOIN task_telemetry t ON tt.task_telemetry_id = t.id
             WHERE t.{filter_col} = $1 AND t.completed_at >= $2
             GROUP BY tt.tool_name
             ORDER BY COUNT(*) DESC
             LIMIT $3"
        );
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let rows = conn.query(&sql, &[&filter_val, &from, &limit]).await?;
            Ok(rows.into_iter().map(row_to_tool_cost_entry).collect())
        })
    }

    fn round_outliers_filtered(
        &self,
        agent_id: Option<String>,
        since: DateTime<Utc>,
        threshold_multiplier: f64,
        limit: usize,
    ) -> Result<(RoundStats, Vec<TaskHistoryEntry>)> {
        let limit = limit as i64;
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let rounds_rows = match agent_id {
                Some(ref id) => {
                    conn.query(
                        "SELECT rounds FROM task_telemetry
                         WHERE agent_id = $1 AND completed_at >= $2
                         ORDER BY rounds ASC",
                        &[id, &since],
                    )
                    .await?
                }
                None => {
                    conn.query(
                        "SELECT rounds FROM task_telemetry
                         WHERE completed_at >= $1
                         ORDER BY rounds ASC",
                        &[&since],
                    )
                    .await?
                }
            };
            let round_counts: Vec<u32> = rounds_rows
                .into_iter()
                .map(|r| r.get::<_, i64>(0) as u32)
                .collect();
            let Some(stats) = crate::persistence::round_stats_from(&round_counts) else {
                return Ok((RoundStats::default(), Vec::new()));
            };
            let threshold_rounds = (stats.mean + threshold_multiplier * stats.stddev) as i64;
            let outlier_rows = match agent_id {
                Some(ref id) => {
                    let sql = format!(
                        "SELECT {TASK_HISTORY_COLUMNS}
                         FROM task_telemetry
                         WHERE agent_id = $1 AND completed_at >= $2 AND rounds > $3
                         ORDER BY rounds DESC
                         LIMIT $4"
                    );
                    conn.query(&sql, &[id, &since, &threshold_rounds, &limit])
                        .await?
                }
                None => {
                    let sql = format!(
                        "SELECT {TASK_HISTORY_COLUMNS}
                         FROM task_telemetry
                         WHERE completed_at >= $1 AND rounds > $2
                         ORDER BY rounds DESC
                         LIMIT $3"
                    );
                    conn.query(&sql, &[&since, &threshold_rounds, &limit])
                        .await?
                }
            };
            let outliers = outlier_rows.into_iter().map(row_to_task_history).collect();
            Ok((stats, outliers))
        })
    }
}

// ─── Row mapping helpers ───────────────────────────────────────────────

fn row_to_task(r: tokio_postgres::Row) -> TaskRow {
    TaskRow {
        id: r.get(0),
        title: r.get(1),
        description: r.get(2),
        status: r.get(3),
        priority: r.get(4),
        assignee: r.get(5),
        team_id: r.get(6),
        labels: r.get(7),
        parent_id: r.get(8),
        result_json: r.get(9),
        created_at: fmt_rfc3339(r.get::<_, DateTime<Utc>>(10)),
        updated_at: fmt_rfc3339(r.get::<_, DateTime<Utc>>(11)),
        started_at: fmt_rfc3339_opt(r.get::<_, Option<DateTime<Utc>>>(12)),
        completed_at: fmt_rfc3339_opt(r.get::<_, Option<DateTime<Utc>>>(13)),
    }
}

fn row_to_audit(r: tokio_postgres::Row) -> AuditEntry {
    AuditEntry {
        timestamp: fmt_rfc3339(r.get::<_, DateTime<Utc>>(0)),
        actor: r.get(1),
        action: r.get(2),
        resource: r.get(3),
        detail: r.get(4),
        outcome: audit_outcome_from_str(r.get::<_, &str>(5)),
    }
}

fn row_to_agent_usage(r: tokio_postgres::Row) -> AgentUsageSummary {
    AgentUsageSummary {
        group_key: r.get(0),
        total_tasks: r.get::<_, i64>(1) as u64,
        total_input_tokens: r.get::<_, i64>(2) as u64,
        total_output_tokens: r.get::<_, i64>(3) as u64,
        total_cost: r.get(4),
        avg_rounds: r.get(5),
        avg_duration_ms: r.get(6),
        success_rate: r.get(7),
        total_cache_created_tokens: r.get::<_, i64>(8) as u64,
        total_cache_read_tokens: r.get::<_, i64>(9) as u64,
    }
}

fn token_history_point_from_row(r: tokio_postgres::Row) -> TokenHistoryPoint {
    TokenHistoryPoint {
        bucket_start: r.get::<_, DateTime<Utc>>(0),
        input_tokens: r.get::<_, i64>(1) as u64,
        output_tokens: r.get::<_, i64>(2) as u64,
        cache_creation_input_tokens: r.get::<_, i64>(3) as u64,
        cache_read_input_tokens: r.get::<_, i64>(4) as u64,
    }
}

fn row_to_model_comparison(r: tokio_postgres::Row) -> ModelComparison {
    ModelComparison {
        model: r.get(0),
        total_tasks: r.get::<_, i64>(1) as u64,
        avg_input_tokens: r.get(2),
        avg_output_tokens: r.get(3),
        avg_cost: r.get(4),
        avg_rounds: r.get(5),
        avg_latency_ms: r.get(6),
        success_rate: r.get(7),
        total_cost: r.get(8),
        avg_cache_read_tokens: r.get(9),
        total_cache_read_tokens: r.get::<_, f64>(10),
    }
}

fn row_to_tool_cost_entry(r: tokio_postgres::Row) -> ToolCostEntry {
    let invocations = r.get::<_, i64>(1) as u64;
    let successes = r.get::<_, i64>(6) as u64;
    let total_output_bytes = r.get::<_, i64>(5) as u64;
    ToolCostEntry {
        tool_name: r.get(0),
        invocations,
        total_duration_ms: r.get::<_, i64>(2) as u64,
        avg_duration_ms: r.get(3),
        total_input_bytes: r.get::<_, i64>(4) as u64,
        total_output_bytes,
        successes,
        success_rate_pct: if invocations > 0 {
            successes as f64 / invocations as f64 * 100.0
        } else {
            0.0
        },
        estimated_token_cost: total_output_bytes / 4,
    }
}

/// Projection used by every postgres `TaskHistoryEntry` query so the
/// `r.get(...)` indices in [`row_to_task_history`] stay in sync. Includes
/// per-row `round_count` and `tool_call_count` subqueries — same shape as
/// the SQLite [`crate::persistence::TASK_HISTORY_COLUMNS`].
const TASK_HISTORY_COLUMNS: &str = "
    id, agent_id, model, task_type, task_id,
    input_tokens, output_tokens,
    cache_creation_input_tokens, cache_read_input_tokens,
    cost_usd, rounds, duration_ms, success, completed_at,
    model_switches, ticket_id, identity_id,
    (SELECT COUNT(*) FROM round_telemetry r WHERE r.task_telemetry_id = task_telemetry.id)::BIGINT AS round_count,
    (SELECT COUNT(*) FROM tool_telemetry t  WHERE t.task_telemetry_id = task_telemetry.id)::BIGINT AS tool_call_count
";

fn row_to_task_history(r: tokio_postgres::Row) -> TaskHistoryEntry {
    TaskHistoryEntry {
        id: r.get(0),
        agent_id: r.get(1),
        model: r.get(2),
        task_type: r.get(3),
        task_id: r.get(4),
        input_tokens: r.get::<_, i64>(5) as u64,
        output_tokens: r.get::<_, i64>(6) as u64,
        cache_creation_input_tokens: r.get::<_, i64>(7) as u64,
        cache_read_input_tokens: r.get::<_, i64>(8) as u64,
        cost_usd: r.get(9),
        rounds: r.get::<_, i64>(10) as u32,
        duration_ms: r.get::<_, i64>(11) as u64,
        success: r.get(12),
        completed_at: fmt_rfc3339(r.get::<_, DateTime<Utc>>(13)),
        model_switches: r.get(14),
        ticket_id: r.get(15),
        identity_id: r.get(16),
        round_count: r.get::<_, i64>(17) as u32,
        tool_call_count: r.get::<_, i64>(18) as u32,
    }
}

fn row_to_model_pricing(r: tokio_postgres::Row) -> StoredModelPricing {
    StoredModelPricing {
        model: r.get(0),
        provider: r.get(1),
        input_per_million: r.get(2),
        output_per_million: r.get(3),
        source: r.get(4),
        fetched_at: fmt_rfc3339(r.get::<_, DateTime<Utc>>(5)),
        updated_at: fmt_rfc3339(r.get::<_, DateTime<Utc>>(6)),
    }
}

fn row_to_policy_proposal(r: tokio_postgres::Row) -> PolicyProposalRow {
    PolicyProposalRow {
        id: r.get(0),
        agent_id: r.get(1),
        proposed_rule: r.get(2),
        justification: r.get(3),
        status: r.get(4),
        reviewed_by: r.get(5),
        review_note: r.get(6),
        created_at: fmt_rfc3339(r.get::<_, DateTime<Utc>>(7)),
        reviewed_at: fmt_rfc3339_opt(r.get::<_, Option<DateTime<Utc>>>(8)),
    }
}

/// The columns [`row_to_workflow_instance`] reads, in its order.
const INSTANCE_COLUMNS: &str = "id, workflow_id, status, current_step_id, step_history,
     variables, trigger_json, agent_id, created_at, updated_at,
     parent_instance_id, channel_id, thread_id, owner_instance, lease_expires_at, version";

fn row_to_workflow_instance(r: tokio_postgres::Row) -> WorkflowInstanceRow {
    WorkflowInstanceRow {
        id: r.get(0),
        workflow_id: r.get(1),
        status: r.get(2),
        current_step_id: r.get(3),
        step_history: r.get(4),
        variables: r.get(5),
        trigger_json: r.get(6),
        agent_id: r.get(7),
        created_at: fmt_rfc3339(r.get::<_, DateTime<Utc>>(8)),
        updated_at: fmt_rfc3339(r.get::<_, DateTime<Utc>>(9)),
        parent_instance_id: r.get(10),
        channel_id: r.get(11),
        thread_id: r.get(12),
        owner_instance: r.get(13),
        lease_expires_at: fmt_rfc3339_opt(r.get::<_, Option<DateTime<Utc>>>(14)),
        version: r.get(15),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::admin::AuditOutcome;
    use crate::persistence::{CostRecord, StoredModelPricing, TaskRow, TaskTelemetryRecord};
    use crate::telemetry::TelemetryStore;
    use workforce_postgres::test_support::{now_micros, with_test_pool};

    /// Re-exported for `telemetry_migrate::tests`, which drives the
    /// migration runner directly and so needs the bare pool.
    pub(crate) async fn with_embedded_pg_pool<F, Fut>(test: F)
    where
        F: FnOnce(workforce_postgres::PostgresPool) -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        with_test_pool(PostgresTelemetryStore::build_pool, test).await;
    }

    async fn with_embedded_pg<F, Fut>(test: F)
    where
        F: FnOnce(Arc<PostgresTelemetryStore>) -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        with_embedded_pg_pool(|pool| async move {
            let store = PostgresTelemetryStore::with_pool(pool)
                .await
                .expect("construct store (runs migrations)");
            test(store).await;
        })
        .await;
    }

    /// The telemetry migrations up to and including `.0`.
    struct MigrationsUpTo(u32);

    #[async_trait::async_trait]
    impl MigrationRun for MigrationsUpTo {
        async fn run(&self, client: &mut tokio_postgres::Client) -> Result<(), PostgresError> {
            embedded::migrations::runner()
                .set_target(refinery::Target::Version(self.0))
                .run_async(client)
                .await
                .map_err(PostgresError::from)?;
            Ok(())
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn pg_record_cost_keeps_one_row_per_ledger_key() {
        with_embedded_pg(|store| async move {
            let row = |ledger_key: Option<&str>| CostRecord {
                agent_id: "allan".into(),
                model: "claude-code:sonnet".into(),
                cost_usd: 1.0,
                ledger_key: ledger_key.map(String::from),
                ..Default::default()
            };
            assert!(store.record_cost(&row(Some("r1.0.0"))).unwrap());
            assert!(!store.record_cost(&row(Some("r1.0.0"))).unwrap());
            assert!(store.record_cost(&row(Some("r1.0.1"))).unwrap());
            assert!(store.record_cost(&row(None)).unwrap());
            assert!(store.record_cost(&row(None)).unwrap());
            let total = store
                .total_cost(Utc::now() - chrono::Duration::hours(1), Utc::now())
                .unwrap();
            assert!((total - 4.0).abs() < 1e-9, "{total}");
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn pg_metered_spend_excludes_subscription_rows() {
        with_embedded_pg(|store| async move {
            let row = |identity: Option<&str>, billing, cost_usd| CostRecord {
                agent_id: "allan".into(),
                model: "chatgpt:gpt-6.1-sol".into(),
                cost_usd,
                identity_id: identity.map(String::from),
                task_id: Some("scheduler:job-1:1".into()),
                billing,
                ..Default::default()
            };
            for record in [
                row(Some("alice"), crate::subscription::Billing::Metered, 2.0),
                row(
                    Some("alice"),
                    crate::subscription::Billing::Subscription,
                    50.0,
                ),
                row(None, crate::subscription::Billing::Metered, 1.0),
            ] {
                store.record_cost(&record).unwrap();
            }
            let mut spend = store
                .metered_spend(Utc::now() - chrono::Duration::hours(1), Utc::now())
                .unwrap();
            spend.sort_by(|a, b| a.identity_id.cmp(&b.identity_id));
            let spend: Vec<(Option<&str>, f64)> = spend
                .iter()
                .map(|s| (s.identity_id.as_deref(), s.cost_usd))
                .collect();
            assert_eq!(spend, [(None, 1.0), (Some("alice"), 2.0)]);
            assert_eq!(
                store
                    .metered_cost_by_task_prefix("scheduler:job-1:")
                    .unwrap(),
                3.0
            );
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn cost_billing_migrations_backfill_legacy_rows_and_store_new_ones() {
        with_embedded_pg_pool(|pool| async move {
            let before_billing = MigrationSet::new(
                PostgresTelemetryStore::SCHEMA,
                Box::new(MigrationsUpTo(19)) as Box<dyn MigrationRun>,
            );
            MigrationRunner::run(&pool, &[before_billing]).await.unwrap();
            let legacy = [
                "claude-code:sonnet",
                "claude-code:bedrock:anthropic.claude-sonnet-5",
                "codex:gpt-5.6-sol",
                "codex_api:gpt-5.6-sol",
                "claude-pilot:sonnet",
                "gemini-3.1-pro-preview",
            ];
            let conn = pool.get().await.unwrap();
            for model in legacy {
                conn.execute(
                    "INSERT INTO cost_records (timestamp, agent_id, model, input_tokens, output_tokens, cost_usd)
                     VALUES (now(), 'allan', $1, 1, 1, 0.1)",
                    &[&model],
                )
                .await
                .unwrap();
            }
            drop(conn);

            let store = PostgresTelemetryStore::with_pool(pool.clone()).await.unwrap();
            store
                .record_cost(&CostRecord {
                    agent_id: "allan".into(),
                    model: "codex:gpt-6.1-sol".into(),
                    origin: Some(crate::subscription::Origin::Coding),
                    billing: crate::subscription::Billing::Subscription,
                    payer: Some("install".into()),
                    credential_tier: Some(crate::subscription::CredentialTier::SystemShared),
                    ..Default::default()
                })
                .unwrap();

            let rows: Vec<(String, String, Option<String>, Option<String>, Option<String>)> = pool
                .get()
                .await
                .unwrap()
                .query(
                    "SELECT model, billing, origin, payer, credential_tier FROM cost_records ORDER BY id",
                    &[],
                )
                .await
                .unwrap()
                .iter()
                .map(|r| (r.get(0), r.get(1), r.get(2), r.get(3), r.get(4)))
                .collect();
            let legacy_row = |model: &str, billing: &str| {
                (model.to_string(), billing.to_string(), None, None, None)
            };
            assert_eq!(
                rows,
                [
                    legacy_row("claude-code:sonnet", "subscription"),
                    legacy_row("claude-code:bedrock:anthropic.claude-sonnet-5", "metered"),
                    legacy_row("codex:gpt-5.6-sol", "subscription"),
                    legacy_row("codex_api:gpt-5.6-sol", "metered"),
                    legacy_row("claude-pilot:sonnet", "subscription"),
                    legacy_row("gemini-3.1-pro-preview", "metered"),
                    (
                        "codex:gpt-6.1-sol".to_string(),
                        "subscription".to_string(),
                        Some("coding".to_string()),
                        Some("install".to_string()),
                        Some("system_shared".to_string()),
                    ),
                ]
            );
        })
        .await;
    }

    fn sample_task() -> TaskRow {
        let now = now_micros().to_rfc3339();
        TaskRow {
            id: "task-parity-1".into(),
            title: "parity test".into(),
            description: "compare sqlite vs postgres".into(),
            status: "created".into(),
            priority: "normal".into(),
            assignee: Some("alice".into()),
            team_id: Some("team-a".into()),
            labels: "[]".into(),
            parent_id: None,
            result_json: None,
            created_at: now.clone(),
            updated_at: now,
            started_at: None,
            completed_at: None,
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn parity_tasks_crud() {
        with_embedded_pg(|store| async move {
            let trait_store: Arc<dyn TelemetryStore> = store;

            assert!(trait_store.list_tasks(None).unwrap().is_empty());

            let task = sample_task();
            trait_store.save_task(&task).unwrap();

            let fetched = trait_store.get_task(&task.id).unwrap().expect("task");
            assert_eq!(fetched.id, task.id);
            assert_eq!(fetched.title, task.title);
            assert_eq!(fetched.assignee.as_deref(), Some("alice"));
            assert_eq!(fetched.status, "created");

            trait_store.update_task_status(&task.id, "running").unwrap();
            let running = trait_store.get_task(&task.id).unwrap().expect("task");
            assert_eq!(running.status, "running");

            let listed = trait_store.list_tasks(Some("running")).unwrap();
            assert_eq!(listed.len(), 1);
            assert!(trait_store.list_tasks(Some("done")).unwrap().is_empty());
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn parity_workflow_instances_by_id() {
        with_embedded_pg(|store| async move {
            let trait_store: Arc<dyn TelemetryStore> = store;
            let now = now_micros().to_rfc3339();
            for id in ["run-a", "run-b", "run-c"] {
                trait_store
                    .save_workflow_instance(&WorkflowInstanceRow {
                        id: id.to_string(),
                        workflow_id: "ticket-to-pr".to_string(),
                        status: "completed".to_string(),
                        current_step_id: None,
                        step_history: r#"[{"step_id":"implement"}]"#.to_string(),
                        variables: r#"{"ticket_id":"ENG-1"}"#.to_string(),
                        trigger_json: "{}".to_string(),
                        agent_id: Some("atlas".to_string()),
                        created_at: now.clone(),
                        updated_at: now.clone(),
                        parent_instance_id: None,
                        channel_id: None,
                        thread_id: None,
                        ..Default::default()
                    })
                    .unwrap();
            }
            let named: Vec<String> = ["run-c", "run-a", "chat-session", "run-a"]
                .map(String::from)
                .to_vec();
            let mut found = trait_store.get_workflow_instances(&named).unwrap();
            found.sort_by(|a, b| a.id.cmp(&b.id));
            assert_eq!(
                found.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
                ["run-a", "run-c"]
            );
            assert_eq!(found[0].agent_id.as_deref(), Some("atlas"));
            assert_eq!(found[0].step_history, r#"[{"step_id":"implement"}]"#);
            assert!(trait_store.get_workflow_instances(&[]).unwrap().is_empty());
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_read_only_pool_reads_and_refuses_every_write() {
        let ran = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let reached = Arc::clone(&ran);
        workforce_postgres::test_support::with_test_pool(
            PostgresTelemetryStore::read_only_pool,
            |pool| async move {
                let conn = pool.get().await.unwrap();
                let one: i32 = conn.query_one("SELECT 1", &[]).await.unwrap().get(0);
                assert_eq!(one, 1);
                for write in ["CREATE TABLE written (id INT)", "CREATE SCHEMA telemetry"] {
                    let refused = conn.execute(write, &[]).await.unwrap_err();
                    let why = refused
                        .as_db_error()
                        .map(|e| e.message().to_string())
                        .unwrap_or_default();
                    assert!(why.contains("read-only transaction"), "{write}: {why}");
                }
                reached.store(true, std::sync::atomic::Ordering::SeqCst);
            },
        )
        .await;
        assert!(
            ran.load(std::sync::atomic::Ordering::SeqCst)
                || std::env::var("WORKFORCE_SKIP_POSTGRES_TESTS").is_ok()
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn parity_identity_billing_follows_a_merged_person() {
        with_embedded_pg(|store| async move {
            crate::telemetry::conformance::identity_billing_follows_a_merged_person(store.as_ref());
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn parity_identity_signals_count_exactly_what_a_move_and_an_erase_change() {
        with_embedded_pg(|store| async move {
            crate::telemetry::conformance::identity_signals_count_exactly_what_a_move_and_an_erase_change(
                store.as_ref(),
            );
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn parity_activity_is_pruned_by_completion() {
        with_embedded_pg(|store| async move {
            crate::telemetry::conformance::activity_is_pruned_by_completion(store.as_ref());
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn parity_costs_are_pruned_but_a_jobs_are_kept() {
        with_embedded_pg(|store| async move {
            crate::telemetry::conformance::costs_are_pruned_but_a_jobs_are_kept(store.as_ref());
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_fenced_save_applies_only_at_the_version_it_read() {
        with_embedded_pg(|store| async move {
            crate::telemetry::conformance::a_fenced_save_applies_only_at_the_version_it_read(
                store.as_ref(),
            );
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn every_content_write_moves_the_version_and_the_lease_does_not() {
        with_embedded_pg(|store| async move {
            crate::telemetry::conformance::every_content_write_moves_the_version_and_the_lease_does_not(store.as_ref());
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_fenced_save_after_a_targeted_cancel_is_refused() {
        with_embedded_pg(|store| async move {
            crate::telemetry::conformance::a_fenced_save_after_a_targeted_cancel_is_refused(
                store.as_ref(),
            );
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn parity_workflow_instances_are_deleted() {
        with_embedded_pg(|store| async move {
            crate::telemetry::conformance::instances_are_deleted(store.as_ref());
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn parity_instances_walked_in_pages() {
        with_embedded_pg(|store| async move {
            crate::telemetry::conformance::instances_walked_in_pages(store.as_ref());
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn parity_filtered_instance_listing() {
        with_embedded_pg(|store| async move {
            crate::telemetry::conformance::filtered_instance_listing(store.as_ref());
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn parity_audit_round_trip() {
        with_embedded_pg(|store| async move {
            let trait_store: Arc<dyn TelemetryStore> = store;

            trait_store
                .record_audit(
                    "actor-1",
                    "test.action",
                    "resource-xyz",
                    Some("detail text"),
                    AuditOutcome::Success,
                )
                .unwrap();
            trait_store
                .record_audit(
                    "actor-2",
                    "test.denied",
                    "resource-xyz",
                    None,
                    AuditOutcome::Denied,
                )
                .unwrap();

            let recent = trait_store.recent_audit(10).unwrap();
            assert_eq!(recent.len(), 2);
            // `recent_audit` returns newest-first: the denied record
            // was inserted last.
            assert_eq!(recent[0].actor, "actor-2");
            assert!(matches!(recent[0].outcome, AuditOutcome::Denied));
            assert_eq!(recent[1].actor, "actor-1");
            assert!(matches!(recent[1].outcome, AuditOutcome::Success));
            assert_eq!(recent[1].detail.as_deref(), Some("detail text"));

            let matched = trait_store.search_audit("actor-1", 10).unwrap();
            assert_eq!(matched.len(), 1);
            assert_eq!(matched[0].actor, "actor-1");
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn parity_agent_tool_turns() {
        with_embedded_pg(|store| async move {
            let trait_store: Arc<dyn TelemetryStore> = store;
            let tool = |name: &str| ToolTelemetryRecord {
                round_number: 1,
                tool_name: name.into(),
                success: true,
                duration_ms: 1,
                policy_decision: "allow".into(),
                input_size_bytes: 0,
                output_size_bytes: 0,
                denial_reason: None,
            };
            let turn = |agent: &str, task_type: &str, minutes_ago: i64| {
                let at = now_micros() - chrono::Duration::minutes(minutes_ago);
                TaskTelemetryRecord {
                    agent_id: agent.into(),
                    model: "claude-sonnet-5".into(),
                    provider: "bedrock".into(),
                    input_tokens: 1,
                    output_tokens: 1,
                    cache_creation_input_tokens: 0,
                    cache_read_input_tokens: 0,
                    cost_usd: 0.0,
                    rounds: 1,
                    duration_ms: 1,
                    tool_invocation_count: 1,
                    success: true,
                    error_type: None,
                    retry_count: 0,
                    task_type: Some(task_type.into()),
                    task_id: None,
                    started_at: at.to_rfc3339(),
                    completed_at: at.to_rfc3339(),
                    forecast_cost_usd: None,
                    forecast_rounds: None,
                    forecast_confidence: None,
                    team_id: None,
                    session_id: None,
                    model_switches: None,
                    ticket_id: None,
                    identity_id: None,
                    description: None,
                }
            };
            trait_store
                .record_task_telemetry(
                    &turn("a", "chat", 30),
                    &[],
                    &[tool("memory"), tool("memory"), tool("vault.set")],
                )
                .unwrap();
            trait_store
                .record_task_telemetry(&turn("a", "chat", 20), &[], &[])
                .unwrap();
            trait_store
                .record_task_telemetry(&turn("a", "job:report", 10), &[], &[tool("report")])
                .unwrap();
            trait_store
                .record_task_telemetry(&turn("b", "chat", 5), &[], &[tool("memory")])
                .unwrap();

            let turns = trait_store
                .agent_tool_turns("a", now_micros() - chrono::Duration::days(1), 100)
                .unwrap();
            let mut first = turns[0].tools.clone();
            first.sort();
            assert_eq!(turns.len(), 2);
            assert_eq!(first, ["memory", "vault.set"]);
            assert!(turns[1].tools.is_empty());
            assert!(turns[0].started_at < turns[1].started_at);
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn parity_task_telemetry_and_cost() {
        with_embedded_pg(|store| async move {
            let trait_store: Arc<dyn TelemetryStore> = store;

            let started = now_micros() - chrono::Duration::minutes(5);
            let completed = now_micros();
            let record = TaskTelemetryRecord {
                agent_id: "agent-a".into(),
                model: "claude-opus-4-6".into(),
                provider: "anthropic".into(),
                input_tokens: 1_000,
                output_tokens: 500,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 0,
                cost_usd: 0.0123,
                rounds: 2,
                duration_ms: 4_500,
                tool_invocation_count: 3,
                success: true,
                error_type: None,
                retry_count: 0,
                task_type: Some("code".into()),
                task_id: Some("t-1".into()),
                started_at: started.to_rfc3339(),
                completed_at: completed.to_rfc3339(),
                forecast_cost_usd: None,
                forecast_rounds: None,
                forecast_confidence: None,
                team_id: Some("team-a".into()),
                session_id: Some("slack:C123".into()),
                model_switches: None,
                ticket_id: Some("ticket-42".into()),
                identity_id: Some("alice-uuid".into()),
                description: Some("parity test prompt".into()),
            };
            trait_store
                .record_task_telemetry(&record, &[], &[])
                .unwrap();

            // `record_task_telemetry` only writes the task_telemetry
            // family of tables. Cost aggregation against `cost_records`
            // requires an explicit `record_cost` call (callers do both
            // — see workforce-orchestrator's task completion path).
            trait_store
                .record_cost(&CostRecord {
                    agent_id: "agent-a".into(),
                    model: "claude-opus-4-6".into(),
                    input_tokens: 1_000,
                    output_tokens: 500,
                    cache_creation_input_tokens: 0,
                    cache_read_input_tokens: 0,
                    cost_usd: 0.0123,
                    team_id: Some("team-a".into()),
                    task_id: Some("t-1".into()),
                    task_type: Some("code".into()),
                    ticket_id: Some("ticket-42".into()),
                    identity_id: Some("alice-uuid".into()),
                    description: Some("parity test prompt".into()),
                    ..Default::default()
                })
                .unwrap();

            let total = trait_store
                .total_cost(
                    started - chrono::Duration::hours(1),
                    completed + chrono::Duration::hours(1),
                )
                .unwrap();
            assert!((total - 0.0123).abs() < 1e-9);

            let by_agent = trait_store
                .cost_by_agent(
                    started - chrono::Duration::hours(1),
                    completed + chrono::Duration::hours(1),
                )
                .unwrap();
            assert_eq!(by_agent.len(), 1);
            assert_eq!(by_agent[0].0, "agent-a");
            assert!((by_agent[0].1 - 0.0123).abs() < 1e-9);

            let by_ticket = trait_store.cost_by_ticket("ticket-42").unwrap();
            assert!((by_ticket.total_cost_usd - 0.0123).abs() < 1e-9);
            assert_eq!(by_ticket.step_count, 1);

            let history = trait_store
                .get_task_history("agent-a", 10, None, None)
                .unwrap();
            assert_eq!(history.len(), 1);
            assert_eq!(history[0].agent_id, "agent-a");
            assert_eq!(history[0].model, "claude-opus-4-6");
            assert_eq!(history[0].ticket_id.as_deref(), Some("ticket-42"));

            let day_buckets = trait_store
                .cost_token_history_by_day(
                    started - chrono::Duration::hours(1),
                    completed + chrono::Duration::hours(1),
                )
                .unwrap();
            assert_eq!(day_buckets.len(), 1);
            assert_eq!(day_buckets[0].input_tokens, 1_000);
            assert_eq!(day_buckets[0].output_tokens, 500);
            // Bucket start is UTC midnight regardless of session timezone.
            assert_eq!(day_buckets[0].bucket_start.time().to_string(), "00:00:00");
            assert_eq!(
                day_buckets[0].bucket_start.date_naive(),
                now_micros().date_naive(),
            );
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn parity_model_pricing_upsert() {
        with_embedded_pg(|store| async move {
            let trait_store: Arc<dyn TelemetryStore> = store;

            let now = now_micros().to_rfc3339();
            trait_store
                .upsert_model_pricing(&StoredModelPricing {
                    model: "claude-opus-4-6".into(),
                    provider: "anthropic".into(),
                    input_per_million: 15.0,
                    output_per_million: 75.0,
                    source: "default".into(),
                    fetched_at: now.clone(),
                    updated_at: now.clone(),
                })
                .unwrap();

            // Upsert with a new price — must overwrite not append.
            trait_store
                .upsert_model_pricing(&StoredModelPricing {
                    model: "claude-opus-4-6".into(),
                    provider: "anthropic".into(),
                    input_per_million: 16.0,
                    output_per_million: 80.0,
                    source: "override".into(),
                    fetched_at: now.clone(),
                    updated_at: now,
                })
                .unwrap();

            let all = trait_store.list_model_pricing().unwrap();
            assert_eq!(all.len(), 1);
            let fetched = trait_store
                .get_model_pricing("claude-opus-4-6")
                .unwrap()
                .expect("price");
            assert!((fetched.input_per_million - 16.0).abs() < 1e-9);
            assert_eq!(fetched.source, "override");

            trait_store.delete_model_pricing("claude-opus-4-6").unwrap();
            assert!(trait_store.list_model_pricing().unwrap().is_empty());
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn parity_cost_detail_records() {
        with_embedded_pg(|store| async move {
            let trait_store: Arc<dyn TelemetryStore> = store;
            let mk = |agent: &str, identity: Option<&str>, cost: f64, desc: &str| CostRecord {
                agent_id: agent.into(),
                model: "claude-sonnet-4-6".into(),
                input_tokens: 1_000,
                output_tokens: 500,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 0,
                cost_usd: cost,
                team_id: None,
                task_id: None,
                task_type: Some("chat".into()),
                ticket_id: None,
                identity_id: identity.map(String::from),
                description: Some(desc.into()),
                ..Default::default()
            };
            trait_store
                .record_cost(&mk("agent-a", Some("alice"), 0.10, "vision:gpt-4o"))
                .unwrap();
            trait_store
                .record_cost(&mk("agent-a", Some("bob"), 0.20, "subagent prompt"))
                .unwrap();
            trait_store
                .record_cost(&mk("agent-b", Some("alice"), 0.30, "transcription:42s"))
                .unwrap();
            trait_store
                .record_cost(&mk("agent-b", None, 0.40, "anonymous"))
                .unwrap();

            let from = now_micros() - chrono::Duration::hours(1);
            let to = now_micros() + chrono::Duration::hours(1);

            let page = trait_store
                .cost_detail_records(from, to, CostDetailFilter::default(), 100, 0)
                .unwrap();
            assert_eq!(page.total, 4);
            assert_eq!(page.records.len(), 4);
            assert_eq!(page.distinct_users, vec!["alice", "bob"]);
            assert_eq!(page.distinct_agents, vec!["agent-a", "agent-b"]);

            // Sum invariant — must equal total_cost over the same range.
            let detail_sum: f64 = page.records.iter().map(|r| r.cost_usd).sum();
            let summary = trait_store.total_cost(from, to).unwrap();
            assert!(
                (detail_sum - summary).abs() < 1e-9,
                "detail sum {detail_sum} != summary total {summary}"
            );

            let page = trait_store
                .cost_detail_records(
                    from,
                    to,
                    CostDetailFilter {
                        scope: CostScope {
                            agent_id: Some("agent-a"),
                            ..CostScope::default()
                        },
                        ..CostDetailFilter::default()
                    },
                    100,
                    0,
                )
                .unwrap();
            assert_eq!(page.total, 2);
            assert_eq!(page.distinct_agents, vec!["agent-a", "agent-b"]);

            let page = trait_store
                .cost_detail_records(
                    from,
                    to,
                    CostDetailFilter {
                        scope: CostScope {
                            user_id: Some("alice"),
                            ..CostScope::default()
                        },
                        ..CostDetailFilter::default()
                    },
                    100,
                    0,
                )
                .unwrap();
            assert_eq!(page.total, 2);
            assert_eq!(page.distinct_users, vec!["alice", "bob"]);
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn cost_split_groups_by_billing_origin_payer_tier() {
        use crate::persistence::BillingSplit;
        use crate::subscription::{Billing, CredentialTier, Origin};
        with_embedded_pg(|store| async move {
            let trait_store: Arc<dyn TelemetryStore> = store;
            let billed = |cost: f64, plan: Option<(&str, CredentialTier)>| CostRecord {
                agent_id: "agent-a".into(),
                model: "gpt-6.1-sol".into(),
                cost_usd: cost,
                identity_id: Some("alice".into()),
                origin: Some(Origin::Interactive),
                billing: plan.map_or(Billing::Metered, |_| Billing::Subscription),
                payer: plan.map(|(payer, _)| payer.to_string()),
                credential_tier: plan.map(|(_, tier)| tier),
                ..Default::default()
            };
            for record in [
                billed(0.125, None),
                billed(0.25, Some(("alice", CredentialTier::GlobalUser))),
                billed(0.5, Some(("alice", CredentialTier::GlobalUser))),
            ] {
                trait_store.record_cost(&record).unwrap();
            }
            let from = now_micros() - chrono::Duration::hours(1);
            let to = now_micros() + chrono::Duration::hours(1);

            let rows = trait_store
                .cost_split(from, to, CostScope::default())
                .unwrap();
            assert_eq!(rows.len(), 2, "{rows:?}");
            assert_eq!(rows[1].payer.as_deref(), Some("alice"));
            assert_eq!(
                rows[1].credential_tier.map(CredentialTier::as_str),
                Some("global_user")
            );
            assert_eq!(rows[1].calls, 2);
            assert_eq!(
                BillingSplit::of(&rows),
                BillingSplit {
                    metered_cost_usd: 0.125,
                    subscription_cost_usd: 0.75,
                    subscription_calls: 2,
                }
            );

            let page = trait_store
                .cost_detail_records(
                    from,
                    to,
                    CostDetailFilter {
                        billing: Some(Billing::Subscription),
                        ..CostDetailFilter::default()
                    },
                    100,
                    0,
                )
                .unwrap();
            assert_eq!(page.total, 2);
            assert!(page
                .records
                .iter()
                .all(|r| r.billing == Billing::Subscription
                    && r.origin.as_deref() == Some("interactive")
                    && r.payer.as_deref() == Some("alice")));
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn parity_scheduled_job_history() {
        with_embedded_pg(|store| async move {
            let trait_store: Arc<dyn TelemetryStore> = store;
            let mk = |provider: &str, agent: &str, success: bool| TaskTelemetryRecord {
                agent_id: agent.into(),
                model: String::new(),
                provider: provider.into(),
                input_tokens: 0,
                output_tokens: 0,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 0,
                cost_usd: 0.0,
                rounds: 1,
                duration_ms: 100,
                tool_invocation_count: 1,
                success,
                error_type: if success {
                    None
                } else {
                    Some("job_error".into())
                },
                retry_count: 0,
                task_type: Some("scheduled".into()),
                task_id: Some(format!("{agent}-id")),
                started_at: now_micros().to_rfc3339(),
                completed_at: now_micros().to_rfc3339(),
                forecast_cost_usd: None,
                forecast_rounds: None,
                forecast_confidence: None,
                team_id: None,
                session_id: None,
                model_switches: None,
                ticket_id: None,
                identity_id: (agent == "scheduler-1").then(|| "alice".to_string()),
                description: Some(format!("tool:{agent}")),
            };
            trait_store
                .record_task_telemetry(&mk("job_runner", "scheduler-1", true), &[], &[])
                .unwrap();
            trait_store
                .record_task_telemetry(&mk("job_runner", "scheduler-2", false), &[], &[])
                .unwrap();
            trait_store
                .record_task_telemetry(&mk("anthropic", "agent-x", true), &[], &[])
                .unwrap();

            let from = now_micros() - chrono::Duration::hours(1);
            let to = now_micros() + chrono::Duration::hours(1);

            let page = trait_store
                .scheduled_job_history(from, to, CostScope::default(), 100, 0)
                .unwrap();
            assert_eq!(page.total, 2);
            assert!(page
                .records
                .iter()
                .all(|r| r.agent_id.starts_with("scheduler-")));

            let page = trait_store
                .scheduled_job_history(
                    from,
                    to,
                    CostScope {
                        agent_id: Some("scheduler-1"),
                        user_id: None,
                    },
                    100,
                    0,
                )
                .unwrap();
            assert_eq!(page.total, 1);
            assert_eq!(page.records[0].agent_id, "scheduler-1");
            assert!(page.records[0].success);

            let runs_of = |user_id| {
                trait_store
                    .scheduled_job_history(
                        from,
                        to,
                        CostScope {
                            agent_id: None,
                            user_id: Some(user_id),
                        },
                        100,
                        0,
                    )
                    .unwrap()
            };
            let alices = runs_of("alice");
            assert_eq!(alices.total, 1);
            assert_eq!(alices.records[0].user_id.as_deref(), Some("alice"));
            assert_eq!(runs_of("bob").total, 0);
        })
        .await;
    }
}
