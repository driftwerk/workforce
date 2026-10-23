//! One-shot SQLite → Postgres migration for the telemetry store.
//!
//! Read every row from a `telemetry.db` SQLite file and write it into
//! the corresponding table in a Postgres backend (after running the
//! refinery migrations against the destination). Driven by the
//! `workforce migrate-telemetry` CLI subcommand.
//!
//! ## Design
//!
//! - **Idempotent.** Every insert uses `ON CONFLICT DO NOTHING`, so
//!   re-running on the same source/destination is safe and skips
//!   already-migrated rows.
//! - **Source IDs preserved.** Tables with `BIGSERIAL` primary keys
//!   (`task_telemetry`, `audit_log`, etc.) are written with their
//!   original IDs from SQLite. This is essential for `task_telemetry`
//!   because `round_telemetry` and `tool_telemetry` reference it via
//!   foreign key, and they too carry their original IDs through. After
//!   each table is loaded the corresponding sequence is bumped via
//!   `setval` so subsequent live writes pick up where SQLite left off.
//! - **Batched.** Rows are sent in chunks (`BATCH_SIZE`) using
//!   parameterised multi-row inserts to keep round-trips low without
//!   risking the Postgres parameter limit (~32k).
//! - **Order matters.** Parents before children: `task_telemetry`
//!   before `round_telemetry`/`tool_telemetry`. The migration runs
//!   tables in dependency order.
//! - **Timestamps.** SQLite stores `RFC3339` strings; Postgres stores
//!   `TIMESTAMPTZ`. We parse on read and bind native
//!   `chrono::DateTime<Utc>` on write.
//!
//! ## Out of scope
//!
//! - Schema diffs / partial migrations. Source schema is assumed to
//!   match what the SQLite store would create today; if the SQLite
//!   file is from an older workforce version, run that version's
//!   `SqliteStore::open` first to bring the schema up to date before
//!   migrating.
//! - Live cutover. The expectation is: stop the hub, migrate, restart
//!   the hub against `[telemetry] backend = "postgres"`.

use std::path::Path;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use eyre::{Context, Result};
use rusqlite::{Connection, OpenFlags};
use tokio_postgres::types::ToSql;

use crate::postgres::{PostgresConfig, PostgresPool};
use crate::telemetry_postgres::PostgresTelemetryStore;

/// Maximum rows per batched INSERT. The Postgres protocol caps a
/// single statement at 65535 parameters; with the widest table
/// (`task_telemetry`, 26 columns including `id`) that allows ~2500
/// rows per batch. 1000 leaves comfortable headroom.
const BATCH_SIZE: usize = 1000;

/// Per-table row count summary returned to the caller for reporting.
#[derive(Debug, Default, Clone)]
pub struct MigrationReport {
    pub tasks: usize,
    pub audit_log: usize,
    pub cost_records: usize,
    pub task_telemetry: usize,
    pub round_telemetry: usize,
    pub tool_telemetry: usize,
    pub model_health_events: usize,
    pub agent_model_preferences: usize,
    pub policy_proposals: usize,
    pub model_pricing: usize,
    pub workflow_instances: usize,
    pub security_events: usize,
}

impl MigrationReport {
    pub fn total(&self) -> usize {
        self.tasks
            + self.audit_log
            + self.cost_records
            + self.task_telemetry
            + self.round_telemetry
            + self.tool_telemetry
            + self.model_health_events
            + self.agent_model_preferences
            + self.policy_proposals
            + self.model_pricing
            + self.workflow_instances
            + self.security_events
    }
}

/// Migrate a SQLite telemetry database into a Postgres backend.
///
/// Builds a fresh telemetry pool from `pg_config` (with `search_path`
/// pinned to the telemetry schema), opens the SQLite file read-only,
/// ensures the destination schema is up to date by constructing a
/// [`PostgresTelemetryStore`] (which runs the refinery migrations),
/// and then copies every table.
pub async fn migrate_sqlite_to_postgres(
    sqlite_path: &Path,
    pg_config: &PostgresConfig,
) -> Result<MigrationReport> {
    let pool = PostgresTelemetryStore::build_pool(pg_config)
        .context("building telemetry pool for migration destination")?;
    migrate_sqlite_to_postgres_with_pool(sqlite_path, pool).await
}

/// Variant taking an already-built pool. The pool MUST have come from
/// [`PostgresTelemetryStore::build_pool`] (or otherwise have its
/// `search_path` pinned to the telemetry schema). Used by tests
/// against the embedded Postgres harness.
pub async fn migrate_sqlite_to_postgres_with_pool(
    sqlite_path: &Path,
    pool: PostgresPool,
) -> Result<MigrationReport> {
    // Run destination migrations first so the tables exist.
    let _store: Arc<PostgresTelemetryStore> = PostgresTelemetryStore::with_pool(pool.clone())
        .await
        .context("preparing Postgres destination")?;

    let sqlite = Connection::open_with_flags(sqlite_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("opening SQLite source at {}", sqlite_path.display()))?;

    // Migrate in dependency order: parents before children so foreign
    // keys on round_telemetry / tool_telemetry resolve on insert.
    let tasks = migrate_tasks(&sqlite, &pool).await?;
    let audit_log = migrate_audit_log(&sqlite, &pool).await?;
    let cost_records = migrate_cost_records(&sqlite, &pool).await?;
    let task_telemetry = migrate_task_telemetry(&sqlite, &pool).await?;
    let round_telemetry = migrate_round_telemetry(&sqlite, &pool).await?;
    let tool_telemetry = migrate_tool_telemetry(&sqlite, &pool).await?;
    let model_health_events = migrate_model_health_events(&sqlite, &pool).await?;
    let agent_model_preferences = migrate_agent_model_preferences(&sqlite, &pool).await?;
    let policy_proposals = migrate_policy_proposals(&sqlite, &pool).await?;
    let model_pricing = migrate_model_pricing(&sqlite, &pool).await?;
    let workflow_instances = migrate_workflow_instances(&sqlite, &pool).await?;
    let security_events = migrate_security_events(&sqlite, &pool).await?;

    Ok(MigrationReport {
        tasks,
        audit_log,
        cost_records,
        task_telemetry,
        round_telemetry,
        tool_telemetry,
        model_health_events,
        agent_model_preferences,
        policy_proposals,
        model_pricing,
        workflow_instances,
        security_events,
    })
}

// ─── Helpers ───────────────────────────────────────────────────────────

fn parse_ts(s: &str) -> Result<DateTime<Utc>> {
    Ok(DateTime::parse_from_rfc3339(s)
        .with_context(|| format!("invalid RFC3339 timestamp: {s}"))?
        .with_timezone(&Utc))
}

fn parse_ts_opt(s: Option<String>) -> Result<Option<DateTime<Utc>>> {
    s.as_deref().map(parse_ts).transpose()
}

/// Build a multi-row INSERT placeholder string like
/// `($1, $2, $3), ($4, $5, $6), ...` for `cols * rows` parameters.
fn placeholders(rows: usize, cols: usize) -> String {
    let mut s = String::with_capacity(rows * (cols * 5 + 4));
    for r in 0..rows {
        if r > 0 {
            s.push_str(", ");
        }
        s.push('(');
        for c in 0..cols {
            if c > 0 {
                s.push_str(", ");
            }
            s.push('$');
            s.push_str(&((r * cols + c) + 1).to_string());
        }
        s.push(')');
    }
    s
}

/// `pg_get_serial_sequence` and `MAX(col) FROM table` only work with
/// the table/column name interpolated into the SQL string, not bound
/// as parameters. Every caller passes a hardcoded `&'static str`, but
/// this validator makes that contract enforced rather than implicit
/// — any future caller that tries to pass user input fails loudly.
fn validate_ident(name: &str) -> Result<()> {
    let valid = !name.is_empty()
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !name.starts_with(|c: char| c.is_ascii_digit());
    if !valid {
        eyre::bail!("invalid SQL identifier: {name:?}");
    }
    Ok(())
}

/// After a serial-id table is loaded with explicit IDs, advance the
/// sequence past `MAX(id)` so subsequent live writes don't collide.
async fn bump_sequence(pool: &PostgresPool, table: &str, id_col: &str) -> Result<()> {
    validate_ident(table)?;
    validate_ident(id_col)?;
    let conn = pool.get().await?;
    let sql = format!(
        "SELECT setval(pg_get_serial_sequence('{table}', '{id_col}'),
                       COALESCE((SELECT MAX({id_col}) FROM {table}), 1),
                       (SELECT MAX({id_col}) IS NOT NULL FROM {table}))"
    );
    conn.execute(&sql, &[])
        .await
        .with_context(|| format!("bumping sequence for {table}.{id_col}"))?;
    Ok(())
}

// ─── Per-table migrators ───────────────────────────────────────────────

async fn migrate_tasks(sqlite: &Connection, pool: &PostgresPool) -> Result<usize> {
    type Row = (
        String,                // id
        String,                // title
        String,                // description
        String,                // status
        String,                // priority
        Option<String>,        // assignee
        Option<String>,        // team_id
        String,                // labels
        Option<String>,        // parent_id
        Option<String>,        // result_json
        DateTime<Utc>,         // created_at
        DateTime<Utc>,         // updated_at
        Option<DateTime<Utc>>, // started_at
        Option<DateTime<Utc>>, // completed_at
    );

    let mut stmt = sqlite.prepare(
        "SELECT id, title, description, status, priority, assignee, team_id,
                labels, parent_id, result_json, created_at, updated_at,
                started_at, completed_at
         FROM tasks",
    )?;
    let rows: Vec<Row> = stmt
        .query_map([], |r| {
            let created_at: String = r.get(10)?;
            let updated_at: String = r.get(11)?;
            let started_at: Option<String> = r.get(12)?;
            let completed_at: Option<String> = r.get(13)?;
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
                r.get(7)?,
                r.get(8)?,
                r.get(9)?,
                parse_ts(&created_at).map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        10,
                        rusqlite::types::Type::Text,
                        e.into(),
                    )
                })?,
                parse_ts(&updated_at).map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        11,
                        rusqlite::types::Type::Text,
                        e.into(),
                    )
                })?,
                parse_ts_opt(started_at).map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        12,
                        rusqlite::types::Type::Text,
                        e.into(),
                    )
                })?,
                parse_ts_opt(completed_at).map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        13,
                        rusqlite::types::Type::Text,
                        e.into(),
                    )
                })?,
            ))
        })?
        .collect::<rusqlite::Result<_>>()?;

    let total = rows.len();
    if total == 0 {
        return Ok(0);
    }

    let conn = pool.get().await?;
    for chunk in rows.chunks(BATCH_SIZE) {
        let sql = format!(
            "INSERT INTO tasks
             (id, title, description, status, priority, assignee, team_id,
              labels, parent_id, result_json, created_at, updated_at,
              started_at, completed_at)
             VALUES {}
             ON CONFLICT (id) DO NOTHING",
            placeholders(chunk.len(), 14)
        );
        let mut params: Vec<&(dyn ToSql + Sync)> = Vec::with_capacity(chunk.len() * 14);
        for r in chunk {
            params.push(&r.0);
            params.push(&r.1);
            params.push(&r.2);
            params.push(&r.3);
            params.push(&r.4);
            params.push(&r.5);
            params.push(&r.6);
            params.push(&r.7);
            params.push(&r.8);
            params.push(&r.9);
            params.push(&r.10);
            params.push(&r.11);
            params.push(&r.12);
            params.push(&r.13);
        }
        conn.execute(&sql, &params).await.context("insert tasks")?;
    }

    Ok(total)
}

async fn migrate_audit_log(sqlite: &Connection, pool: &PostgresPool) -> Result<usize> {
    type Row = (
        i64,            // id
        DateTime<Utc>,  // timestamp
        String,         // actor
        String,         // action
        String,         // resource
        Option<String>, // detail
        String,         // outcome
    );

    let mut stmt = sqlite
        .prepare("SELECT id, timestamp, actor, action, resource, detail, outcome FROM audit_log")?;
    let rows: Vec<Row> = stmt
        .query_map([], |r| {
            let ts: String = r.get(1)?;
            Ok((
                r.get(0)?,
                parse_ts(&ts).map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        1,
                        rusqlite::types::Type::Text,
                        e.into(),
                    )
                })?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
            ))
        })?
        .collect::<rusqlite::Result<_>>()?;

    let total = rows.len();
    if total == 0 {
        return Ok(0);
    }

    let conn = pool.get().await?;
    for chunk in rows.chunks(BATCH_SIZE) {
        let sql = format!(
            "INSERT INTO audit_log (id, timestamp, actor, action, resource, detail, outcome)
             VALUES {}
             ON CONFLICT (id) DO NOTHING",
            placeholders(chunk.len(), 7)
        );
        let mut params: Vec<&(dyn ToSql + Sync)> = Vec::with_capacity(chunk.len() * 7);
        for r in chunk {
            params.push(&r.0);
            params.push(&r.1);
            params.push(&r.2);
            params.push(&r.3);
            params.push(&r.4);
            params.push(&r.5);
            params.push(&r.6);
        }
        conn.execute(&sql, &params)
            .await
            .context("insert audit_log")?;
    }
    bump_sequence(pool, "audit_log", "id").await?;
    Ok(total)
}

async fn migrate_cost_records(sqlite: &Connection, pool: &PostgresPool) -> Result<usize> {
    struct Row {
        id: i64,
        timestamp: DateTime<Utc>,
        agent_id: String,
        model: String,
        input_tokens: i64,
        output_tokens: i64,
        cache_creation_input_tokens: i64,
        cache_read_input_tokens: i64,
        cost_usd: f64,
        team_id: Option<String>,
        task_id: Option<String>,
        task_type: Option<String>,
        ticket_id: Option<String>,
        identity_id: Option<String>,
        origin: Option<String>,
        billing: String,
        payer: Option<String>,
        credential_tier: Option<String>,
        ledger_key: Option<String>,
    }

    let has_column = |name: &str| -> Result<bool> {
        Ok(sqlite.query_row(
            "SELECT COUNT(*) FROM pragma_table_info('cost_records') WHERE name = ?1",
            [name],
            |r| r.get::<_, i64>(0),
        )? > 0)
    };
    // A source from before the billing columns reads them as their defaults;
    // its rows are backfilled once copied, as V021 does in place.
    let billed = has_column("billing")?;
    let billing_columns = if billed {
        "origin, billing, payer, credential_tier"
    } else {
        "NULL, 'metered', NULL, NULL"
    };
    let ledger_key = if has_column("ledger_key")? {
        "ledger_key"
    } else {
        "NULL"
    };
    let mut stmt = sqlite.prepare(&format!(
        "SELECT id, timestamp, agent_id, model, input_tokens, output_tokens,
                cache_creation_input_tokens, cache_read_input_tokens,
                cost_usd, team_id, task_id, task_type, ticket_id, identity_id,
                {billing_columns}, {ledger_key}
         FROM cost_records"
    ))?;
    let rows: Vec<Row> = stmt
        .query_map([], |r| {
            let ts: String = r.get(1)?;
            Ok(Row {
                id: r.get(0)?,
                timestamp: parse_ts(&ts).map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        1,
                        rusqlite::types::Type::Text,
                        e.into(),
                    )
                })?,
                agent_id: r.get(2)?,
                model: r.get(3)?,
                input_tokens: r.get(4)?,
                output_tokens: r.get(5)?,
                cache_creation_input_tokens: r.get(6)?,
                cache_read_input_tokens: r.get(7)?,
                cost_usd: r.get(8)?,
                team_id: r.get(9)?,
                task_id: r.get(10)?,
                task_type: r.get(11)?,
                ticket_id: r.get(12)?,
                identity_id: r.get(13)?,
                origin: r.get(14)?,
                billing: r.get(15)?,
                payer: r.get(16)?,
                credential_tier: r.get(17)?,
                ledger_key: r.get(18)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;

    let total = rows.len();
    if total == 0 {
        return Ok(0);
    }

    let conn = pool.get().await?;
    for chunk in rows.chunks(BATCH_SIZE) {
        let sql = format!(
            "INSERT INTO cost_records
             (id, timestamp, agent_id, model, input_tokens, output_tokens,
              cache_creation_input_tokens, cache_read_input_tokens,
              cost_usd, team_id, task_id, task_type, ticket_id, identity_id,
              origin, billing, payer, credential_tier, ledger_key)
             VALUES {}
             ON CONFLICT DO NOTHING",
            placeholders(chunk.len(), 19)
        );
        let mut params: Vec<&(dyn ToSql + Sync)> = Vec::with_capacity(chunk.len() * 19);
        for r in chunk {
            params.push(&r.id);
            params.push(&r.timestamp);
            params.push(&r.agent_id);
            params.push(&r.model);
            params.push(&r.input_tokens);
            params.push(&r.output_tokens);
            params.push(&r.cache_creation_input_tokens);
            params.push(&r.cache_read_input_tokens);
            params.push(&r.cost_usd);
            params.push(&r.team_id);
            params.push(&r.task_id);
            params.push(&r.task_type);
            params.push(&r.ticket_id);
            params.push(&r.identity_id);
            params.push(&r.origin);
            params.push(&r.billing);
            params.push(&r.payer);
            params.push(&r.credential_tier);
            params.push(&r.ledger_key);
        }
        conn.execute(&sql, &params)
            .await
            .context("insert cost_records")?;
    }
    if !billed {
        conn.batch_execute(crate::persistence::SUBSCRIPTION_BILLING_BACKFILL)
            .await
            .context("backfill cost_records billing")?;
    }
    bump_sequence(pool, "cost_records", "id").await?;
    Ok(total)
}

#[allow(clippy::type_complexity)]
async fn migrate_task_telemetry(sqlite: &Connection, pool: &PostgresPool) -> Result<usize> {
    // 26 columns including id. Bind individually rather than as a tuple
    // (Rust tuples max out at 12-arity for trait impls).
    struct Row {
        id: i64,
        agent_id: String,
        model: String,
        provider: String,
        input_tokens: i64,
        output_tokens: i64,
        cache_creation_input_tokens: i64,
        cache_read_input_tokens: i64,
        cost_usd: f64,
        rounds: i64,
        duration_ms: i64,
        tool_invocation_count: i64,
        success: bool,
        error_type: Option<String>,
        retry_count: i64,
        task_type: Option<String>,
        task_id: Option<String>,
        started_at: DateTime<Utc>,
        completed_at: DateTime<Utc>,
        forecast_cost_usd: Option<f64>,
        forecast_rounds: Option<i64>,
        forecast_confidence: Option<f64>,
        team_id: Option<String>,
        session_id: Option<String>,
        model_switches: Option<String>,
        ticket_id: Option<String>,
        identity_id: Option<String>,
    }

    let mut stmt = sqlite.prepare(
        "SELECT id, agent_id, model, provider, input_tokens, output_tokens,
                cache_creation_input_tokens, cache_read_input_tokens, cost_usd,
                rounds, duration_ms, tool_invocation_count, success, error_type,
                retry_count, task_type, task_id, started_at, completed_at,
                forecast_cost_usd, forecast_rounds, forecast_confidence,
                team_id, session_id, model_switches, ticket_id, identity_id
         FROM task_telemetry",
    )?;
    let rows: Vec<Row> = stmt
        .query_map([], |r| {
            let started_at: String = r.get(17)?;
            let completed_at: String = r.get(18)?;
            let success_int: i64 = r.get(12)?;
            Ok(Row {
                id: r.get(0)?,
                agent_id: r.get(1)?,
                model: r.get(2)?,
                provider: r.get(3)?,
                input_tokens: r.get(4)?,
                output_tokens: r.get(5)?,
                cache_creation_input_tokens: r.get(6)?,
                cache_read_input_tokens: r.get(7)?,
                cost_usd: r.get(8)?,
                rounds: r.get(9)?,
                duration_ms: r.get(10)?,
                tool_invocation_count: r.get(11)?,
                success: success_int != 0,
                error_type: r.get(13)?,
                retry_count: r.get(14)?,
                task_type: r.get(15)?,
                task_id: r.get(16)?,
                started_at: parse_ts(&started_at).map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        17,
                        rusqlite::types::Type::Text,
                        e.into(),
                    )
                })?,
                completed_at: parse_ts(&completed_at).map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        18,
                        rusqlite::types::Type::Text,
                        e.into(),
                    )
                })?,
                forecast_cost_usd: r.get(19)?,
                forecast_rounds: r.get(20)?,
                forecast_confidence: r.get(21)?,
                team_id: r.get(22)?,
                session_id: r.get(23)?,
                model_switches: r.get(24)?,
                ticket_id: r.get(25)?,
                identity_id: r.get(26)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;

    let total = rows.len();
    if total == 0 {
        return Ok(0);
    }

    let conn = pool.get().await?;
    for chunk in rows.chunks(BATCH_SIZE) {
        let sql = format!(
            "INSERT INTO task_telemetry
             (id, agent_id, model, provider, input_tokens, output_tokens,
              cache_creation_input_tokens, cache_read_input_tokens, cost_usd,
              rounds, duration_ms, tool_invocation_count, success, error_type,
              retry_count, task_type, task_id, started_at, completed_at,
              forecast_cost_usd, forecast_rounds, forecast_confidence,
              team_id, session_id, model_switches, ticket_id, identity_id)
             VALUES {}
             ON CONFLICT (id) DO NOTHING",
            placeholders(chunk.len(), 27)
        );
        let mut params: Vec<&(dyn ToSql + Sync)> = Vec::with_capacity(chunk.len() * 27);
        for r in chunk {
            params.push(&r.id);
            params.push(&r.agent_id);
            params.push(&r.model);
            params.push(&r.provider);
            params.push(&r.input_tokens);
            params.push(&r.output_tokens);
            params.push(&r.cache_creation_input_tokens);
            params.push(&r.cache_read_input_tokens);
            params.push(&r.cost_usd);
            params.push(&r.rounds);
            params.push(&r.duration_ms);
            params.push(&r.tool_invocation_count);
            params.push(&r.success);
            params.push(&r.error_type);
            params.push(&r.retry_count);
            params.push(&r.task_type);
            params.push(&r.task_id);
            params.push(&r.started_at);
            params.push(&r.completed_at);
            params.push(&r.forecast_cost_usd);
            params.push(&r.forecast_rounds);
            params.push(&r.forecast_confidence);
            params.push(&r.team_id);
            params.push(&r.session_id);
            params.push(&r.model_switches);
            params.push(&r.ticket_id);
            params.push(&r.identity_id);
        }
        conn.execute(&sql, &params)
            .await
            .context("insert task_telemetry")?;
    }
    bump_sequence(pool, "task_telemetry", "id").await?;
    Ok(total)
}

async fn migrate_round_telemetry(sqlite: &Connection, pool: &PostgresPool) -> Result<usize> {
    type Row = (
        i64,
        i64,
        i64,
        i64,
        i64,
        i64,
        i64,
        i64,
        i64,
        i64,
        i64,
        String,
    );

    let mut stmt = sqlite.prepare(
        "SELECT id, task_telemetry_id, round_number, input_tokens, output_tokens,
                latency_ms, cache_creation_tokens, cache_read_tokens,
                prompt_size_bytes, response_size_bytes, tool_count, stop_reason
         FROM round_telemetry",
    )?;
    let rows: Vec<Row> = stmt
        .query_map([], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
                r.get(7)?,
                r.get(8)?,
                r.get(9)?,
                r.get(10)?,
                r.get(11)?,
            ))
        })?
        .collect::<rusqlite::Result<_>>()?;

    let total = rows.len();
    if total == 0 {
        return Ok(0);
    }

    let conn = pool.get().await?;
    for chunk in rows.chunks(BATCH_SIZE) {
        let sql = format!(
            "INSERT INTO round_telemetry
             (id, task_telemetry_id, round_number, input_tokens, output_tokens,
              latency_ms, cache_creation_tokens, cache_read_tokens,
              prompt_size_bytes, response_size_bytes, tool_count, stop_reason)
             VALUES {}
             ON CONFLICT (id) DO NOTHING",
            placeholders(chunk.len(), 12)
        );
        let mut params: Vec<&(dyn ToSql + Sync)> = Vec::with_capacity(chunk.len() * 12);
        for r in chunk {
            params.push(&r.0);
            params.push(&r.1);
            params.push(&r.2);
            params.push(&r.3);
            params.push(&r.4);
            params.push(&r.5);
            params.push(&r.6);
            params.push(&r.7);
            params.push(&r.8);
            params.push(&r.9);
            params.push(&r.10);
            params.push(&r.11);
        }
        conn.execute(&sql, &params)
            .await
            .context("insert round_telemetry")?;
    }
    bump_sequence(pool, "round_telemetry", "id").await?;
    Ok(total)
}

async fn migrate_tool_telemetry(sqlite: &Connection, pool: &PostgresPool) -> Result<usize> {
    struct Row {
        id: i64,
        task_telemetry_id: i64,
        round_number: i64,
        tool_name: String,
        success: bool,
        duration_ms: i64,
        policy_decision: String,
        input_size_bytes: i64,
        output_size_bytes: i64,
        denial_reason: Option<String>,
    }

    let mut stmt = sqlite.prepare(
        "SELECT id, task_telemetry_id, round_number, tool_name, success,
                duration_ms, policy_decision, input_size_bytes, output_size_bytes,
                denial_reason
         FROM tool_telemetry",
    )?;
    let rows: Vec<Row> = stmt
        .query_map([], |r| {
            let success_int: i64 = r.get(4)?;
            Ok(Row {
                id: r.get(0)?,
                task_telemetry_id: r.get(1)?,
                round_number: r.get(2)?,
                tool_name: r.get(3)?,
                success: success_int != 0,
                duration_ms: r.get(5)?,
                policy_decision: r.get(6)?,
                input_size_bytes: r.get(7)?,
                output_size_bytes: r.get(8)?,
                denial_reason: r.get(9)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;

    let total = rows.len();
    if total == 0 {
        return Ok(0);
    }

    let conn = pool.get().await?;
    for chunk in rows.chunks(BATCH_SIZE) {
        let sql = format!(
            "INSERT INTO tool_telemetry
             (id, task_telemetry_id, round_number, tool_name, success,
              duration_ms, policy_decision, input_size_bytes, output_size_bytes,
              denial_reason)
             VALUES {}
             ON CONFLICT (id) DO NOTHING",
            placeholders(chunk.len(), 10)
        );
        let mut params: Vec<&(dyn ToSql + Sync)> = Vec::with_capacity(chunk.len() * 10);
        for r in chunk {
            params.push(&r.id);
            params.push(&r.task_telemetry_id);
            params.push(&r.round_number);
            params.push(&r.tool_name);
            params.push(&r.success);
            params.push(&r.duration_ms);
            params.push(&r.policy_decision);
            params.push(&r.input_size_bytes);
            params.push(&r.output_size_bytes);
            params.push(&r.denial_reason);
        }
        conn.execute(&sql, &params)
            .await
            .context("insert tool_telemetry")?;
    }
    bump_sequence(pool, "tool_telemetry", "id").await?;
    Ok(total)
}

async fn migrate_model_health_events(sqlite: &Connection, pool: &PostgresPool) -> Result<usize> {
    type Row = (
        i64,
        DateTime<Utc>,
        String,
        String,
        Option<String>,
        Option<String>,
    );

    let mut stmt = sqlite.prepare(
        "SELECT id, timestamp, model, event_type, detail, agent_id FROM model_health_events",
    )?;
    let rows: Vec<Row> = stmt
        .query_map([], |r| {
            let ts: String = r.get(1)?;
            Ok((
                r.get(0)?,
                parse_ts(&ts).map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        1,
                        rusqlite::types::Type::Text,
                        e.into(),
                    )
                })?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
            ))
        })?
        .collect::<rusqlite::Result<_>>()?;

    let total = rows.len();
    if total == 0 {
        return Ok(0);
    }

    let conn = pool.get().await?;
    for chunk in rows.chunks(BATCH_SIZE) {
        let sql = format!(
            "INSERT INTO model_health_events (id, timestamp, model, event_type, detail, agent_id)
             VALUES {}
             ON CONFLICT (id) DO NOTHING",
            placeholders(chunk.len(), 6)
        );
        let mut params: Vec<&(dyn ToSql + Sync)> = Vec::with_capacity(chunk.len() * 6);
        for r in chunk {
            params.push(&r.0);
            params.push(&r.1);
            params.push(&r.2);
            params.push(&r.3);
            params.push(&r.4);
            params.push(&r.5);
        }
        conn.execute(&sql, &params)
            .await
            .context("insert model_health_events")?;
    }
    bump_sequence(pool, "model_health_events", "id").await?;
    Ok(total)
}

async fn migrate_agent_model_preferences(
    sqlite: &Connection,
    pool: &PostgresPool,
) -> Result<usize> {
    type Row = (String, String, String, Option<String>, DateTime<Utc>);

    let mut stmt = sqlite.prepare(
        "SELECT agent_id, task_type, preferred_model, reason, updated_at
         FROM agent_model_preferences",
    )?;
    let rows: Vec<Row> = stmt
        .query_map([], |r| {
            let ts: String = r.get(4)?;
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                parse_ts(&ts).map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        4,
                        rusqlite::types::Type::Text,
                        e.into(),
                    )
                })?,
            ))
        })?
        .collect::<rusqlite::Result<_>>()?;

    let total = rows.len();
    if total == 0 {
        return Ok(0);
    }

    let conn = pool.get().await?;
    for chunk in rows.chunks(BATCH_SIZE) {
        let sql = format!(
            "INSERT INTO agent_model_preferences
             (agent_id, task_type, preferred_model, reason, updated_at)
             VALUES {}
             ON CONFLICT (agent_id, task_type) DO NOTHING",
            placeholders(chunk.len(), 5)
        );
        let mut params: Vec<&(dyn ToSql + Sync)> = Vec::with_capacity(chunk.len() * 5);
        for r in chunk {
            params.push(&r.0);
            params.push(&r.1);
            params.push(&r.2);
            params.push(&r.3);
            params.push(&r.4);
        }
        conn.execute(&sql, &params)
            .await
            .context("insert agent_model_preferences")?;
    }
    Ok(total)
}

async fn migrate_policy_proposals(sqlite: &Connection, pool: &PostgresPool) -> Result<usize> {
    type Row = (
        i64,
        String,
        String,
        String,
        String,
        Option<String>,
        Option<String>,
        DateTime<Utc>,
        Option<DateTime<Utc>>,
    );

    let mut stmt = sqlite.prepare(
        "SELECT id, agent_id, proposed_rule, justification, status,
                reviewed_by, review_note, created_at, reviewed_at
         FROM policy_proposals",
    )?;
    let rows: Vec<Row> = stmt
        .query_map([], |r| {
            let created_at: String = r.get(7)?;
            let reviewed_at: Option<String> = r.get(8)?;
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
                parse_ts(&created_at).map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        7,
                        rusqlite::types::Type::Text,
                        e.into(),
                    )
                })?,
                parse_ts_opt(reviewed_at).map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        8,
                        rusqlite::types::Type::Text,
                        e.into(),
                    )
                })?,
            ))
        })?
        .collect::<rusqlite::Result<_>>()?;

    let total = rows.len();
    if total == 0 {
        return Ok(0);
    }

    let conn = pool.get().await?;
    for chunk in rows.chunks(BATCH_SIZE) {
        let sql = format!(
            "INSERT INTO policy_proposals
             (id, agent_id, proposed_rule, justification, status,
              reviewed_by, review_note, created_at, reviewed_at)
             VALUES {}
             ON CONFLICT (id) DO NOTHING",
            placeholders(chunk.len(), 9)
        );
        let mut params: Vec<&(dyn ToSql + Sync)> = Vec::with_capacity(chunk.len() * 9);
        for r in chunk {
            params.push(&r.0);
            params.push(&r.1);
            params.push(&r.2);
            params.push(&r.3);
            params.push(&r.4);
            params.push(&r.5);
            params.push(&r.6);
            params.push(&r.7);
            params.push(&r.8);
        }
        conn.execute(&sql, &params)
            .await
            .context("insert policy_proposals")?;
    }
    bump_sequence(pool, "policy_proposals", "id").await?;
    Ok(total)
}

async fn migrate_model_pricing(sqlite: &Connection, pool: &PostgresPool) -> Result<usize> {
    type Row = (
        String,
        String,
        f64,
        f64,
        String,
        DateTime<Utc>,
        DateTime<Utc>,
    );

    let mut stmt = sqlite.prepare(
        "SELECT model, provider, input_per_million, output_per_million,
                source, fetched_at, updated_at
         FROM model_pricing",
    )?;
    let rows: Vec<Row> = stmt
        .query_map([], |r| {
            let fetched_at: String = r.get(5)?;
            let updated_at: String = r.get(6)?;
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                parse_ts(&fetched_at).map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        5,
                        rusqlite::types::Type::Text,
                        e.into(),
                    )
                })?,
                parse_ts(&updated_at).map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        6,
                        rusqlite::types::Type::Text,
                        e.into(),
                    )
                })?,
            ))
        })?
        .collect::<rusqlite::Result<_>>()?;

    let total = rows.len();
    if total == 0 {
        return Ok(0);
    }

    let conn = pool.get().await?;
    for chunk in rows.chunks(BATCH_SIZE) {
        let sql = format!(
            "INSERT INTO model_pricing
             (model, provider, input_per_million, output_per_million,
              source, fetched_at, updated_at)
             VALUES {}
             ON CONFLICT (model) DO NOTHING",
            placeholders(chunk.len(), 7)
        );
        let mut params: Vec<&(dyn ToSql + Sync)> = Vec::with_capacity(chunk.len() * 7);
        for r in chunk {
            params.push(&r.0);
            params.push(&r.1);
            params.push(&r.2);
            params.push(&r.3);
            params.push(&r.4);
            params.push(&r.5);
            params.push(&r.6);
        }
        conn.execute(&sql, &params)
            .await
            .context("insert model_pricing")?;
    }
    Ok(total)
}

async fn migrate_workflow_instances(sqlite: &Connection, pool: &PostgresPool) -> Result<usize> {
    struct Row {
        id: String,
        workflow_id: String,
        status: String,
        current_step_id: Option<String>,
        step_history: String,
        variables: String,
        trigger_json: String,
        agent_id: Option<String>,
        created_at: DateTime<Utc>,
        updated_at: DateTime<Utc>,
        parent_instance_id: Option<String>,
        channel_id: Option<String>,
        thread_id: Option<String>,
        version: i64,
    }

    let versioned = sqlite.query_row(
        "SELECT COUNT(*) FROM pragma_table_info('workflow_instances') WHERE name = 'version'",
        [],
        |r| r.get::<_, i64>(0),
    )? > 0;
    let version = if versioned { "version" } else { "0" };
    let mut stmt = sqlite.prepare(&format!(
        "SELECT id, workflow_id, status, current_step_id, step_history, variables,
                trigger_json, agent_id, created_at, updated_at, parent_instance_id,
                channel_id, thread_id, {version}
         FROM workflow_instances"
    ))?;
    let rows: Vec<Row> = stmt
        .query_map([], |r| {
            let created_at: String = r.get(8)?;
            let updated_at: String = r.get(9)?;
            Ok(Row {
                id: r.get(0)?,
                workflow_id: r.get(1)?,
                status: r.get(2)?,
                current_step_id: r.get(3)?,
                step_history: r.get(4)?,
                variables: r.get(5)?,
                trigger_json: r.get(6)?,
                agent_id: r.get(7)?,
                created_at: parse_ts(&created_at).map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        8,
                        rusqlite::types::Type::Text,
                        e.into(),
                    )
                })?,
                updated_at: parse_ts(&updated_at).map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        9,
                        rusqlite::types::Type::Text,
                        e.into(),
                    )
                })?,
                parent_instance_id: r.get(10)?,
                channel_id: r.get(11)?,
                thread_id: r.get(12)?,
                version: r.get(13)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;

    let total = rows.len();
    if total == 0 {
        return Ok(0);
    }

    let conn = pool.get().await?;
    for chunk in rows.chunks(BATCH_SIZE) {
        let sql = format!(
            "INSERT INTO workflow_instances
             (id, workflow_id, status, current_step_id, step_history, variables,
              trigger_json, agent_id, created_at, updated_at, parent_instance_id,
              channel_id, thread_id, version)
             VALUES {}
             ON CONFLICT (id) DO NOTHING",
            placeholders(chunk.len(), 14)
        );
        let mut params: Vec<&(dyn ToSql + Sync)> = Vec::with_capacity(chunk.len() * 14);
        for r in chunk {
            params.push(&r.id);
            params.push(&r.workflow_id);
            params.push(&r.status);
            params.push(&r.current_step_id);
            params.push(&r.step_history);
            params.push(&r.variables);
            params.push(&r.trigger_json);
            params.push(&r.agent_id);
            params.push(&r.created_at);
            params.push(&r.updated_at);
            params.push(&r.parent_instance_id);
            params.push(&r.channel_id);
            params.push(&r.thread_id);
            params.push(&r.version);
        }
        conn.execute(&sql, &params)
            .await
            .context("insert workflow_instances")?;
    }
    Ok(total)
}

async fn migrate_security_events(sqlite: &Connection, pool: &PostgresPool) -> Result<usize> {
    type Row = (
        i64,
        DateTime<Utc>,
        String,
        String,
        Option<String>,
        String,
        Option<String>,
        String,
        Option<String>,
    );

    let mut stmt = sqlite.prepare(
        "SELECT id, timestamp, event_type, agent_id, session_id, severity,
                source, detail, content_hash
         FROM security_events",
    )?;
    let rows: Vec<Row> = stmt
        .query_map([], |r| {
            let ts: String = r.get(1)?;
            Ok((
                r.get(0)?,
                parse_ts(&ts).map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        1,
                        rusqlite::types::Type::Text,
                        e.into(),
                    )
                })?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
                r.get(7)?,
                r.get(8)?,
            ))
        })?
        .collect::<rusqlite::Result<_>>()?;

    let total = rows.len();
    if total == 0 {
        return Ok(0);
    }

    let conn = pool.get().await?;
    for chunk in rows.chunks(BATCH_SIZE) {
        let sql = format!(
            "INSERT INTO security_events
             (id, timestamp, event_type, agent_id, session_id, severity,
              source, detail, content_hash)
             VALUES {}
             ON CONFLICT (id) DO NOTHING",
            placeholders(chunk.len(), 9)
        );
        let mut params: Vec<&(dyn ToSql + Sync)> = Vec::with_capacity(chunk.len() * 9);
        for r in chunk {
            params.push(&r.0);
            params.push(&r.1);
            params.push(&r.2);
            params.push(&r.3);
            params.push(&r.4);
            params.push(&r.5);
            params.push(&r.6);
            params.push(&r.7);
            params.push(&r.8);
        }
        conn.execute(&sql, &params)
            .await
            .context("insert security_events")?;
    }
    bump_sequence(pool, "security_events", "id").await?;
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persistence::{
        CostRecord, RoundTelemetryRecord, SqliteStore, TaskRow, TaskTelemetryRecord,
        ToolTelemetryRecord,
    };
    use crate::telemetry::TelemetryStore;
    use crate::telemetry_postgres::tests::with_embedded_pg_pool;

    /// Populate a SQLite source with one row in every table that has
    /// a public `TelemetryStore` writer, then migrate to a fresh
    /// embedded Postgres and verify the row counts come through.
    #[tokio::test(flavor = "multi_thread")]
    async fn migrates_all_tables_into_postgres() {
        with_embedded_pg_pool(|pool| async move {
            // ── Source: a SQLite store seeded with at least one row in
            // every migrated table.
            let tmp = tempfile::NamedTempFile::new().unwrap();
            let sqlite = SqliteStore::open(tmp.path()).unwrap();
            let trait_src: Arc<dyn TelemetryStore> = Arc::new(sqlite);

            let now = Utc::now();
            trait_src
                .save_task(&TaskRow {
                    id: "task-mig-1".into(),
                    title: "migrate me".into(),
                    description: "test".into(),
                    status: "created".into(),
                    priority: "normal".into(),
                    assignee: Some("alice".into()),
                    team_id: Some("team-a".into()),
                    labels: "[]".into(),
                    parent_id: None,
                    result_json: None,
                    created_at: now.to_rfc3339(),
                    updated_at: now.to_rfc3339(),
                    started_at: None,
                    completed_at: None,
                })
                .unwrap();

            trait_src
                .record_audit(
                    "alice",
                    "test.action",
                    "resource-1",
                    Some("hello"),
                    crate::admin::AuditOutcome::Success,
                )
                .unwrap();

            trait_src
                .record_cost(&CostRecord {
                    agent_id: "agent-a".into(),
                    model: "claude-opus-4-6".into(),
                    input_tokens: 100,
                    output_tokens: 50,
                    cache_creation_input_tokens: 0,
                    cache_read_input_tokens: 0,
                    cost_usd: 0.001,
                    team_id: None,
                    task_id: None,
                    task_type: None,
                    ticket_id: Some("ticket-1".into()),
                    identity_id: None,
                    description: Some("migrate test cost".into()),
                    ..Default::default()
                })
                .unwrap();

            trait_src
                .record_task_telemetry(
                    &TaskTelemetryRecord {
                        agent_id: "agent-a".into(),
                        model: "claude-opus-4-6".into(),
                        provider: "anthropic".into(),
                        input_tokens: 100,
                        output_tokens: 50,
                        cache_creation_input_tokens: 0,
                        cache_read_input_tokens: 0,
                        cost_usd: 0.001,
                        rounds: 1,
                        duration_ms: 1234,
                        tool_invocation_count: 1,
                        success: true,
                        error_type: None,
                        retry_count: 0,
                        task_type: Some("code".into()),
                        task_id: Some("t-1".into()),
                        started_at: now.to_rfc3339(),
                        completed_at: now.to_rfc3339(),
                        forecast_cost_usd: None,
                        forecast_rounds: None,
                        forecast_confidence: None,
                        team_id: None,
                        session_id: None,
                        model_switches: None,
                        ticket_id: Some("ticket-1".into()),
                        identity_id: None,
                        description: Some("migrate test task".into()),
                    },
                    &[RoundTelemetryRecord {
                        round_number: 1,
                        input_tokens: 100,
                        output_tokens: 50,
                        latency_ms: 1234,
                        cache_creation_tokens: 0,
                        cache_read_tokens: 0,
                        prompt_size_bytes: 200,
                        response_size_bytes: 100,
                        tool_count: 1,
                        stop_reason: "end_turn".into(),
                    }],
                    &[ToolTelemetryRecord {
                        round_number: 1,
                        tool_name: "Read".into(),
                        success: true,
                        duration_ms: 50,
                        policy_decision: "allow".into(),
                        input_size_bytes: 50,
                        output_size_bytes: 100,
                        denial_reason: None,
                    }],
                )
                .unwrap();

            trait_src
                .upsert_model_pricing(&crate::persistence::StoredModelPricing {
                    model: "claude-opus-4-6".into(),
                    provider: "anthropic".into(),
                    input_per_million: 15.0,
                    output_per_million: 75.0,
                    source: "default".into(),
                    fetched_at: now.to_rfc3339(),
                    updated_at: now.to_rfc3339(),
                })
                .unwrap();

            // The source is on a Mutex<Connection>; drop the trait
            // object so we can re-open the SQLite file for migration.
            drop(trait_src);

            // ── Migrate.
            let report = migrate_sqlite_to_postgres_with_pool(tmp.path(), pool.clone())
                .await
                .expect("migration succeeds");

            assert_eq!(report.tasks, 1);
            assert_eq!(report.audit_log, 1);
            assert_eq!(report.cost_records, 1);
            assert_eq!(report.task_telemetry, 1);
            assert_eq!(report.round_telemetry, 1);
            assert_eq!(report.tool_telemetry, 1);
            assert_eq!(report.model_pricing, 1);

            // ── Verify by reading through the destination store.
            let dest = PostgresTelemetryStore::with_pool(pool.clone())
                .await
                .unwrap();
            let trait_dest: Arc<dyn TelemetryStore> = dest;

            let tasks = trait_dest.list_tasks(None).unwrap();
            assert_eq!(tasks.len(), 1);
            assert_eq!(tasks[0].id, "task-mig-1");

            let audits = trait_dest.recent_audit(10).unwrap();
            assert_eq!(audits.len(), 1);
            assert_eq!(audits[0].actor, "alice");

            let total = trait_dest
                .total_cost(
                    now - chrono::Duration::hours(1),
                    now + chrono::Duration::hours(1),
                )
                .unwrap();
            assert!((total - 0.001).abs() < 1e-9);

            let history = trait_dest
                .get_task_history("agent-a", 10, None, None)
                .unwrap();
            assert_eq!(history.len(), 1);
            assert_eq!(history[0].agent_id, "agent-a");

            let pricing = trait_dest.list_model_pricing().unwrap();
            assert_eq!(pricing.len(), 1);

            // ── Idempotency: re-running on the same destination must
            // skip every row (ON CONFLICT DO NOTHING) and not blow up.
            let report2 = migrate_sqlite_to_postgres_with_pool(tmp.path(), pool)
                .await
                .expect("migration is idempotent");
            assert_eq!(report2.tasks, 1);
            assert_eq!(report2.task_telemetry, 1);
            // Counts represent rows-read-from-source, not
            // rows-newly-inserted-into-dest, so the second run reports
            // the same totals.
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_migration_copies_the_version() {
        with_embedded_pg_pool(|pool| async move {
            let tmp = tempfile::NamedTempFile::new().unwrap();
            let sqlite = SqliteStore::open(tmp.path()).unwrap();
            let run = crate::persistence::WorkflowInstanceRow {
                id: "run-1".into(),
                workflow_id: "daily-digest-email".into(),
                status: "paused".into(),
                step_history: "[]".into(),
                variables: "{}".into(),
                trigger_json: "{}".into(),
                created_at: Utc::now().to_rfc3339(),
                updated_at: Utc::now().to_rfc3339(),
                ..Default::default()
            };
            sqlite.save_workflow_instance(&run).unwrap();
            sqlite.save_workflow_instance(&run).unwrap();
            sqlite.save_workflow_instance(&run).unwrap();
            let version = sqlite
                .get_workflow_instance("run-1")
                .unwrap()
                .unwrap()
                .version;
            drop(sqlite);

            let report = migrate_sqlite_to_postgres_with_pool(tmp.path(), pool.clone())
                .await
                .unwrap();

            assert_eq!(report.workflow_instances, 1);
            let dest: Arc<dyn TelemetryStore> =
                PostgresTelemetryStore::with_pool(pool).await.unwrap();
            let copied = dest.get_workflow_instance("run-1").unwrap().unwrap();
            assert_eq!((version, copied.version), (2, 2));
        })
        .await;
    }
}
