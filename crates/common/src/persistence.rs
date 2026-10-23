use std::path::Path;
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use eyre::Result;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

use crate::admin::{AuditEntry, AuditOutcome};
use crate::cost::TokenHistoryPoint;
use crate::subscription::{Billing, CredentialTier, Origin};

/// Builds an `AND`-joined `WHERE` clause and a positional parameter vector
/// in lock-step. Each `add` appends one predicate (with a single `?`
/// placeholder rewritten to `?N`) and one bound parameter — so the
/// rendered SQL and bound values can never drift out of sync. Owns its
/// values via boxing so callers don't need to manage borrow lifetimes
/// for short-lived strings or datetimes.
struct WhereBuilder {
    parts: Vec<String>,
    params: Vec<Box<dyn rusqlite::ToSql>>,
}

impl WhereBuilder {
    fn new() -> Self {
        Self {
            parts: Vec::new(),
            params: Vec::new(),
        }
    }

    /// Push one predicate. The string must contain exactly one `?`
    /// placeholder, which is renumbered to its 1-based parameter index.
    fn add(&mut self, predicate: &str, value: impl rusqlite::ToSql + 'static) {
        debug_assert_eq!(
            predicate.matches('?').count(),
            1,
            "WhereBuilder predicates must contain exactly one '?' placeholder"
        );
        let idx = self.params.len() + 1;
        self.parts
            .push(predicate.replacen('?', &format!("?{idx}"), 1));
        self.params.push(Box::new(value));
    }

    /// Push a predicate that has no bound parameters (e.g. an `IN`
    /// with literal values, or a `NOT NULL` check).
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

    fn params(&self) -> impl Iterator<Item = &dyn rusqlite::ToSql> {
        self.params.iter().map(|b| b.as_ref())
    }
}

/// The time window and scope every cost query over `cost_records` shares.
fn cost_scope_filter(from: DateTime<Utc>, to: DateTime<Utc>, scope: CostScope<'_>) -> WhereBuilder {
    let mut filter = WhereBuilder::new();
    filter.add("timestamp >= ?", from.to_rfc3339());
    filter.add("timestamp <= ?", to.to_rfc3339());
    if let Some(agent_id) = scope.agent_id {
        filter.add("agent_id = ?", agent_id.to_string());
    }
    if let Some(user_id) = scope.user_id {
        filter.add("identity_id = ?", user_id.to_string());
    }
    filter
}

/// SQLite-backed persistent storage for the workforce hub.
///
/// Stores tasks, audit log entries, and cost records so state
/// survives hub restarts.
pub struct SqliteStore {
    pub(crate) conn: Mutex<Connection>,
}

impl SqliteStore {
    /// Open (or create) a SQLite database at the given path.
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)?;
        let store = Self {
            conn: Mutex::new(conn),
        };
        store.migrate()?;
        Ok(store)
    }

    /// Open an existing database for reading only: nothing is created or
    /// migrated, so an offline reader cannot change a live install.
    pub fn open_read_only(path: &Path) -> Result<Self> {
        let conn = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Create an in-memory database (for testing).
    pub fn in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        let store = Self {
            conn: Mutex::new(conn),
        };
        store.migrate()?;
        Ok(store)
    }

    /// Run schema migrations.
    fn migrate(&self) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {}", e))?;
        conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS tasks (
                id TEXT PRIMARY KEY,
                title TEXT NOT NULL,
                description TEXT NOT NULL,
                status TEXT NOT NULL DEFAULT 'created',
                priority TEXT NOT NULL DEFAULT 'normal',
                assignee TEXT,
                team_id TEXT,
                labels TEXT NOT NULL DEFAULT '[]',
                parent_id TEXT,
                result_json TEXT,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                started_at TEXT,
                completed_at TEXT
            );

            CREATE TABLE IF NOT EXISTS audit_log (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                timestamp TEXT NOT NULL,
                actor TEXT NOT NULL,
                action TEXT NOT NULL,
                resource TEXT NOT NULL,
                detail TEXT,
                outcome TEXT NOT NULL DEFAULT 'success'
            );

            CREATE TABLE IF NOT EXISTS cost_records (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                timestamp TEXT NOT NULL,
                agent_id TEXT NOT NULL,
                model TEXT NOT NULL,
                input_tokens INTEGER NOT NULL,
                output_tokens INTEGER NOT NULL,
                cache_creation_input_tokens INTEGER NOT NULL DEFAULT 0,
                cache_read_input_tokens INTEGER NOT NULL DEFAULT 0,
                cost_usd REAL NOT NULL,
                team_id TEXT,
                task_id TEXT,
                task_type TEXT
            );

            CREATE INDEX IF NOT EXISTS idx_tasks_status ON tasks(status);
            CREATE INDEX IF NOT EXISTS idx_tasks_team ON tasks(team_id);
            CREATE INDEX IF NOT EXISTS idx_audit_timestamp ON audit_log(timestamp);
            CREATE INDEX IF NOT EXISTS idx_audit_actor ON audit_log(actor);
            CREATE INDEX IF NOT EXISTS idx_cost_timestamp ON cost_records(timestamp);
            CREATE INDEX IF NOT EXISTS idx_cost_agent ON cost_records(agent_id);

            CREATE TABLE IF NOT EXISTS task_telemetry (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                agent_id TEXT NOT NULL,
                model TEXT NOT NULL,
                provider TEXT NOT NULL DEFAULT 'anthropic',
                input_tokens INTEGER NOT NULL,
                output_tokens INTEGER NOT NULL,
                cache_creation_input_tokens INTEGER NOT NULL DEFAULT 0,
                cache_read_input_tokens INTEGER NOT NULL DEFAULT 0,
                cost_usd REAL NOT NULL,
                rounds INTEGER NOT NULL,
                duration_ms INTEGER NOT NULL,
                tool_invocation_count INTEGER NOT NULL DEFAULT 0,
                success INTEGER NOT NULL DEFAULT 1,
                error_type TEXT,
                retry_count INTEGER NOT NULL DEFAULT 0,
                task_type TEXT,
                task_id TEXT,
                started_at TEXT NOT NULL,
                completed_at TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS round_telemetry (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                task_telemetry_id INTEGER NOT NULL REFERENCES task_telemetry(id),
                round_number INTEGER NOT NULL,
                input_tokens INTEGER NOT NULL,
                output_tokens INTEGER NOT NULL,
                latency_ms INTEGER NOT NULL,
                cache_creation_tokens INTEGER NOT NULL DEFAULT 0,
                cache_read_tokens INTEGER NOT NULL DEFAULT 0,
                prompt_size_bytes INTEGER NOT NULL DEFAULT 0,
                response_size_bytes INTEGER NOT NULL DEFAULT 0,
                tool_count INTEGER NOT NULL DEFAULT 0,
                stop_reason TEXT NOT NULL DEFAULT 'end_turn'
            );

            CREATE TABLE IF NOT EXISTS tool_telemetry (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                task_telemetry_id INTEGER NOT NULL REFERENCES task_telemetry(id),
                round_number INTEGER NOT NULL,
                tool_name TEXT NOT NULL,
                success INTEGER NOT NULL DEFAULT 1,
                duration_ms INTEGER NOT NULL DEFAULT 0,
                policy_decision TEXT NOT NULL DEFAULT 'allow',
                input_size_bytes INTEGER NOT NULL DEFAULT 0,
                output_size_bytes INTEGER NOT NULL DEFAULT 0
            );

            CREATE TABLE IF NOT EXISTS model_health_events (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                timestamp TEXT NOT NULL,
                model TEXT NOT NULL,
                event_type TEXT NOT NULL,
                detail TEXT,
                agent_id TEXT
            );

            CREATE TABLE IF NOT EXISTS agent_model_preferences (
                agent_id TEXT NOT NULL,
                task_type TEXT NOT NULL,
                preferred_model TEXT NOT NULL,
                reason TEXT,
                updated_at TEXT NOT NULL,
                PRIMARY KEY (agent_id, task_type)
            );

            CREATE TABLE IF NOT EXISTS lesson_events (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                timestamp TEXT NOT NULL,
                agent_id TEXT NOT NULL,
                identity_id TEXT,
                lesson_id TEXT,
                event_type TEXT NOT NULL,
                learning_kind TEXT,
                recall_score REAL,
                importance_delta REAL,
                confidence_after REAL,
                applied_after INTEGER,
                session_id TEXT,
                trigger_tags TEXT
            );

            CREATE TABLE IF NOT EXISTS relationship_events (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                timestamp TEXT NOT NULL,
                agent_id TEXT NOT NULL,
                identity_id TEXT NOT NULL,
                session_id TEXT NOT NULL,
                message_id TEXT NOT NULL,
                valence REAL NOT NULL,
                intensity REAL NOT NULL,
                emotion_label TEXT,
                reaction TEXT NOT NULL,
                is_key_moment INTEGER NOT NULL,
                confidence REAL NOT NULL,
                source TEXT NOT NULL,
                signals TEXT NOT NULL,
                UNIQUE(session_id, message_id)
            );

            CREATE TABLE IF NOT EXISTS relationship_cursor (
                session_id TEXT PRIMARY KEY,
                agent_id TEXT NOT NULL,
                last_message_ts TEXT NOT NULL,
                last_message_id TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_task_telemetry_agent ON task_telemetry(agent_id);
            CREATE INDEX IF NOT EXISTS idx_task_telemetry_model ON task_telemetry(model);
            CREATE INDEX IF NOT EXISTS idx_task_telemetry_completed ON task_telemetry(completed_at);
            CREATE INDEX IF NOT EXISTS idx_task_telemetry_task_type ON task_telemetry(task_type);
            CREATE INDEX IF NOT EXISTS idx_task_telemetry_agent_model ON task_telemetry(agent_id, model);
            CREATE INDEX IF NOT EXISTS idx_round_telemetry_task ON round_telemetry(task_telemetry_id);
            CREATE INDEX IF NOT EXISTS idx_tool_telemetry_task ON tool_telemetry(task_telemetry_id);
            CREATE INDEX IF NOT EXISTS idx_model_health_model ON model_health_events(model);
            CREATE INDEX IF NOT EXISTS idx_model_health_timestamp ON model_health_events(timestamp);
            CREATE INDEX IF NOT EXISTS idx_lesson_events_agent_ts ON lesson_events(agent_id, timestamp);
            CREATE INDEX IF NOT EXISTS idx_lesson_events_type_ts ON lesson_events(event_type, timestamp);
            CREATE INDEX IF NOT EXISTS idx_relationship_events_agent_identity_ts ON relationship_events(agent_id, identity_id, timestamp);
            CREATE INDEX IF NOT EXISTS idx_relationship_events_identity_ts ON relationship_events(identity_id, timestamp);
            ",
        )?;

        // Migration: add task_type to cost_records for existing databases.
        // Silently ignore if column already exists.
        let _ = conn.execute_batch("ALTER TABLE cost_records ADD COLUMN task_type TEXT;");

        // Migration: add cost forecasting columns to task_telemetry (DW-249)
        let _ = conn.execute_batch(
            "ALTER TABLE task_telemetry ADD COLUMN forecast_cost_usd REAL;
             ALTER TABLE task_telemetry ADD COLUMN forecast_rounds INTEGER;
             ALTER TABLE task_telemetry ADD COLUMN forecast_confidence REAL;",
        );

        // Migration: add team_id and session_id to task_telemetry (DW-217)
        let _ = conn.execute_batch(
            "ALTER TABLE task_telemetry ADD COLUMN team_id TEXT;
             ALTER TABLE task_telemetry ADD COLUMN session_id TEXT;
             CREATE INDEX IF NOT EXISTS idx_task_telemetry_team ON task_telemetry(team_id);
             CREATE INDEX IF NOT EXISTS idx_task_telemetry_session ON task_telemetry(session_id);",
        );

        // DW-267: model switches tracking
        conn.execute(
            "ALTER TABLE task_telemetry ADD COLUMN model_switches TEXT",
            [],
        )
        .ok();

        // DW-187: structured denial reasons
        conn.execute(
            "ALTER TABLE tool_telemetry ADD COLUMN denial_reason TEXT",
            [],
        )
        .ok();

        // DW-189: policy proposals store
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS policy_proposals (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                agent_id TEXT NOT NULL,
                proposed_rule TEXT NOT NULL,
                justification TEXT NOT NULL,
                status TEXT NOT NULL DEFAULT 'pending',
                reviewed_by TEXT,
                review_note TEXT,
                created_at TEXT NOT NULL,
                reviewed_at TEXT
            );
            CREATE INDEX IF NOT EXISTS idx_proposals_status ON policy_proposals(status);
            CREATE INDEX IF NOT EXISTS idx_proposals_agent ON policy_proposals(agent_id);",
        )?;

        // DW-334: model pricing management
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS model_pricing (
                model TEXT PRIMARY KEY,
                provider TEXT NOT NULL,
                input_per_million REAL NOT NULL,
                output_per_million REAL NOT NULL,
                source TEXT NOT NULL DEFAULT 'default',
                fetched_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );",
        )?;

        // DW-357: workflow instances
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS workflow_instances (
                id TEXT PRIMARY KEY,
                workflow_id TEXT NOT NULL,
                status TEXT NOT NULL DEFAULT 'running',
                current_step_id TEXT,
                step_history TEXT NOT NULL DEFAULT '[]',
                variables TEXT NOT NULL DEFAULT '{}',
                trigger_json TEXT NOT NULL,
                agent_id TEXT,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_workflow_instances_status ON workflow_instances(status);
            CREATE INDEX IF NOT EXISTS idx_workflow_instances_workflow ON workflow_instances(workflow_id);",
        )?;

        // Migration: add parent_instance_id for cascading abort support
        let _ = conn.execute_batch(
            "ALTER TABLE workflow_instances ADD COLUMN parent_instance_id TEXT;
             CREATE INDEX IF NOT EXISTS idx_workflow_instances_parent ON workflow_instances(parent_instance_id);",
        );

        // Migration: first-class channel/thread columns for O(1) workflow command routing.
        let _ = conn.execute_batch("ALTER TABLE workflow_instances ADD COLUMN channel_id TEXT;");
        let _ = conn.execute_batch("ALTER TABLE workflow_instances ADD COLUMN thread_id TEXT;");
        let _ = conn.execute_batch("CREATE INDEX IF NOT EXISTS idx_workflow_instances_thread ON workflow_instances(channel_id, thread_id);");

        // Migration: cross-instance execution ownership (lease) columns.
        let _ = conn.execute_batch(
            "ALTER TABLE workflow_instances ADD COLUMN owner_instance TEXT;
             ALTER TABLE workflow_instances ADD COLUMN lease_expires_at TEXT;",
        );
        let _ = conn.execute_batch(
            "ALTER TABLE workflow_instances ADD COLUMN version INTEGER NOT NULL DEFAULT 0;",
        );

        // DW-400: per-ticket cost tracking
        let _ = conn.execute_batch(
            "ALTER TABLE task_telemetry ADD COLUMN ticket_id TEXT;
             ALTER TABLE cost_records ADD COLUMN ticket_id TEXT;
             CREATE INDEX IF NOT EXISTS idx_task_telemetry_ticket ON task_telemetry(ticket_id);
             CREATE INDEX IF NOT EXISTS idx_cost_records_ticket ON cost_records(ticket_id);",
        );

        // Cache-aware cost tracking: bill cache reads + writes separately.
        let _ = conn.execute_batch(
            "ALTER TABLE cost_records ADD COLUMN cache_creation_input_tokens INTEGER NOT NULL DEFAULT 0;
             ALTER TABLE cost_records ADD COLUMN cache_read_input_tokens INTEGER NOT NULL DEFAULT 0;",
        );

        // Per-identity stats: capture which user triggered each task / cost
        // record so the UI can drill into per-identity usage. NULL on rows
        // written before this migration and on system-initiated tasks.
        let _ = conn.execute_batch(
            "ALTER TABLE cost_records   ADD COLUMN identity_id TEXT;
             ALTER TABLE task_telemetry ADD COLUMN identity_id TEXT;
             CREATE INDEX IF NOT EXISTS idx_cost_records_identity   ON cost_records(identity_id);
             CREATE INDEX IF NOT EXISTS idx_task_telemetry_identity ON task_telemetry(identity_id);",
        );

        // Budget-enforcement scope lookups: "spend today for this scope" and
        // per-job cumulative spend must be indexed — the governor checks them
        // on the turn admission path.
        let _ = conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS idx_cost_identity_ts ON cost_records(identity_id, timestamp);
             CREATE INDEX IF NOT EXISTS idx_cost_agent_ts ON cost_records(agent_id, timestamp);
             CREATE INDEX IF NOT EXISTS idx_cost_task ON cost_records(task_id);",
        );

        // Per-call description for cost attribution UI. Records the high-level
        // intent of an LLM call ("vision:gpt-4o", "transcription:42s", a
        // truncated task prompt, etc.) so operators can answer "what was this
        // $0.04 for?" without joining back to session history.
        let _ = conn.execute_batch(
            "ALTER TABLE cost_records   ADD COLUMN description TEXT;
             ALTER TABLE task_telemetry ADD COLUMN description TEXT;",
        );

        // What caused each call and what paid for it. Mirrors telemetry
        // V020 and V021: rows from before it are backfilled once, by model
        // prefix, when the billing column is first added.
        if conn
            .execute_batch(
                "ALTER TABLE cost_records ADD COLUMN billing TEXT NOT NULL DEFAULT 'metered';",
            )
            .is_ok()
        {
            conn.execute_batch(SUBSCRIPTION_BILLING_BACKFILL)?;
        }
        for column in ["origin", "payer", "credential_tier"] {
            let _ = conn.execute_batch(&format!(
                "ALTER TABLE cost_records ADD COLUMN {column} TEXT;"
            ));
        }
        conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS idx_cost_billing_ts ON cost_records(billing, timestamp);",
        )?;
        // Mirrors telemetry V022: a call recorded again under its ledger key
        // (a durable round collected after a restart) is kept once.
        let _ = conn.execute_batch("ALTER TABLE cost_records ADD COLUMN ledger_key TEXT;");
        conn.execute_batch(
            "CREATE UNIQUE INDEX IF NOT EXISTS idx_cost_ledger_key ON cost_records(ledger_key) WHERE ledger_key IS NOT NULL;",
        )?;

        // DW-202: security event audit log
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS security_events (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                timestamp TEXT NOT NULL,
                event_type TEXT NOT NULL,
                agent_id TEXT NOT NULL,
                session_id TEXT,
                severity TEXT NOT NULL,
                source TEXT,
                detail TEXT NOT NULL,
                content_hash TEXT
            );
            CREATE INDEX IF NOT EXISTS idx_security_events_timestamp ON security_events(timestamp);
            CREATE INDEX IF NOT EXISTS idx_security_events_type ON security_events(event_type);
            CREATE INDEX IF NOT EXISTS idx_security_events_agent ON security_events(agent_id);
            CREATE INDEX IF NOT EXISTS idx_security_events_severity ON security_events(severity);",
        )?;

        Ok(())
    }

    // ─── Tasks ───

    /// Save a task (insert or update).
    pub fn save_task(&self, task: &TaskRow) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {}", e))?;
        conn.execute(
            "INSERT OR REPLACE INTO tasks
             (id, title, description, status, priority, assignee, team_id, labels,
              parent_id, result_json, created_at, updated_at, started_at, completed_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
            params![
                task.id,
                task.title,
                task.description,
                task.status,
                task.priority,
                task.assignee,
                task.team_id,
                task.labels,
                task.parent_id,
                task.result_json,
                task.created_at,
                task.updated_at,
                task.started_at,
                task.completed_at,
            ],
        )?;
        Ok(())
    }

    /// Get a task by ID.
    pub fn get_task(&self, id: &str) -> Result<Option<TaskRow>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {}", e))?;
        let mut stmt = conn.prepare(
            "SELECT id, title, description, status, priority, assignee, team_id, labels,
                    parent_id, result_json, created_at, updated_at, started_at, completed_at
             FROM tasks WHERE id = ?1",
        )?;

        let row = stmt.query_row(params![id], |row| {
            Ok(TaskRow {
                id: row.get(0)?,
                title: row.get(1)?,
                description: row.get(2)?,
                status: row.get(3)?,
                priority: row.get(4)?,
                assignee: row.get(5)?,
                team_id: row.get(6)?,
                labels: row.get(7)?,
                parent_id: row.get(8)?,
                result_json: row.get(9)?,
                created_at: row.get(10)?,
                updated_at: row.get(11)?,
                started_at: row.get(12)?,
                completed_at: row.get(13)?,
            })
        });

        match row {
            Ok(task) => Ok(Some(task)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// List tasks, optionally filtered by status.
    pub fn list_tasks(&self, status: Option<&str>) -> Result<Vec<TaskRow>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {}", e))?;
        let sql = if status.is_some() {
            "SELECT id, title, description, status, priority, assignee, team_id, labels,
                    parent_id, result_json, created_at, updated_at, started_at, completed_at
             FROM tasks WHERE status = ?1 ORDER BY created_at DESC"
        } else {
            "SELECT id, title, description, status, priority, assignee, team_id, labels,
                    parent_id, result_json, created_at, updated_at, started_at, completed_at
             FROM tasks ORDER BY created_at DESC"
        };

        let mut stmt = conn.prepare(sql)?;
        let rows = if let Some(s) = status {
            stmt.query_map(params![s], row_to_task)?
                .collect::<std::result::Result<Vec<_>, _>>()?
        } else {
            stmt.query_map([], row_to_task)?
                .collect::<std::result::Result<Vec<_>, _>>()?
        };

        Ok(rows)
    }

    /// Update task status.
    pub fn update_task_status(&self, id: &str, status: &str) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {}", e))?;
        let now = Utc::now().to_rfc3339();
        conn.execute(
            "UPDATE tasks SET status = ?1, updated_at = ?2 WHERE id = ?3",
            params![status, now, id],
        )?;
        Ok(())
    }

    // ─── Audit Log ───

    /// Record an audit entry.
    pub fn record_audit(
        &self,
        actor: &str,
        action: &str,
        resource: &str,
        detail: Option<&str>,
        outcome: AuditOutcome,
    ) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {}", e))?;
        let now = Utc::now().to_rfc3339();
        let outcome_str = match outcome {
            AuditOutcome::Success => "success",
            AuditOutcome::Denied => "denied",
            AuditOutcome::Error => "error",
        };
        conn.execute(
            "INSERT INTO audit_log (timestamp, actor, action, resource, detail, outcome)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![now, actor, action, resource, detail, outcome_str],
        )?;
        Ok(())
    }

    /// Get recent audit entries.
    pub fn recent_audit(&self, limit: usize) -> Result<Vec<AuditEntry>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {}", e))?;
        let mut stmt = conn.prepare(
            "SELECT timestamp, actor, action, resource, detail, outcome
             FROM audit_log ORDER BY id DESC LIMIT ?1",
        )?;
        let rows = stmt
            .query_map(params![limit as i64], row_to_audit)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Search audit log.
    pub fn search_audit(&self, query: &str, limit: usize) -> Result<Vec<AuditEntry>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {}", e))?;
        let pattern = format!("%{}%", query);
        let mut stmt = conn.prepare(
            "SELECT timestamp, actor, action, resource, detail, outcome
             FROM audit_log
             WHERE actor LIKE ?1 OR action LIKE ?1 OR resource LIKE ?1 OR detail LIKE ?1
             ORDER BY id DESC LIMIT ?2",
        )?;
        let rows = stmt
            .query_map(params![pattern, limit as i64], row_to_audit)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    // ─── Cost Records ───

    /// Record a cost entry, returning whether it was inserted: a row whose
    /// ledger key is already recorded is not.
    pub fn record_cost(&self, record: &CostRecord) -> Result<bool> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {}", e))?;
        let now = Utc::now().to_rfc3339();
        let inserted = conn.execute(
            "INSERT INTO cost_records (timestamp, agent_id, model, input_tokens, output_tokens, cache_creation_input_tokens, cache_read_input_tokens, cost_usd, team_id, task_id, task_type, ticket_id, identity_id, description, origin, billing, payer, credential_tier, ledger_key)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19)
             ON CONFLICT (ledger_key) WHERE ledger_key IS NOT NULL DO NOTHING",
            params![
                now,
                record.agent_id,
                record.model,
                record.input_tokens as i64,
                record.output_tokens as i64,
                record.cache_creation_input_tokens as i64,
                record.cache_read_input_tokens as i64,
                record.cost_usd,
                record.team_id,
                record.task_id,
                record.task_type,
                record.ticket_id,
                record.identity_id,
                record.description,
                record.origin_str(),
                record.billing.as_str(),
                record.payer,
                record.credential_tier_str(),
                record.ledger_key,
            ],
        )?;
        Ok(inserted > 0)
    }

    /// Get total cost for a time range.
    pub fn total_cost(&self, from: DateTime<Utc>, to: DateTime<Utc>) -> Result<f64> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {}", e))?;
        let mut stmt = conn.prepare(
            "SELECT COALESCE(SUM(cost_usd), 0.0) FROM cost_records
             WHERE timestamp >= ?1 AND timestamp <= ?2",
        )?;
        let total: f64 = stmt.query_row(params![from.to_rfc3339(), to.to_rfc3339()], |row| {
            row.get(0)
        })?;
        Ok(total)
    }

    /// Get cost grouped by agent.
    pub fn cost_by_agent(
        &self,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<Vec<(String, f64)>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {}", e))?;
        let mut stmt = conn.prepare(
            "SELECT agent_id, SUM(cost_usd) FROM cost_records
             WHERE timestamp >= ?1 AND timestamp <= ?2
             GROUP BY agent_id ORDER BY SUM(cost_usd) DESC",
        )?;
        let rows = stmt
            .query_map(params![from.to_rfc3339(), to.to_rfc3339()], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, f64>(1)?))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Get cost grouped by model.
    pub fn cost_by_model(
        &self,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<Vec<(String, f64)>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {}", e))?;
        let mut stmt = conn.prepare(
            "SELECT model, SUM(cost_usd) FROM cost_records
             WHERE timestamp >= ?1 AND timestamp <= ?2
             GROUP BY model ORDER BY SUM(cost_usd) DESC",
        )?;
        let rows = stmt
            .query_map(params![from.to_rfc3339(), to.to_rfc3339()], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, f64>(1)?))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Get total token counts for a time range.
    pub fn cost_token_totals(
        &self,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<(u64, u64, usize)> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {}", e))?;
        let mut stmt = conn.prepare(
            "SELECT COALESCE(SUM(input_tokens), 0), COALESCE(SUM(output_tokens), 0), COUNT(*)
             FROM cost_records WHERE timestamp >= ?1 AND timestamp <= ?2",
        )?;
        let (input, output, count): (i64, i64, usize) = stmt
            .query_row(params![from.to_rfc3339(), to.to_rfc3339()], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })?;
        Ok((input as u64, output as u64, count))
    }

    /// Get token totals grouped by UTC day. Returns only days that have records;
    /// callers are expected to zero-fill any missing days they want to display.
    pub fn cost_token_history_by_day(
        &self,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<Vec<TokenHistoryPoint>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {}", e))?;
        let mut stmt = conn.prepare(
            "SELECT substr(timestamp, 1, 10) AS day,
                    COALESCE(SUM(input_tokens), 0),
                    COALESCE(SUM(output_tokens), 0),
                    COALESCE(SUM(cache_creation_input_tokens), 0),
                    COALESCE(SUM(cache_read_input_tokens), 0)
             FROM cost_records
             WHERE timestamp >= ?1 AND timestamp <= ?2
             GROUP BY day
             ORDER BY day ASC",
        )?;
        let rows = stmt
            .query_map(params![from.to_rfc3339(), to.to_rfc3339()], |row| {
                let day: String = row.get(0)?;
                let bucket_start = chrono::NaiveDate::parse_from_str(&day, "%Y-%m-%d")
                    .map_err(|e| {
                        rusqlite::Error::FromSqlConversionFailure(
                            0,
                            rusqlite::types::Type::Text,
                            Box::new(e),
                        )
                    })?
                    .and_hms_opt(0, 0, 0)
                    .unwrap()
                    .and_utc();
                Ok(TokenHistoryPoint {
                    bucket_start,
                    input_tokens: row.get::<_, i64>(1)? as u64,
                    output_tokens: row.get::<_, i64>(2)? as u64,
                    cache_creation_input_tokens: row.get::<_, i64>(3)? as u64,
                    cache_read_input_tokens: row.get::<_, i64>(4)? as u64,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn cost_token_history_by_month(
        &self,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<Vec<TokenHistoryPoint>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {}", e))?;
        let mut stmt = conn.prepare(
            "SELECT substr(timestamp, 1, 7) AS month,
                    COALESCE(SUM(input_tokens), 0),
                    COALESCE(SUM(output_tokens), 0),
                    COALESCE(SUM(cache_creation_input_tokens), 0),
                    COALESCE(SUM(cache_read_input_tokens), 0)
             FROM cost_records
             WHERE timestamp >= ?1 AND timestamp <= ?2
             GROUP BY month
             ORDER BY month ASC",
        )?;
        let rows = stmt
            .query_map(params![from.to_rfc3339(), to.to_rfc3339()], |row| {
                let month: String = row.get(0)?;
                let bucket_start =
                    chrono::NaiveDate::parse_from_str(&format!("{}-01", month), "%Y-%m-%d")
                        .map_err(|e| {
                            rusqlite::Error::FromSqlConversionFailure(
                                0,
                                rusqlite::types::Type::Text,
                                Box::new(e),
                            )
                        })?
                        .and_hms_opt(0, 0, 0)
                        .unwrap()
                        .and_utc();
                Ok(TokenHistoryPoint {
                    bucket_start,
                    input_tokens: row.get::<_, i64>(1)? as u64,
                    output_tokens: row.get::<_, i64>(2)? as u64,
                    cache_creation_input_tokens: row.get::<_, i64>(3)? as u64,
                    cache_read_input_tokens: row.get::<_, i64>(4)? as u64,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Get cost breakdown by provider from task telemetry.
    pub fn cost_by_provider(
        &self,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<Vec<(String, f64)>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {}", e))?;
        let mut stmt = conn.prepare(
            "SELECT COALESCE(provider, 'unknown'), SUM(cost_usd)
             FROM task_telemetry
             WHERE completed_at >= ?1 AND completed_at <= ?2
             GROUP BY provider ORDER BY SUM(cost_usd) DESC",
        )?;
        let rows = stmt
            .query_map(params![from.to_rfc3339(), to.to_rfc3339()], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, f64>(1)?))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Get aggregated usage summary for a team, grouped by agent.
    pub fn get_team_usage_summary(
        &self,
        team_id: &str,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<Vec<AgentUsageSummary>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {}", e))?;

        let mut stmt = conn.prepare(
            "SELECT agent_id as group_key,
                    COUNT(*) as total_tasks,
                    SUM(input_tokens) as total_input,
                    SUM(output_tokens) as total_output,
                    SUM(cost_usd) as total_cost,
                    AVG(rounds) as avg_rounds,
                    AVG(duration_ms) as avg_duration,
                    AVG(CASE WHEN success THEN 1.0 ELSE 0.0 END) as success_rate,
                    SUM(cache_creation_input_tokens) as total_cache_created,
                    SUM(cache_read_input_tokens) as total_cache_read
             FROM task_telemetry
             WHERE team_id = ?1 AND completed_at >= ?2 AND completed_at <= ?3
             GROUP BY agent_id
             ORDER BY total_cost DESC",
        )?;
        let rows = stmt
            .query_map(
                params![team_id, from.to_rfc3339(), to.to_rfc3339()],
                |row| {
                    Ok(AgentUsageSummary {
                        group_key: row.get(0)?,
                        total_tasks: row.get::<_, i64>(1)? as u64,
                        total_input_tokens: row.get::<_, i64>(2)? as u64,
                        total_output_tokens: row.get::<_, i64>(3)? as u64,
                        total_cost: row.get(4)?,
                        avg_rounds: row.get(5)?,
                        avg_duration_ms: row.get(6)?,
                        success_rate: row.get(7)?,
                        total_cache_created_tokens: row.get::<_, i64>(8)? as u64,
                        total_cache_read_tokens: row.get::<_, i64>(9)? as u64,
                    })
                },
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Get total cost for a team in a time range.
    pub fn get_team_cost(
        &self,
        team_id: &str,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<f64> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {}", e))?;
        let cost: f64 = conn.query_row(
            "SELECT COALESCE(SUM(cost_usd), 0.0) FROM cost_records
             WHERE team_id = ?1 AND timestamp >= ?2 AND timestamp <= ?3",
            params![team_id, from.to_rfc3339(), to.to_rfc3339()],
            |row| row.get(0),
        )?;
        Ok(cost)
    }

    /// Get cost grouped by team in a time range.
    pub fn cost_by_team(
        &self,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<Vec<(String, f64)>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {}", e))?;
        let mut stmt = conn.prepare(
            "SELECT team_id, SUM(cost_usd) FROM cost_records
             WHERE team_id IS NOT NULL AND timestamp >= ?1 AND timestamp <= ?2
             GROUP BY team_id ORDER BY SUM(cost_usd) DESC",
        )?;
        let rows = stmt
            .query_map(params![from.to_rfc3339(), to.to_rfc3339()], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, f64>(1)?))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    // ─── Per-identity stats slices ───

    /// Total cost grouped by `identity_id` in the window. Rows with NULL
    /// identity (system-initiated tasks, pre-migration writes) collapse
    /// into a single `"unknown"` bucket.
    pub fn metered_cost_by_task_prefix(&self, prefix: &str) -> Result<f64> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {}", e))?;
        // Range predicate so `idx_cost_task` serves the lookup (a LIKE with
        // an ESCAPE clause disables SQLite's index optimization). Prefixes
        // are internal (`scheduler:{uuid}:`) — no wildcard chars to escape.
        let total = conn.query_row(
            "SELECT COALESCE(SUM(cost_usd), 0.0) FROM cost_records
             WHERE task_id >= ?1 AND task_id < ?2 AND billing = ?3",
            params![
                prefix,
                format!("{prefix}\u{10FFFF}"),
                Billing::Metered.as_str()
            ],
            |row| row.get::<_, f64>(0),
        )?;
        Ok(total)
    }

    /// Delete cost records made before `before`, but never a scheduled
    /// job's, one batch per statement so `record_cost` waits for one batch
    /// at most.
    pub fn prune_costs(&self, before: DateTime<Utc>) -> Result<usize> {
        let before = before.to_rfc3339();
        crate::telemetry::prune_in_batches(|| {
            Ok(self
                .conn
                .lock()
                .map_err(|e| eyre::eyre!("Lock poisoned: {}", e))?
                .execute(
                    "DELETE FROM cost_records WHERE id IN (
                         SELECT id FROM cost_records
                         WHERE timestamp < ?1
                           AND (task_id IS NULL OR substr(task_id, 1, length(?3)) <> ?3)
                         ORDER BY timestamp LIMIT ?2)",
                    params![
                        before,
                        crate::telemetry::PRUNE_BATCH as i64,
                        crate::telemetry::JOB_TASK_PREFIX
                    ],
                )?)
        })
    }

    pub fn metered_spend(
        &self,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<Vec<MeteredSpend>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {}", e))?;
        let mut stmt = conn.prepare(
            "SELECT agent_id, identity_id, SUM(cost_usd) FROM cost_records
             WHERE timestamp >= ?1 AND timestamp <= ?2 AND billing = ?3
             GROUP BY agent_id, identity_id",
        )?;
        let rows = stmt
            .query_map(
                params![
                    from.to_rfc3339(),
                    to.to_rfc3339(),
                    Billing::Metered.as_str()
                ],
                |row| {
                    Ok(MeteredSpend {
                        agent_id: row.get(0)?,
                        identity_id: row.get(1)?,
                        cost_usd: row.get(2)?,
                    })
                },
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn cost_by_identity(
        &self,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<Vec<(String, f64)>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {}", e))?;
        let mut stmt = conn.prepare(
            "SELECT COALESCE(identity_id, 'unknown'), SUM(cost_usd)
             FROM cost_records
             WHERE timestamp >= ?1 AND timestamp <= ?2
             GROUP BY COALESCE(identity_id, 'unknown')
             ORDER BY SUM(cost_usd) DESC",
        )?;
        let rows = stmt
            .query_map(params![from.to_rfc3339(), to.to_rfc3339()], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, f64>(1)?))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Daily token-history bucket for one identity.
    pub fn cost_token_history_by_identity_day(
        &self,
        identity_id: &str,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<Vec<TokenHistoryPoint>> {
        token_history_filtered(
            self,
            "identity_id = ?1 AND timestamp >= ?2 AND timestamp <= ?3",
            params![identity_id, from.to_rfc3339(), to.to_rfc3339()],
        )
    }

    /// Daily token-history bucket for one agent.
    pub fn cost_token_history_by_agent_day(
        &self,
        agent_id: &str,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<Vec<TokenHistoryPoint>> {
        token_history_filtered(
            self,
            "agent_id = ?1 AND timestamp >= ?2 AND timestamp <= ?3",
            params![agent_id, from.to_rfc3339(), to.to_rfc3339()],
        )
    }

    /// Identity-scoped variant of [`Self::get_agent_usage_summary`].
    pub fn get_identity_usage_summary(
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
        let conn = self
            .conn
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {}", e))?;
        let sql = format!(
            "SELECT {group_col} as group_key,
                    COUNT(*),
                    SUM(input_tokens),
                    SUM(output_tokens),
                    SUM(cost_usd),
                    AVG(rounds),
                    AVG(duration_ms),
                    AVG(CASE WHEN success THEN 1.0 ELSE 0.0 END),
                    SUM(cache_creation_input_tokens),
                    SUM(cache_read_input_tokens)
             FROM task_telemetry
             WHERE identity_id = ?1 AND completed_at >= ?2 AND completed_at <= ?3
             GROUP BY {group_col}
             ORDER BY SUM(cost_usd) DESC"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt
            .query_map(
                params![identity_id, from.to_rfc3339(), to.to_rfc3339()],
                row_to_agent_usage_summary,
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Identity-scoped variant of [`Self::compare_models_for_agent`].
    pub fn compare_models_for_identity(
        &self,
        identity_id: &str,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<Vec<ModelComparison>> {
        self.compare_models_filtered("identity_id", identity_id, from, to)
    }

    /// Identity-scoped variant of [`Self::get_tool_cost_breakdown`].
    pub fn tool_breakdown_for_identity(
        &self,
        identity_id: &str,
        from: DateTime<Utc>,
        limit: usize,
    ) -> Result<Vec<ToolCostEntry>> {
        self.tool_breakdown_filtered("identity_id", identity_id, from, limit)
    }

    fn compare_models_filtered(
        &self,
        filter_col: &str,
        filter_val: &str,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<Vec<ModelComparison>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {}", e))?;
        let sql = format!(
            "{MODEL_COMPARISON_PROJECTION}
             WHERE {filter_col} = ?1 AND completed_at >= ?2 AND completed_at <= ?3
             GROUP BY model
             ORDER BY SUM(cost_usd) DESC"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt
            .query_map(
                params![filter_val, from.to_rfc3339(), to.to_rfc3339()],
                row_to_model_comparison,
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    fn tool_breakdown_filtered(
        &self,
        filter_col: &str,
        filter_val: &str,
        from: DateTime<Utc>,
        limit: usize,
    ) -> Result<Vec<ToolCostEntry>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {}", e))?;
        let sql = format!(
            "{TOOL_BREAKDOWN_PROJECTION}
             WHERE t.{filter_col} = ?1 AND t.completed_at >= ?2
             GROUP BY tt.tool_name
             ORDER BY invocations DESC
             LIMIT ?3"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt
            .query_map(
                params![filter_val, from.to_rfc3339(), limit as i64],
                row_to_tool_cost_entry,
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Identities that have triggered this agent in the window. NULL
    /// identity (system-initiated tasks) becomes `"unknown"`.
    pub fn agent_identity_breakdown(
        &self,
        agent_id: &str,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<Vec<(String, f64)>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {}", e))?;
        let mut stmt = conn.prepare(
            "SELECT COALESCE(identity_id, 'unknown'), SUM(cost_usd)
             FROM task_telemetry
             WHERE agent_id = ?1 AND completed_at >= ?2 AND completed_at <= ?3
             GROUP BY COALESCE(identity_id, 'unknown')
             ORDER BY SUM(cost_usd) DESC",
        )?;
        let rows = stmt
            .query_map(
                params![agent_id, from.to_rfc3339(), to.to_rfc3339()],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, f64>(1)?)),
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Cost grouped by `task_type`. NULL collapses to `"unknown"`.
    pub fn cost_by_task_type(
        &self,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<Vec<(String, f64)>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {}", e))?;
        let mut stmt = conn.prepare(
            "SELECT COALESCE(task_type, 'unknown'), SUM(cost_usd)
             FROM cost_records
             WHERE timestamp >= ?1 AND timestamp <= ?2
             GROUP BY COALESCE(task_type, 'unknown')
             ORDER BY SUM(cost_usd) DESC",
        )?;
        let rows = stmt
            .query_map(params![from.to_rfc3339(), to.to_rfc3339()], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, f64>(1)?))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Cursor-paginated task history. See [`TaskHistoryFilter`].
    pub fn task_history_paginated(&self, filter: &TaskHistoryFilter) -> Result<TaskHistoryPage> {
        let limit = crate::pagination::clamp_limit(Some(filter.limit));
        let conn = self
            .conn
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {}", e))?;

        let mut clauses: Vec<String> = Vec::new();
        let mut bind: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        if let Some(ref id) = filter.identity_id {
            clauses.push(format!("identity_id = ?{}", bind.len() + 1));
            bind.push(Box::new(id.clone()));
        }
        if let Some(ref id) = filter.agent_id {
            clauses.push(format!("agent_id = ?{}", bind.len() + 1));
            bind.push(Box::new(id.clone()));
        }
        if let Some(ref tt) = filter.task_type {
            clauses.push(format!("task_type = ?{}", bind.len() + 1));
            bind.push(Box::new(tt.clone()));
        }
        if let Some(ref m) = filter.model {
            clauses.push(format!("model = ?{}", bind.len() + 1));
            bind.push(Box::new(m.clone()));
        }
        if let Some(since) = filter.since {
            clauses.push(format!("completed_at >= ?{}", bind.len() + 1));
            bind.push(Box::new(since.to_rfc3339()));
        }
        if let Some(until) = filter.until {
            clauses.push(format!("completed_at <= ?{}", bind.len() + 1));
            bind.push(Box::new(until.to_rfc3339()));
        }
        if let Some((cursor_ts, cursor_id)) = filter.cursor {
            // Stable cursor on the (completed_at, id) pair.
            clauses.push(format!(
                "(completed_at < ?{} OR (completed_at = ?{} AND id < ?{}))",
                bind.len() + 1,
                bind.len() + 2,
                bind.len() + 3,
            ));
            bind.push(Box::new(cursor_ts.to_rfc3339()));
            bind.push(Box::new(cursor_ts.to_rfc3339()));
            bind.push(Box::new(cursor_id));
        }
        let where_sql = if clauses.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", clauses.join(" AND "))
        };

        // +1 so we can detect whether another page exists.
        let fetch = limit + 1;
        let sql = format!(
            "SELECT {TASK_HISTORY_COLUMNS}
             FROM task_telemetry
             {where_sql}
             ORDER BY completed_at DESC, id DESC
             LIMIT ?{}",
            bind.len() + 1
        );
        bind.push(Box::new(fetch as i64));

        let mut stmt = conn.prepare(&sql)?;
        let params: Vec<&dyn rusqlite::ToSql> = bind
            .iter()
            .map(|b| b.as_ref() as &dyn rusqlite::ToSql)
            .collect();
        let mut entries: Vec<TaskHistoryEntry> = stmt
            .query_map(rusqlite::params_from_iter(params), row_to_task_history)?
            .collect::<std::result::Result<Vec<_>, _>>()?;

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
    }

    /// End-to-end trace for one task.
    pub fn get_task_trace(&self, task_id: &str) -> Result<Option<TaskTrace>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {}", e))?;
        let task_row: Option<(i64, TaskTelemetryRecord)> = conn
            .query_row(
                "SELECT id, agent_id, model, provider, input_tokens, output_tokens,
                        cache_creation_input_tokens, cache_read_input_tokens, cost_usd,
                        rounds, duration_ms, tool_invocation_count, success, error_type,
                        retry_count, task_type, task_id, started_at, completed_at,
                        forecast_cost_usd, forecast_rounds, forecast_confidence,
                        team_id, session_id, model_switches, ticket_id, identity_id,
                        description
                 FROM task_telemetry WHERE task_id = ?1
                 ORDER BY id DESC LIMIT 1",
                params![task_id],
                |r| Ok((r.get::<_, i64>(0)?, row_to_task_telemetry_record(r)?)),
            )
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                e => Err(eyre::eyre!("trace lookup failed: {e}")),
            })?;

        let Some((task_pk, task)) = task_row else {
            return Ok(None);
        };

        let mut rounds_stmt = conn.prepare(
            "SELECT round_number, input_tokens, output_tokens, latency_ms,
                    cache_creation_tokens, cache_read_tokens,
                    prompt_size_bytes, response_size_bytes, tool_count, stop_reason
             FROM round_telemetry WHERE task_telemetry_id = ?1 ORDER BY round_number ASC",
        )?;
        let rounds: Vec<RoundTelemetryRecord> = rounds_stmt
            .query_map(params![task_pk], |r| {
                Ok(RoundTelemetryRecord {
                    round_number: r.get::<_, i64>(0)? as u32,
                    input_tokens: r.get::<_, i64>(1)? as u64,
                    output_tokens: r.get::<_, i64>(2)? as u64,
                    latency_ms: r.get::<_, i64>(3)? as u64,
                    cache_creation_tokens: r.get::<_, i64>(4)? as u64,
                    cache_read_tokens: r.get::<_, i64>(5)? as u64,
                    prompt_size_bytes: r.get::<_, i64>(6)? as u64,
                    response_size_bytes: r.get::<_, i64>(7)? as u64,
                    tool_count: r.get::<_, i64>(8)? as u32,
                    stop_reason: r.get(9)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        let mut tools_stmt = conn.prepare(
            "SELECT round_number, tool_name, success, duration_ms,
                    policy_decision, input_size_bytes, output_size_bytes, denial_reason
             FROM tool_telemetry WHERE task_telemetry_id = ?1
             ORDER BY round_number ASC, id ASC",
        )?;
        let tools: Vec<ToolTelemetryRecord> = tools_stmt
            .query_map(params![task_pk], |r| {
                Ok(ToolTelemetryRecord {
                    round_number: r.get::<_, i64>(0)? as u32,
                    tool_name: r.get(1)?,
                    success: r.get::<_, i64>(2)? != 0,
                    duration_ms: r.get::<_, i64>(3)? as u64,
                    policy_decision: r.get(4)?,
                    input_size_bytes: r.get::<_, i64>(5)? as u64,
                    output_size_bytes: r.get::<_, i64>(6)? as u64,
                    denial_reason: r.get(7)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        let mut cost_stmt = conn.prepare(
            "SELECT timestamp, agent_id, model, input_tokens, output_tokens,
                    cache_creation_input_tokens, cache_read_input_tokens, cost_usd,
                    team_id, task_id, task_type, ticket_id, identity_id,
                    origin, billing, payer, credential_tier
             FROM cost_records WHERE task_id = ?1 ORDER BY timestamp ASC",
        )?;
        let cost_records: Vec<CostRecordView> = cost_stmt
            .query_map(params![task_id], |r| {
                Ok(CostRecordView {
                    timestamp: r.get(0)?,
                    agent_id: r.get(1)?,
                    model: r.get(2)?,
                    input_tokens: r.get::<_, i64>(3)? as u64,
                    output_tokens: r.get::<_, i64>(4)? as u64,
                    cache_creation_input_tokens: r.get::<_, i64>(5)? as u64,
                    cache_read_input_tokens: r.get::<_, i64>(6)? as u64,
                    cost_usd: r.get(7)?,
                    team_id: r.get(8)?,
                    task_id: r.get(9)?,
                    task_type: r.get(10)?,
                    ticket_id: r.get(11)?,
                    identity_id: r.get(12)?,
                    origin: r.get(13)?,
                    billing: Billing::from_column(&r.get::<_, String>(14)?),
                    payer: r.get(15)?,
                    credential_tier: tier_column(r.get(16)?),
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        Ok(Some(TaskTrace {
            task,
            rounds,
            tools,
            cost_records,
        }))
    }

    // ─── Telemetry ───

    /// Record full task telemetry with per-round and per-tool details in a single transaction.
    pub fn record_task_telemetry(
        &self,
        task: &TaskTelemetryRecord,
        rounds: &[RoundTelemetryRecord],
        tools: &[ToolTelemetryRecord],
    ) -> Result<i64> {
        let mut conn = self
            .conn
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {}", e))?;
        let tx = conn.transaction()?;

        tx.execute(
            "INSERT INTO task_telemetry
             (agent_id, model, provider, input_tokens, output_tokens,
              cache_creation_input_tokens, cache_read_input_tokens,
              cost_usd, rounds, duration_ms, tool_invocation_count,
              success, error_type, retry_count, task_type, task_id,
              started_at, completed_at, forecast_cost_usd, forecast_rounds, forecast_confidence,
              team_id, session_id, model_switches, ticket_id, identity_id, description)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26, ?27)",
            params![
                task.agent_id,
                task.model,
                task.provider,
                task.input_tokens as i64,
                task.output_tokens as i64,
                task.cache_creation_input_tokens as i64,
                task.cache_read_input_tokens as i64,
                task.cost_usd,
                task.rounds as i64,
                task.duration_ms as i64,
                task.tool_invocation_count as i64,
                task.success as i64,
                task.error_type,
                task.retry_count as i64,
                task.task_type,
                task.task_id,
                task.started_at,
                task.completed_at,
                task.forecast_cost_usd,
                task.forecast_rounds.map(|r| r as i64),
                task.forecast_confidence,
                task.team_id,
                task.session_id,
                task.model_switches,
                task.ticket_id,
                task.identity_id,
                task.description,
            ],
        )?;
        let telemetry_id = tx.last_insert_rowid();

        for round in rounds {
            tx.execute(
                "INSERT INTO round_telemetry
                 (task_telemetry_id, round_number, input_tokens, output_tokens,
                  latency_ms, cache_creation_tokens, cache_read_tokens,
                  prompt_size_bytes, response_size_bytes, tool_count, stop_reason)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                params![
                    telemetry_id,
                    round.round_number as i64,
                    round.input_tokens as i64,
                    round.output_tokens as i64,
                    round.latency_ms as i64,
                    round.cache_creation_tokens as i64,
                    round.cache_read_tokens as i64,
                    round.prompt_size_bytes as i64,
                    round.response_size_bytes as i64,
                    round.tool_count as i64,
                    round.stop_reason,
                ],
            )?;
        }

        for tool in tools {
            tx.execute(
                "INSERT INTO tool_telemetry
                 (task_telemetry_id, round_number, tool_name, success,
                  duration_ms, policy_decision, input_size_bytes, output_size_bytes,
                  denial_reason)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    telemetry_id,
                    tool.round_number as i64,
                    tool.tool_name,
                    tool.success as i64,
                    tool.duration_ms as i64,
                    tool.policy_decision,
                    tool.input_size_bytes as i64,
                    tool.output_size_bytes as i64,
                    tool.denial_reason,
                ],
            )?;
        }

        tx.commit()?;
        Ok(telemetry_id)
    }

    /// Paginated cost-attribution ledger over `cost_records` — every billable
    /// LLM call (agent runs, vision, transcription, workflow steps).
    ///
    /// Filters: time range (required), the scope's `agent_id` and `user_id`
    /// (matches `identity_id`), and a billing. Sorted by timestamp DESC. The
    /// returned `distinct_users` and `distinct_agents` lists cover the whole
    /// filtered range (modulo `user_id` and billing, which are intentionally
    /// not self-narrowed) so UI dropdowns stay complete across pagination.
    pub fn cost_detail_records(
        &self,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
        filter: CostDetailFilter<'_>,
        limit: u32,
        offset: u32,
    ) -> Result<CostDetailPage> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {}", e))?;

        let mut page_filter = cost_scope_filter(from, to, filter.scope);
        if let Some(billing) = filter.billing {
            page_filter.add("billing = ?", billing.as_str());
        }

        let count_sql = format!(
            "SELECT COUNT(*) FROM cost_records WHERE {}",
            page_filter.where_clause()
        );
        let total: u64 = conn
            .query_row(
                &count_sql,
                rusqlite::params_from_iter(page_filter.params()),
                |row| row.get::<_, i64>(0),
            )
            .map(|n| n as u64)
            .unwrap_or(0);

        let data_sql = format!(
            "SELECT id, timestamp, agent_id, model, input_tokens, output_tokens,
                    cache_creation_input_tokens, cache_read_input_tokens, cost_usd,
                    team_id, task_id, task_type, ticket_id, identity_id, description,
                    origin, billing, payer, credential_tier
             FROM cost_records
             WHERE {}
             ORDER BY timestamp DESC
             LIMIT {} OFFSET {}",
            page_filter.where_clause(),
            limit,
            offset,
        );
        let mut stmt = conn.prepare(&data_sql)?;
        let records: Vec<CostDetailRow> = stmt
            .query_map(rusqlite::params_from_iter(page_filter.params()), |row| {
                Ok(CostDetailRow {
                    id: row.get::<_, i64>(0)?,
                    timestamp: row.get(1)?,
                    agent_id: row.get(2)?,
                    model: row.get(3)?,
                    input_tokens: row.get::<_, i64>(4)? as u64,
                    output_tokens: row.get::<_, i64>(5)? as u64,
                    cache_creation_input_tokens: row.get::<_, i64>(6)? as u64,
                    cache_read_input_tokens: row.get::<_, i64>(7)? as u64,
                    cost_usd: row.get(8)?,
                    team_id: row.get(9)?,
                    task_id: row.get(10)?,
                    task_type: row.get(11)?,
                    ticket_id: row.get(12)?,
                    user_id: row.get(13)?,
                    description: row.get(14)?,
                    origin: row.get(15)?,
                    billing: Billing::from_column(&row.get::<_, String>(16)?),
                    payer: row.get(17)?,
                    credential_tier: tier_column(row.get(18)?),
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        // Distinct lists query (time + agent — user dropdown shouldn't
        // narrow itself when a user filter is active).
        let dist_filter = cost_scope_filter(
            from,
            to,
            CostScope {
                user_id: None,
                ..filter.scope
            },
        );
        let distinct_users: Vec<String> = conn
            .prepare(&format!(
                "SELECT DISTINCT identity_id FROM cost_records
                 WHERE {} AND identity_id IS NOT NULL
                 ORDER BY identity_id",
                dist_filter.where_clause()
            ))?
            .query_map(rusqlite::params_from_iter(dist_filter.params()), |r| {
                r.get::<_, String>(0)
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        // Agent dropdown should reflect the whole time range, ignoring
        // both agent and user filters.
        let agents_filter = cost_scope_filter(from, to, CostScope::default());
        let distinct_agents: Vec<String> = conn
            .prepare(&format!(
                "SELECT DISTINCT agent_id FROM cost_records
                 WHERE {}
                 ORDER BY agent_id",
                agents_filter.where_clause()
            ))?
            .query_map(rusqlite::params_from_iter(agents_filter.params()), |r| {
                r.get::<_, String>(0)
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        Ok(CostDetailPage {
            records,
            total,
            distinct_users,
            distinct_agents,
        })
    }

    /// Calls and cost in `scope` between `from` and `to`, per agent, billing,
    /// origin, payer and credential tier.
    pub fn cost_split(
        &self,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
        scope: CostScope<'_>,
    ) -> Result<Vec<CostSplitRow>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {}", e))?;
        let filter = cost_scope_filter(from, to, scope);
        let sql = format!(
            "SELECT agent_id, billing, origin, payer, credential_tier, COUNT(*), SUM(cost_usd)
             FROM cost_records
             WHERE {}
             GROUP BY agent_id, billing, origin, payer, credential_tier
             ORDER BY agent_id, billing, origin, payer, credential_tier",
            filter.where_clause()
        );
        let rows = conn
            .prepare(&sql)?
            .query_map(rusqlite::params_from_iter(filter.params()), |row| {
                Ok(CostSplitRow {
                    agent_id: row.get(0)?,
                    billing: Billing::from_column(&row.get::<_, String>(1)?),
                    origin: row.get(2)?,
                    payer: row.get(3)?,
                    credential_tier: tier_column(row.get(4)?),
                    calls: row.get::<_, i64>(5)? as u64,
                    cost_usd: row.get(6)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Paginated execution log for scheduled jobs (`provider IN
    /// ('job_runner', 'scheduler')` rows in `task_telemetry`). Backs the
    /// scheduler UI's history tab.
    pub fn scheduled_job_history(
        &self,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
        scope: CostScope<'_>,
        limit: u32,
        offset: u32,
    ) -> Result<ScheduledJobHistoryPage> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {}", e))?;

        let from_str = from.to_rfc3339();
        let to_str = to.to_rfc3339();

        let mut filter = WhereBuilder::new();
        filter.add_literal("t.provider IN ('job_runner', 'scheduler')");
        filter.add("t.completed_at >= ?", from_str);
        filter.add("t.completed_at <= ?", to_str);
        if let Some(aid) = scope.agent_id {
            filter.add("t.agent_id = ?", aid.to_string());
        }
        if let Some(user_id) = scope.user_id {
            filter.add("t.identity_id = ?", user_id.to_string());
        }

        let total: u64 = conn
            .query_row(
                &format!(
                    "SELECT COUNT(*) FROM task_telemetry t WHERE {}",
                    filter.where_clause()
                ),
                rusqlite::params_from_iter(filter.params()),
                |row| row.get::<_, i64>(0),
            )
            .map(|n| n as u64)
            .unwrap_or(0);

        // `cost_usd` is sourced from cost_records via task_id rather than
        // `task_telemetry.cost_usd`. The per-provider task_telemetry row
        // that `process_message` also writes already captures that figure;
        // adding it again here would double-count once the inner row and
        // the scheduler-marker row are both queried.
        let data_sql = format!(
            "SELECT t.id, t.agent_id, t.task_type, t.task_id, t.description, t.success,
                    t.error_type, t.duration_ms, t.started_at, t.completed_at, t.identity_id,
                    COALESCE(c.total_cost, 0.0) AS cost_usd
             FROM task_telemetry t
             LEFT JOIN (
                 SELECT task_id, SUM(cost_usd) AS total_cost
                 FROM cost_records
                 WHERE task_id IS NOT NULL
                 GROUP BY task_id
             ) c ON c.task_id = t.task_id
             WHERE {}
             ORDER BY t.completed_at DESC
             LIMIT {} OFFSET {}",
            filter.where_clause(),
            limit,
            offset,
        );
        let mut stmt = conn.prepare(&data_sql)?;
        let records: Vec<ScheduledJobRow> = stmt
            .query_map(rusqlite::params_from_iter(filter.params()), |row| {
                Ok(ScheduledJobRow {
                    id: row.get::<_, i64>(0)?,
                    agent_id: row.get(1)?,
                    task_type: row.get(2)?,
                    task_id: row.get(3)?,
                    description: row.get(4)?,
                    success: row.get::<_, i64>(5)? != 0,
                    error_type: row.get(6)?,
                    duration_ms: row.get::<_, i64>(7)? as u64,
                    started_at: row.get(8)?,
                    completed_at: row.get(9)?,
                    user_id: row.get(10)?,
                    cost_usd: row.get(11)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        Ok(ScheduledJobHistoryPage { records, total })
    }

    /// Get aggregated usage summary for an agent, grouped by model or task_type.
    pub fn get_agent_usage_summary(
        &self,
        agent_id: &str,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
        group_by: &str,
    ) -> Result<Vec<AgentUsageSummary>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {}", e))?;

        let group_col = match group_by {
            "task_type" => "COALESCE(task_type, 'unknown')",
            "provider" => "COALESCE(provider, 'unknown')",
            _ => "model",
        };

        let sql = format!(
            "SELECT {group_col} as group_key,
                    COUNT(*) as total_tasks,
                    SUM(input_tokens) as total_input,
                    SUM(output_tokens) as total_output,
                    SUM(cost_usd) as total_cost,
                    AVG(rounds) as avg_rounds,
                    AVG(duration_ms) as avg_duration,
                    AVG(CASE WHEN success THEN 1.0 ELSE 0.0 END) as success_rate,
                    SUM(cache_creation_input_tokens) as total_cache_created,
                    SUM(cache_read_input_tokens) as total_cache_read
             FROM task_telemetry
             WHERE agent_id = ?1 AND completed_at >= ?2 AND completed_at <= ?3
             GROUP BY {group_col}
             ORDER BY total_cost DESC"
        );

        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt
            .query_map(
                params![agent_id, from.to_rfc3339(), to.to_rfc3339()],
                |row| {
                    Ok(AgentUsageSummary {
                        group_key: row.get(0)?,
                        total_tasks: row.get::<_, i64>(1)? as u64,
                        total_input_tokens: row.get::<_, i64>(2)? as u64,
                        total_output_tokens: row.get::<_, i64>(3)? as u64,
                        total_cost: row.get(4)?,
                        avg_rounds: row.get(5)?,
                        avg_duration_ms: row.get(6)?,
                        success_rate: row.get(7)?,
                        total_cache_created_tokens: row.get::<_, i64>(8)? as u64,
                        total_cache_read_tokens: row.get::<_, i64>(9)? as u64,
                    })
                },
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Compare model performance for an agent over a time range.
    pub fn compare_models_for_agent(
        &self,
        agent_id: &str,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<Vec<ModelComparison>> {
        self.compare_models_filtered("agent_id", agent_id, from, to)
    }

    /// Get recent task history for an agent with optional filters.
    pub fn get_task_history(
        &self,
        agent_id: &str,
        limit: usize,
        task_type: Option<&str>,
        model: Option<&str>,
    ) -> Result<Vec<TaskHistoryEntry>> {
        self.get_task_history_filtered(agent_id, limit, task_type, model, None, None)
    }

    /// Get task history with optional time range and sort order.
    pub fn get_task_history_filtered(
        &self,
        agent_id: &str,
        limit: usize,
        task_type: Option<&str>,
        model: Option<&str>,
        since: Option<&str>,
        sort_by: Option<&str>,
    ) -> Result<Vec<TaskHistoryEntry>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {}", e))?;

        let order_clause = match sort_by {
            Some("cost") => "cost_usd DESC",
            Some("duration") => "duration_ms DESC",
            Some("tokens") => "(input_tokens + output_tokens) DESC",
            _ => "completed_at DESC",
        };

        let sql = format!(
            "SELECT {TASK_HISTORY_COLUMNS}
             FROM task_telemetry
             WHERE agent_id = ?1
               AND (?2 IS NULL OR task_type = ?2)
               AND (?3 IS NULL OR model = ?3)
               AND (?4 IS NULL OR completed_at >= ?4)
             ORDER BY {order_clause}
             LIMIT ?5"
        );

        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt
            .query_map(
                params![agent_id, task_type, model, since, limit as i64],
                row_to_task_history,
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Upsert an agent's preferred model for a task type.
    pub fn upsert_agent_model_preference(&self, pref: &AgentModelPreference) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {}", e))?;
        conn.execute(
            "INSERT INTO agent_model_preferences (agent_id, task_type, preferred_model, reason, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(agent_id, task_type) DO UPDATE SET
                 preferred_model = excluded.preferred_model,
                 reason = excluded.reason,
                 updated_at = excluded.updated_at",
            params![
                pref.agent_id,
                pref.task_type,
                pref.preferred_model,
                pref.reason,
                pref.updated_at,
            ],
        )?;
        Ok(())
    }

    /// Get an agent's preferred model for a task type.
    pub fn get_agent_model_preference(
        &self,
        agent_id: &str,
        task_type: &str,
    ) -> Result<Option<AgentModelPreference>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {}", e))?;
        let mut stmt = conn.prepare(
            "SELECT agent_id, task_type, preferred_model, reason, updated_at
             FROM agent_model_preferences
             WHERE agent_id = ?1 AND task_type = ?2",
        )?;
        let row = stmt.query_row(params![agent_id, task_type], |row| {
            Ok(AgentModelPreference {
                agent_id: row.get(0)?,
                task_type: row.get(1)?,
                preferred_model: row.get(2)?,
                reason: row.get(3)?,
                updated_at: row.get(4)?,
            })
        });
        match row {
            Ok(pref) => Ok(Some(pref)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Record a model health event (error, rate-limit, failover).
    pub fn record_model_health_event(
        &self,
        model: &str,
        event_type: &str,
        detail: Option<&str>,
        agent_id: Option<&str>,
    ) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {}", e))?;
        let now = Utc::now().to_rfc3339();
        conn.execute(
            "INSERT INTO model_health_events (timestamp, model, event_type, detail, agent_id)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![now, model, event_type, detail, agent_id],
        )?;
        Ok(())
    }

    /// Append one lesson-pipeline event (see [`crate::telemetry::LessonEvent`]).
    pub fn record_lesson_event(&self, event: &crate::telemetry::LessonEvent) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {}", e))?;
        let tags = serde_json::to_string(&event.trigger_tags).unwrap_or_else(|_| "[]".to_string());
        conn.execute(
            "INSERT INTO lesson_events
                (timestamp, agent_id, identity_id, lesson_id, event_type, learning_kind,
                 recall_score, importance_delta, confidence_after, applied_after, session_id, trigger_tags)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                event.timestamp.to_rfc3339(),
                event.agent_id,
                event.identity_id,
                event.lesson_id,
                event.event_type,
                event.learning_kind,
                event.recall_score,
                event.importance_delta,
                event.confidence_after,
                event.applied_after,
                event.session_id,
                tags,
            ],
        )?;
        Ok(())
    }

    /// Fetch lesson events in `[from, to]`, optionally scoped to one agent,
    /// ordered ascending by time.
    pub fn lesson_events_in_range(
        &self,
        agent_id: Option<&str>,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<Vec<crate::telemetry::LessonEvent>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {}", e))?;
        const COLS: &str = "timestamp, agent_id, identity_id, lesson_id, event_type, learning_kind, \
             recall_score, importance_delta, confidence_after, applied_after, session_id, trigger_tags";
        let map_row = |row: &rusqlite::Row| -> rusqlite::Result<crate::telemetry::LessonEvent> {
            let ts: String = row.get(0)?;
            let timestamp = DateTime::parse_from_rfc3339(&ts)
                .map(|d| d.with_timezone(&Utc))
                .map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        0,
                        rusqlite::types::Type::Text,
                        Box::new(e),
                    )
                })?;
            let tags_raw: Option<String> = row.get(11)?;
            let trigger_tags = tags_raw
                .and_then(|s| serde_json::from_str::<Vec<String>>(&s).ok())
                .unwrap_or_default();
            Ok(crate::telemetry::LessonEvent {
                timestamp,
                agent_id: row.get(1)?,
                identity_id: row.get(2)?,
                lesson_id: row.get(3)?,
                event_type: row.get(4)?,
                learning_kind: row.get(5)?,
                recall_score: row.get(6)?,
                importance_delta: row.get(7)?,
                confidence_after: row.get(8)?,
                applied_after: row.get(9)?,
                session_id: row.get(10)?,
                trigger_tags,
            })
        };
        if let Some(agent) = agent_id {
            let sql = format!(
                "SELECT {COLS} FROM lesson_events
                 WHERE timestamp >= ?1 AND timestamp <= ?2 AND agent_id = ?3
                 ORDER BY timestamp ASC"
            );
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt
                .query_map(params![from.to_rfc3339(), to.to_rfc3339(), agent], map_row)?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            Ok(rows)
        } else {
            let sql = format!(
                "SELECT {COLS} FROM lesson_events
                 WHERE timestamp >= ?1 AND timestamp <= ?2
                 ORDER BY timestamp ASC"
            );
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt
                .query_map(params![from.to_rfc3339(), to.to_rfc3339()], map_row)?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            Ok(rows)
        }
    }

    /// Append one relationship event, ignoring a re-attempt of the same
    /// `(session_id, message_id)`. Returns `true` when a new row was inserted.
    pub fn record_relationship_event(
        &self,
        event: &crate::telemetry::RelationshipEvent,
    ) -> Result<bool> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {}", e))?;
        let affected = conn.execute(
            "INSERT OR IGNORE INTO relationship_events
                (timestamp, agent_id, identity_id, session_id, message_id, valence, intensity,
                 emotion_label, reaction, is_key_moment, confidence, source, signals)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            params![
                event.timestamp.to_rfc3339(),
                event.agent_id,
                event.identity_id,
                event.session_id,
                event.message_id,
                event.valence,
                event.intensity,
                event.emotion_label,
                event.reaction,
                event.is_key_moment as i64,
                event.confidence,
                event.source,
                event.signals,
            ],
        )?;
        Ok(affected > 0)
    }

    /// Fetch relationship events in `[from, to]`, optionally scoped to one agent
    /// and/or one identity, ordered ascending by time.
    pub fn relationship_events_in_range(
        &self,
        agent_id: Option<&str>,
        identity_id: Option<&str>,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<Vec<crate::telemetry::RelationshipEvent>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {}", e))?;
        const COLS: &str = "timestamp, agent_id, identity_id, session_id, message_id, valence, \
             intensity, emotion_label, reaction, is_key_moment, confidence, source, signals";
        let sql = format!(
            "SELECT {COLS} FROM relationship_events
             WHERE timestamp >= ?1 AND timestamp <= ?2
               AND (?3 IS NULL OR agent_id = ?3)
               AND (?4 IS NULL OR identity_id = ?4)
             ORDER BY timestamp ASC"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt
            .query_map(
                params![from.to_rfc3339(), to.to_rfc3339(), agent_id, identity_id],
                row_to_relationship_event,
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// The per-session high-water mark for the periodic relationship scan.
    pub fn get_relationship_cursor(
        &self,
        session_id: &str,
    ) -> Result<Option<crate::telemetry::RelationshipCursor>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {}", e))?;
        let mut stmt = conn.prepare(
            "SELECT session_id, agent_id, last_message_ts, last_message_id, updated_at
             FROM relationship_cursor WHERE session_id = ?1",
        )?;
        let mut rows = stmt.query_map(params![session_id], |row| {
            Ok(crate::telemetry::RelationshipCursor {
                session_id: row.get(0)?,
                agent_id: row.get(1)?,
                last_message_ts: parse_ts_col(2, row.get(2)?)?,
                last_message_id: row.get(3)?,
                updated_at: parse_ts_col(4, row.get(4)?)?,
            })
        })?;
        match rows.next() {
            Some(row) => Ok(Some(row?)),
            None => Ok(None),
        }
    }

    /// Advance (or create) the per-session relationship scan cursor.
    pub fn upsert_relationship_cursor(
        &self,
        cursor: &crate::telemetry::RelationshipCursor,
    ) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {}", e))?;
        conn.execute(
            "INSERT INTO relationship_cursor
                (session_id, agent_id, last_message_ts, last_message_id, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(session_id) DO UPDATE SET
                agent_id = excluded.agent_id,
                last_message_ts = excluded.last_message_ts,
                last_message_id = excluded.last_message_id,
                updated_at = excluded.updated_at",
            params![
                cursor.session_id,
                cursor.agent_id,
                cursor.last_message_ts.to_rfc3339(),
                cursor.last_message_id,
                cursor.updated_at.to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    /// Get cache status metrics for an agent, grouped by model.
    pub fn get_cache_status(
        &self,
        agent_id: &str,
        from: DateTime<Utc>,
    ) -> Result<Vec<CacheStatusEntry>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {}", e))?;

        let mut stmt = conn.prepare(
            "SELECT model,
                    SUM(cache_creation_input_tokens) as total_created,
                    SUM(cache_read_input_tokens) as total_read,
                    SUM(input_tokens) as total_input,
                    COUNT(*) as task_count
             FROM task_telemetry
             WHERE agent_id = ?1 AND completed_at >= ?2
             GROUP BY model",
        )?;
        let rows = stmt
            .query_map(params![agent_id, from.to_rfc3339()], |row| {
                Ok(CacheStatusEntry {
                    model: row.get(0)?,
                    total_cache_creation_tokens: row.get::<_, i64>(1)? as u64,
                    total_cache_read_tokens: row.get::<_, i64>(2)? as u64,
                    total_input_tokens: row.get::<_, i64>(3)? as u64,
                    task_count: row.get::<_, i64>(4)? as u64,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Get per-round cache hit/miss counts for an agent.
    pub fn get_round_cache_stats(&self, agent_id: &str, from: DateTime<Utc>) -> Result<(u64, u64)> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {}", e))?;

        let mut stmt = conn.prepare(
            "SELECT COUNT(*) as total_rounds,
                    SUM(CASE WHEN r.cache_read_tokens > 0 THEN 1 ELSE 0 END) as cache_hit_rounds
             FROM round_telemetry r
             JOIN task_telemetry t ON r.task_telemetry_id = t.id
             WHERE t.agent_id = ?1 AND t.completed_at >= ?2",
        )?;
        let (total, hits) = stmt.query_row(params![agent_id, from.to_rfc3339()], |row| {
            Ok((row.get::<_, i64>(0)? as u64, row.get::<_, i64>(1)? as u64))
        })?;
        Ok((total, hits))
    }

    /// Get tool cost breakdown for an agent.
    pub fn get_tool_cost_breakdown(
        &self,
        agent_id: &str,
        from: DateTime<Utc>,
        limit: usize,
    ) -> Result<Vec<ToolCostEntry>> {
        self.tool_breakdown_filtered("agent_id", agent_id, from, limit)
    }

    /// Tool cost breakdown across every agent in the window.
    pub fn tool_breakdown_global(
        &self,
        from: DateTime<Utc>,
        limit: usize,
    ) -> Result<Vec<ToolCostEntry>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {}", e))?;
        let sql = format!(
            "{TOOL_BREAKDOWN_PROJECTION}
             WHERE t.completed_at >= ?1
             GROUP BY tt.tool_name
             ORDER BY invocations DESC
             LIMIT ?2"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt
            .query_map(
                params![from.to_rfc3339(), limit as i64],
                row_to_tool_cost_entry,
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Daily efficiency trend across every agent in the window.
    pub fn efficiency_trends_global(
        &self,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<Vec<EfficiencyTrendPoint>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {}", e))?;
        let sql = format!(
            "{EFFICIENCY_TRENDS_PROJECTION}
             WHERE completed_at >= ?1 AND completed_at <= ?2
             GROUP BY DATE(completed_at)
             ORDER BY bucket ASC"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt
            .query_map(
                params![from.to_rfc3339(), to.to_rfc3339()],
                row_to_efficiency_trend,
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Round outliers across every agent in the window.
    pub fn round_outliers_global(
        &self,
        since: DateTime<Utc>,
        threshold_multiplier: f64,
        limit: usize,
    ) -> Result<(RoundStats, Vec<TaskHistoryEntry>)> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {}", e))?;

        let mut stmt = conn.prepare(
            "SELECT rounds FROM task_telemetry
             WHERE completed_at >= ?1
             ORDER BY rounds ASC",
        )?;
        let round_counts: Vec<u32> = stmt
            .query_map(params![since.to_rfc3339()], |row| {
                Ok(row.get::<_, i64>(0)? as u32)
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        let Some(stats) = round_stats_from(&round_counts) else {
            return Ok((RoundStats::default(), Vec::new()));
        };
        let threshold_rounds = stats.mean + threshold_multiplier * stats.stddev;

        let sql = format!(
            "SELECT {TASK_HISTORY_COLUMNS}
             FROM task_telemetry
             WHERE completed_at >= ?1 AND rounds > ?2
             ORDER BY rounds DESC
             LIMIT ?3"
        );
        let mut stmt = conn.prepare(&sql)?;
        let outliers = stmt
            .query_map(
                params![since.to_rfc3339(), threshold_rounds as i64, limit as i64],
                row_to_task_history,
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok((stats, outliers))
    }

    /// Get routing history — tasks that had model switches.
    pub fn get_routing_history(
        &self,
        agent_id: &str,
        since: DateTime<Utc>,
        limit: usize,
    ) -> Result<Vec<TaskHistoryEntry>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {}", e))?;

        let sql = format!(
            "SELECT {TASK_HISTORY_COLUMNS}
             FROM task_telemetry
             WHERE agent_id = ?1
               AND model_switches IS NOT NULL AND model_switches != '[]'
               AND completed_at >= ?2
             ORDER BY completed_at DESC
             LIMIT ?3"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt
            .query_map(
                params![agent_id, since.to_rfc3339(), limit as i64],
                row_to_task_history,
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// The agent's turns since `from`, oldest first, each with the distinct
    /// tools it called. Background-job rows record their job, not a tool, and
    /// are left out.
    pub fn agent_tool_turns(
        &self,
        agent_id: &str,
        from: DateTime<Utc>,
        limit: usize,
    ) -> Result<Vec<ToolTurn>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {}", e))?;
        let mut stmt = conn.prepare(
            "SELECT t.started_at, GROUP_CONCAT(DISTINCT tt.tool_name)
             FROM task_telemetry t
             LEFT JOIN tool_telemetry tt ON tt.task_telemetry_id = t.id
             WHERE t.agent_id = ?1 AND t.completed_at >= ?2
               AND (t.task_type IS NULL OR t.task_type NOT LIKE 'job:%')
             GROUP BY t.id
             ORDER BY t.started_at
             LIMIT ?3",
        )?;
        let rows = stmt
            .query_map(params![agent_id, from.to_rfc3339(), limit as i64], |row| {
                Ok(ToolTurn {
                    started_at: parse_ts_col(0, row.get(0)?)?,
                    tools: row
                        .get::<_, Option<String>>(1)?
                        .map(|tools| tools.split(',').map(str::to_string).collect())
                        .unwrap_or_default(),
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    // ─── Per-Ticket Cost Tracking (DW-400) ───

    /// Get aggregated cost summary for a specific ticket.
    pub fn cost_by_ticket(&self, ticket_id: &str) -> Result<TicketCostSummary> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {}", e))?;

        let mut stmt = conn.prepare(
            "SELECT COALESCE(SUM(cost_usd), 0.0),
                    COALESCE(SUM(input_tokens), 0),
                    COALESCE(SUM(output_tokens), 0),
                    COALESCE(SUM(rounds), 0),
                    COALESCE(SUM(duration_ms), 0),
                    COUNT(*),
                    GROUP_CONCAT(DISTINCT model)
             FROM task_telemetry
             WHERE ticket_id = ?1",
        )?;
        let summary = stmt.query_row(params![ticket_id], |row| {
            let models_str: Option<String> = row.get(6)?;
            let models_used = models_str
                .map(|s| s.split(',').map(String::from).collect())
                .unwrap_or_default();
            Ok(TicketCostSummary {
                ticket_id: ticket_id.to_string(),
                total_cost_usd: row.get(0)?,
                total_input_tokens: row.get::<_, i64>(1)? as u64,
                total_output_tokens: row.get::<_, i64>(2)? as u64,
                total_rounds: row.get::<_, i64>(3)? as u64,
                total_duration_ms: row.get::<_, i64>(4)? as u64,
                step_count: row.get::<_, i64>(5)? as u64,
                models_used,
            })
        })?;
        Ok(summary)
    }

    /// Get aggregated cost summaries for all tickets in a time range.
    pub fn cost_by_tickets(
        &self,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<Vec<TicketCostSummary>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {}", e))?;

        let mut stmt = conn.prepare(
            "SELECT ticket_id,
                    COALESCE(SUM(cost_usd), 0.0),
                    COALESCE(SUM(input_tokens), 0),
                    COALESCE(SUM(output_tokens), 0),
                    COALESCE(SUM(rounds), 0),
                    COALESCE(SUM(duration_ms), 0),
                    COUNT(*),
                    GROUP_CONCAT(DISTINCT model)
             FROM task_telemetry
             WHERE ticket_id IS NOT NULL
               AND completed_at >= ?1 AND completed_at <= ?2
             GROUP BY ticket_id
             ORDER BY SUM(cost_usd) DESC",
        )?;
        let rows = stmt
            .query_map(params![from.to_rfc3339(), to.to_rfc3339()], |row| {
                let models_str: Option<String> = row.get(7)?;
                let models_used = models_str
                    .map(|s| s.split(',').map(String::from).collect())
                    .unwrap_or_default();
                Ok(TicketCostSummary {
                    ticket_id: row.get(0)?,
                    total_cost_usd: row.get(1)?,
                    total_input_tokens: row.get::<_, i64>(2)? as u64,
                    total_output_tokens: row.get::<_, i64>(3)? as u64,
                    total_rounds: row.get::<_, i64>(4)? as u64,
                    total_duration_ms: row.get::<_, i64>(5)? as u64,
                    step_count: row.get::<_, i64>(6)? as u64,
                    models_used,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    // ─── Model Pricing (DW-334) ───

    /// Upsert a model pricing entry.
    pub fn upsert_model_pricing(&self, pricing: &StoredModelPricing) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {}", e))?;
        conn.execute(
            "INSERT OR REPLACE INTO model_pricing
             (model, provider, input_per_million, output_per_million, source, fetched_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                pricing.model,
                pricing.provider,
                pricing.input_per_million,
                pricing.output_per_million,
                pricing.source,
                pricing.fetched_at,
                pricing.updated_at,
            ],
        )?;
        Ok(())
    }

    /// Get pricing for a specific model.
    pub fn get_model_pricing(&self, model: &str) -> Result<Option<StoredModelPricing>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {}", e))?;
        let mut stmt = conn.prepare(
            "SELECT model, provider, input_per_million, output_per_million, source, fetched_at, updated_at
             FROM model_pricing WHERE model = ?1",
        )?;
        let row = stmt.query_row(params![model], |row| {
            Ok(StoredModelPricing {
                model: row.get(0)?,
                provider: row.get(1)?,
                input_per_million: row.get(2)?,
                output_per_million: row.get(3)?,
                source: row.get(4)?,
                fetched_at: row.get(5)?,
                updated_at: row.get(6)?,
            })
        });
        match row {
            Ok(p) => Ok(Some(p)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// List all model pricing entries.
    pub fn list_model_pricing(&self) -> Result<Vec<StoredModelPricing>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {}", e))?;
        let mut stmt = conn.prepare(
            "SELECT model, provider, input_per_million, output_per_million, source, fetched_at, updated_at
             FROM model_pricing ORDER BY model ASC",
        )?;
        let rows = stmt
            .query_map([], |row| {
                Ok(StoredModelPricing {
                    model: row.get(0)?,
                    provider: row.get(1)?,
                    input_per_million: row.get(2)?,
                    output_per_million: row.get(3)?,
                    source: row.get(4)?,
                    fetched_at: row.get(5)?,
                    updated_at: row.get(6)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Delete pricing for a specific model.
    pub fn delete_model_pricing(&self, model: &str) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {}", e))?;
        conn.execute("DELETE FROM model_pricing WHERE model = ?1", params![model])?;
        Ok(())
    }

    // ─── Policy Proposals ───

    /// Create a new policy proposal.
    pub fn create_policy_proposal(
        &self,
        agent_id: &str,
        proposed_rule: &str,
        justification: &str,
    ) -> Result<i64> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {}", e))?;
        let now = Utc::now().to_rfc3339();
        conn.execute(
            "INSERT INTO policy_proposals (agent_id, proposed_rule, justification, status, created_at)
             VALUES (?1, ?2, ?3, 'pending', ?4)",
            params![agent_id, proposed_rule, justification, now],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// List policy proposals, optionally filtered by status.
    pub fn list_policy_proposals(
        &self,
        status: Option<&str>,
        limit: usize,
    ) -> Result<Vec<PolicyProposalRow>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {}", e))?;

        let sql = "SELECT id, agent_id, proposed_rule, justification, status,
                          reviewed_by, review_note, created_at, reviewed_at
                   FROM policy_proposals
                   WHERE (?1 IS NULL OR status = ?1)
                   ORDER BY created_at DESC
                   LIMIT ?2";

        let mut stmt = conn.prepare(sql)?;
        let rows = stmt
            .query_map(params![status, limit as i64], |row| {
                Ok(PolicyProposalRow {
                    id: row.get(0)?,
                    agent_id: row.get(1)?,
                    proposed_rule: row.get(2)?,
                    justification: row.get(3)?,
                    status: row.get(4)?,
                    reviewed_by: row.get(5)?,
                    review_note: row.get(6)?,
                    created_at: row.get(7)?,
                    reviewed_at: row.get(8)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Get a single policy proposal by ID.
    pub fn get_policy_proposal(&self, id: i64) -> Result<Option<PolicyProposalRow>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {}", e))?;

        let mut stmt = conn.prepare(
            "SELECT id, agent_id, proposed_rule, justification, status,
                    reviewed_by, review_note, created_at, reviewed_at
             FROM policy_proposals WHERE id = ?1",
        )?;

        let row = stmt.query_row(params![id], |row| {
            Ok(PolicyProposalRow {
                id: row.get(0)?,
                agent_id: row.get(1)?,
                proposed_rule: row.get(2)?,
                justification: row.get(3)?,
                status: row.get(4)?,
                reviewed_by: row.get(5)?,
                review_note: row.get(6)?,
                created_at: row.get(7)?,
                reviewed_at: row.get(8)?,
            })
        });

        match row {
            Ok(proposal) => Ok(Some(proposal)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Review (approve or reject) a policy proposal.
    pub fn review_policy_proposal(
        &self,
        id: i64,
        status: &str,
        reviewed_by: &str,
        note: Option<&str>,
    ) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {}", e))?;
        let now = Utc::now().to_rfc3339();
        conn.execute(
            "UPDATE policy_proposals
             SET status = ?1, reviewed_by = ?2, review_note = ?3, reviewed_at = ?4
             WHERE id = ?5",
            params![status, reviewed_by, note, now, id],
        )?;
        Ok(())
    }

    /// Get round count statistics and outlier tasks (tasks above a threshold).
    ///
    /// Returns (stats, outlier tasks) where stats includes mean, stddev, percentiles.
    pub fn get_round_outliers(
        &self,
        agent_id: &str,
        since: DateTime<Utc>,
        threshold_multiplier: f64,
        limit: usize,
    ) -> Result<(RoundStats, Vec<TaskHistoryEntry>)> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {}", e))?;

        // First, get all round counts for stats computation
        let mut stmt = conn.prepare(
            "SELECT rounds FROM task_telemetry
             WHERE agent_id = ?1 AND completed_at >= ?2
             ORDER BY rounds ASC",
        )?;
        let round_counts: Vec<u32> = stmt
            .query_map(params![agent_id, since.to_rfc3339()], |row| {
                Ok(row.get::<_, i64>(0)? as u32)
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        let Some(stats) = round_stats_from(&round_counts) else {
            return Ok((RoundStats::default(), Vec::new()));
        };
        let threshold_rounds = stats.mean + threshold_multiplier * stats.stddev;

        let sql = format!(
            "SELECT {TASK_HISTORY_COLUMNS}
             FROM task_telemetry
             WHERE agent_id = ?1 AND completed_at >= ?2 AND rounds > ?3
             ORDER BY rounds DESC
             LIMIT ?4"
        );
        let mut stmt = conn.prepare(&sql)?;
        let outliers = stmt
            .query_map(
                params![
                    agent_id,
                    since.to_rfc3339(),
                    threshold_rounds as i64,
                    limit as i64
                ],
                row_to_task_history,
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        Ok((stats, outliers))
    }

    // ─── Workflow Instances (DW-357) ───

    /// Save a workflow instance (insert or update).
    pub fn save_workflow_instance(&self, row: &WorkflowInstanceRow) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {}", e))?;
        // Upsert (not INSERT OR REPLACE, which deletes + reinserts and would
        // wipe the owner/lease columns an execute loop holds).
        conn.execute(
            "INSERT INTO workflow_instances
             (id, workflow_id, status, current_step_id, step_history,
              variables, trigger_json, agent_id, created_at, updated_at,
              parent_instance_id, channel_id, thread_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
             ON CONFLICT(id) DO UPDATE SET
               workflow_id = excluded.workflow_id,
               status = excluded.status,
               current_step_id = excluded.current_step_id,
               step_history = excluded.step_history,
               variables = excluded.variables,
               trigger_json = excluded.trigger_json,
               agent_id = excluded.agent_id,
               created_at = excluded.created_at,
               updated_at = excluded.updated_at,
               parent_instance_id = excluded.parent_instance_id,
               channel_id = excluded.channel_id,
               thread_id = excluded.thread_id,
               version = workflow_instances.version + 1",
            params![
                row.id,
                row.workflow_id,
                row.status,
                row.current_step_id,
                row.step_history,
                row.variables,
                row.trigger_json,
                row.agent_id,
                row.created_at,
                row.updated_at,
                row.parent_instance_id,
                row.channel_id,
                row.thread_id,
            ],
        )?;
        Ok(())
    }

    /// Atomic execution claim: wins when unowned, expired, or already ours.
    pub fn try_claim_workflow_instance(
        &self,
        id: &str,
        owner: &str,
        ttl_secs: i64,
    ) -> Result<bool> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {}", e))?;
        let changed = conn.execute(
            "UPDATE workflow_instances
             SET owner_instance = ?2,
                 lease_expires_at = datetime('now', '+' || ?3 || ' seconds')
             WHERE id = ?1
               AND (owner_instance IS NULL
                    OR owner_instance = ?2
                    OR lease_expires_at IS NULL
                    OR lease_expires_at < datetime('now'))",
            params![id, owner, ttl_secs],
        )?;
        Ok(changed == 1)
    }

    pub fn renew_workflow_lease(&self, id: &str, owner: &str, ttl_secs: i64) -> Result<bool> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {}", e))?;
        let changed = conn.execute(
            "UPDATE workflow_instances
             SET lease_expires_at = datetime('now', '+' || ?3 || ' seconds')
             WHERE id = ?1 AND owner_instance = ?2",
            params![id, owner, ttl_secs],
        )?;
        Ok(changed == 1)
    }

    pub fn release_workflow_instance(&self, id: &str, owner: &str) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {}", e))?;
        conn.execute(
            "UPDATE workflow_instances
             SET owner_instance = NULL, lease_expires_at = NULL
             WHERE id = ?1 AND owner_instance = ?2",
            params![id, owner],
        )?;
        Ok(())
    }

    /// Fenced full-row save: applies only while `owner` holds the row.
    pub fn save_workflow_instance_owned(
        &self,
        row: &WorkflowInstanceRow,
        owner: &str,
    ) -> Result<bool> {
        self.update_workflow_instance_where(row, "owner_instance = ?14", &owner)
    }

    /// Full-row save that applies only while the row is still at
    /// `expected_version`, the version it was read at.
    pub fn save_workflow_instance_fenced(
        &self,
        row: &WorkflowInstanceRow,
        expected_version: i64,
    ) -> Result<bool> {
        self.update_workflow_instance_where(row, "version = ?14", &expected_version)
    }

    fn update_workflow_instance_where(
        &self,
        row: &WorkflowInstanceRow,
        condition: &str,
        guard: &dyn rusqlite::ToSql,
    ) -> Result<bool> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {}", e))?;
        let changed = conn.execute(
            &format!(
                "UPDATE workflow_instances SET
                   workflow_id = ?2, status = ?3, current_step_id = ?4,
                   step_history = ?5, variables = ?6, trigger_json = ?7,
                   agent_id = ?8, created_at = ?9, updated_at = ?10,
                   parent_instance_id = ?11, channel_id = ?12, thread_id = ?13,
                   version = version + 1
                 WHERE id = ?1 AND {condition}"
            ),
            params![
                row.id,
                row.workflow_id,
                row.status,
                row.current_step_id,
                row.step_history,
                row.variables,
                row.trigger_json,
                row.agent_id,
                row.created_at,
                row.updated_at,
                row.parent_instance_id,
                row.channel_id,
                row.thread_id,
                guard,
            ],
        )?;
        Ok(changed == 1)
    }

    /// Targeted status + reason write from any instance — never touches
    /// `step_history`, so it cannot clobber the owning loop's progress.
    pub fn cancel_workflow_instance(
        &self,
        id: &str,
        status: &str,
        reason_key: &str,
        reason: &str,
    ) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {}", e))?;
        conn.execute(
            "UPDATE workflow_instances
             SET status = ?2,
                 variables = json_set(variables, '$.' || ?3, ?4),
                 updated_at = ?5,
                 version = version + 1
             WHERE id = ?1",
            params![id, status, reason_key, reason, Utc::now().to_rfc3339()],
        )?;
        Ok(())
    }

    pub fn delete_workflow_instance(&self, id: &str) -> Result<bool> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {}", e))?;
        Ok(conn.execute("DELETE FROM workflow_instances WHERE id = ?1", params![id])? > 0)
    }

    pub fn identity_signals(&self, identity: &str) -> Result<crate::telemetry::IdentitySignals> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {}", e))?;
        crate::telemetry::SIGNAL_SOURCES.iter().try_fold(
            crate::telemetry::IdentitySignals::default(),
            |held, source| {
                let rows: i64 =
                    conn.query_row(&source.count_sql('?'), params![identity], |row| row.get(0))?;
                Ok(held.add(source.signal, usize::try_from(rows).unwrap_or_default()))
            },
        )
    }

    pub fn identity_signal_holders(&self) -> Result<std::collections::BTreeMap<String, usize>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {}", e))?;
        let mut stmt = conn.prepare(&crate::telemetry::signal_holders_sql())?;
        let holders = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    usize::try_from(row.get::<_, i64>(1)?).unwrap_or_default(),
                ))
            })?
            .collect::<rusqlite::Result<_>>()?;
        Ok(holders)
    }

    pub fn reassign_identity_signals(
        &self,
        from: &str,
        to: &str,
    ) -> Result<crate::telemetry::IdentitySignals> {
        self.change_identity_signals(|source| source.moved, from, Some(to))
    }

    pub fn forget_identity_signals(
        &self,
        identity: &str,
    ) -> Result<crate::telemetry::IdentitySignals> {
        self.change_identity_signals(|source| source.erased, identity, None)
    }

    pub fn reassign_identity_billing(&self, from: &str, to: &str) -> Result<usize> {
        let mut conn = self
            .conn
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {}", e))?;
        let tx = conn.transaction()?;
        let changed = crate::telemetry::BILLING_NAMES.into_iter().try_fold(
            0,
            |changed, names| -> Result<_> {
                Ok(changed
                    + tx.execute(&crate::telemetry::rename_sql(names, '?'), params![from, to])?)
            },
        )?;
        tx.commit()?;
        Ok(changed)
    }

    /// Apply `change` to every signal row naming `from`, in one transaction.
    fn change_identity_signals(
        &self,
        change: fn(&crate::telemetry::SignalSource) -> crate::telemetry::Change,
        from: &str,
        to: Option<&str>,
    ) -> Result<crate::telemetry::IdentitySignals> {
        let mut conn = self
            .conn
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {}", e))?;
        let tx = conn.transaction()?;
        let changed = crate::telemetry::SIGNAL_SOURCES.iter().try_fold(
            crate::telemetry::IdentitySignals::default(),
            |changed, source| -> Result<_> {
                let change = change(source);
                let sql = source.change_sql(change, '?');
                let rows = match (change, to) {
                    (crate::telemetry::Change::Rename, Some(to)) => {
                        tx.execute(&sql, params![from, to])?
                    }
                    (crate::telemetry::Change::Rename, None) => {
                        eyre::bail!("a rename needs the identity to name")
                    }
                    _ => tx.execute(&sql, params![from])?,
                };
                Ok(changed.add(source.signal, rows))
            },
        )?;
        tx.commit()?;
        Ok(changed)
    }

    /// Get a workflow instance by ID.
    pub fn get_workflow_instance(&self, id: &str) -> Result<Option<WorkflowInstanceRow>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {}", e))?;
        let mut stmt = conn.prepare(&format!(
            "SELECT {INSTANCE_COLUMNS} FROM workflow_instances WHERE id = ?1"
        ))?;
        let row = stmt.query_row(params![id], row_to_workflow_instance);
        match row {
            Ok(instance) => Ok(Some(instance)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// The instances named by `ids` that exist, in no particular order. The
    /// ids travel as one JSON array bind, so any number fit one statement.
    pub fn get_workflow_instances(&self, ids: &[String]) -> Result<Vec<WorkflowInstanceRow>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {}", e))?;
        let mut stmt = conn.prepare(&format!(
            "SELECT {INSTANCE_COLUMNS} FROM workflow_instances
             WHERE id IN (SELECT value FROM json_each(?1))"
        ))?;
        let rows = stmt
            .query_map(
                params![serde_json::to_string(ids)?],
                row_to_workflow_instance,
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// List workflow instances, optionally filtered by status and/or workflow
    /// id. A bare id matches the system definition and every member's copy of
    /// it (`{owner}/{id}`), so a workflow page lists all of its runs and none of
    /// another's.
    pub fn list_workflow_instances(
        &self,
        status: Option<&str>,
        workflow_id: Option<&str>,
    ) -> Result<Vec<WorkflowInstanceRow>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {}", e))?;
        let sql = format!(
            "SELECT {INSTANCE_COLUMNS} FROM workflow_instances
             WHERE (?1 IS NULL OR status = ?1)
               AND (?2 IS NULL OR workflow_id = ?2 OR substr(workflow_id, -(length(?2) + 1)) = '/' || ?2)
             ORDER BY updated_at DESC"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt
            .query_map(params![status, workflow_id], row_to_workflow_instance)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// The instances `filter` admits, most recently updated first. Times are
    /// compared as instants, since rows carry both RFC 3339 and `datetime()`
    /// text.
    pub fn list_workflow_instances_filtered(
        &self,
        filter: &InstanceFilter,
    ) -> Result<Vec<WorkflowInstanceRow>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {}", e))?;
        let list =
            |values: &Option<Vec<String>>| values.as_ref().map(serde_json::to_string).transpose();
        let runs_as = filter.runs_as.as_ref();
        let sql = format!(
            "SELECT {INSTANCE_COLUMNS} FROM workflow_instances
             WHERE (?1 IS NULL OR id IN (SELECT value FROM json_each(?1)))
               AND (?2 IS NULL OR EXISTS (
                     SELECT 1 FROM json_each(?2) AS wanted
                     WHERE workflow_id = wanted.value
                        OR substr(workflow_id, -(length(wanted.value) + 1)) = '/' || wanted.value))
               AND (?3 IS NULL OR status IN (SELECT value FROM json_each(?3)))
               AND (?4 IS NULL OR julianday(updated_at) >= julianday(?4))
               AND (?5 IS NULL OR agent_id = ?5
                    OR (agent_id IS NULL AND workflow_id IN (SELECT value FROM json_each(?6))))
             ORDER BY julianday(updated_at) DESC
             LIMIT ?7"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt
            .query_map(
                params![
                    list(&filter.ids)?,
                    list(&filter.workflows)?,
                    list(&filter.statuses)?,
                    filter.since.map(|since| since.to_rfc3339()),
                    runs_as.map(|runs_as| runs_as.agent.as_str()),
                    serde_json::to_string(&runs_as.map_or(&[][..], |runs_as| &runs_as.declaring))?,
                    i64::try_from(filter.limit).unwrap_or(i64::MAX),
                ],
                row_to_workflow_instance,
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// One page of a walk through every instance, oldest update first.
    pub fn workflow_instances_after(
        &self,
        after: DateTime<Utc>,
        after_id: &str,
        limit: usize,
    ) -> Result<Vec<WorkflowInstanceRow>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {}", e))?;
        let sql = format!(
            "SELECT {INSTANCE_COLUMNS} FROM workflow_instances
             WHERE julianday(updated_at) > julianday(?1)
                OR (julianday(updated_at) = julianday(?1) AND id > ?2)
             ORDER BY julianday(updated_at), id
             LIMIT ?3"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt
            .query_map(
                params![
                    after.to_rfc3339(),
                    after_id,
                    i64::try_from(limit).unwrap_or(i64::MAX)
                ],
                row_to_workflow_instance,
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// List non-terminal child instances of a given parent instance.
    pub fn list_child_instances(&self, parent_id: &str) -> Result<Vec<WorkflowInstanceRow>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {}", e))?;
        let sql = format!(
            "SELECT {INSTANCE_COLUMNS} FROM workflow_instances
             WHERE parent_instance_id = ?1
               AND status NOT IN ('completed', 'failed', 'exited', 'cancelled')
             ORDER BY created_at ASC"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt
            .query_map(params![parent_id], row_to_workflow_instance)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Update a workflow instance's status and current step.
    pub fn update_workflow_instance_status(
        &self,
        id: &str,
        status: &str,
        current_step_id: Option<&str>,
    ) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {}", e))?;
        let now = Utc::now().to_rfc3339();
        conn.execute(
            "UPDATE workflow_instances
             SET status = ?1, current_step_id = ?2, updated_at = ?3, version = version + 1
             WHERE id = ?4",
            params![status, current_step_id, now, id],
        )?;
        Ok(())
    }

    /// Find a workflow instance by its Slack channel and thread — O(1) indexed lookup.
    /// Returns the most recently updated instance matching the given statuses.
    pub fn find_instance_by_thread(
        &self,
        channel_id: &str,
        thread_id: &str,
        statuses: &[&str],
    ) -> Result<Option<WorkflowInstanceRow>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {}", e))?;
        // Build status placeholders: (?3, ?4, ?5, ...)
        if statuses.is_empty() {
            return Ok(None);
        }
        let placeholders: Vec<String> =
            (0..statuses.len()).map(|i| format!("?{}", i + 3)).collect();
        let sql = format!(
            "SELECT {INSTANCE_COLUMNS} FROM workflow_instances
             WHERE channel_id = ?1 AND thread_id = ?2 AND status IN ({})
             ORDER BY updated_at DESC
             LIMIT 1",
            placeholders.join(", ")
        );
        let mut stmt = conn.prepare(&sql)?;
        let mut params_vec: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
        params_vec.push(Box::new(channel_id.to_string()));
        params_vec.push(Box::new(thread_id.to_string()));
        for s in statuses {
            params_vec.push(Box::new(s.to_string()));
        }
        let params_refs: Vec<&dyn rusqlite::types::ToSql> =
            params_vec.iter().map(|p| p.as_ref()).collect();
        let row = stmt.query_row(params_refs.as_slice(), row_to_workflow_instance);
        match row {
            Ok(instance) => Ok(Some(instance)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Update the channel/thread columns on a workflow instance.
    pub fn set_instance_thread(&self, id: &str, channel_id: &str, thread_id: &str) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {}", e))?;
        conn.execute(
            "UPDATE workflow_instances
             SET channel_id = ?1, thread_id = ?2, version = version + 1
             WHERE id = ?3",
            params![channel_id, thread_id, id],
        )?;
        Ok(())
    }

    /// Get efficiency trends for an agent over time, bucketed by day.
    pub fn get_efficiency_trends(
        &self,
        agent_id: &str,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<Vec<EfficiencyTrendPoint>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| eyre::eyre!("Lock poisoned: {}", e))?;
        let sql = format!(
            "{EFFICIENCY_TRENDS_PROJECTION}
             WHERE agent_id = ?1 AND completed_at >= ?2 AND completed_at <= ?3
             GROUP BY DATE(completed_at)
             ORDER BY bucket ASC"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt
            .query_map(
                params![agent_id, from.to_rfc3339(), to.to_rfc3339()],
                row_to_efficiency_trend,
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }
}

/// One row of `task_telemetry` flattened with the bucketing day. Returned
/// by [`SqliteStore::task_usage_rows`] (and the `TelemetryStore` trait
/// equivalent) for downstream aggregation by [`crate::cost_analytics::CostAnalytics`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskUsageRow {
    pub agent_id: String,
    pub model: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost_usd: f64,
    pub rounds: u32,
    /// `YYYY-MM-DD` derived from `completed_at`.
    pub day: String,
    pub provider: String,
}

/// Daily aggregate of `task_telemetry` activity over a recent window.
/// Returned by [`SqliteStore::daily_usage_rows`] for forecasting.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DailyUsageRow {
    pub day: String,
    pub task_count: u64,
    pub total_cost: f64,
    pub total_rounds: u64,
}

/// Round count statistics for outlier analysis.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RoundStats {
    pub mean: f64,
    pub stddev: f64,
    pub p50: u32,
    pub p90: u32,
    pub p99: u32,
    pub total_tasks: u64,
}

/// Persisted model pricing entry (DW-334).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredModelPricing {
    pub model: String,
    pub provider: String,
    pub input_per_million: f64,
    pub output_per_million: f64,
    pub source: String,     // "default", "api_fetch", "manual"
    pub fetched_at: String, // RFC3339
    pub updated_at: String, // RFC3339
}

/// A policy proposal row for storage.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolicyProposalRow {
    pub id: i64,
    pub agent_id: String,
    pub proposed_rule: String,
    pub justification: String,
    pub status: String,
    pub reviewed_by: Option<String>,
    pub review_note: Option<String>,
    pub created_at: String,
    pub reviewed_at: Option<String>,
}

/// Cache status metrics for an agent, grouped by model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheStatusEntry {
    pub model: String,
    pub total_cache_creation_tokens: u64,
    pub total_cache_read_tokens: u64,
    pub total_input_tokens: u64,
    pub task_count: u64,
}

/// Tool cost breakdown entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCostEntry {
    pub tool_name: String,
    pub invocations: u64,
    pub total_duration_ms: u64,
    pub avg_duration_ms: f64,
    pub total_input_bytes: u64,
    pub total_output_bytes: u64,
    pub successes: u64,
    pub success_rate_pct: f64,
    pub estimated_token_cost: u64,
}

/// One turn of an agent: when it started and the distinct tools it called.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolTurn {
    pub started_at: DateTime<Utc>,
    pub tools: Vec<String>,
}

/// Aggregated cost summary for a ticket (DW-400).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TicketCostSummary {
    pub ticket_id: String,
    pub total_cost_usd: f64,
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub total_rounds: u64,
    pub total_duration_ms: u64,
    pub step_count: u64,
    pub models_used: Vec<String>,
}

/// A cost record for storage.
#[derive(Debug, Clone, Default)]
pub struct CostRecord {
    pub agent_id: String,
    pub model: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// Tokens written to the prompt cache (Anthropic / Gemini / OpenRouter).
    pub cache_creation_input_tokens: u64,
    /// Tokens read from the prompt cache.
    pub cache_read_input_tokens: u64,
    pub cost_usd: f64,
    pub team_id: Option<String>,
    pub task_id: Option<String>,
    pub task_type: Option<String>,
    /// DW-400: ticket identifier for per-ticket cost tracking
    pub ticket_id: Option<String>,
    /// Canonical identity UUID of the user who triggered this work, when
    /// available. `None` for system-initiated tasks (workflow runtime,
    /// knowledge maintenance, etc.) and for rows written before the
    /// per-identity migration.
    pub identity_id: Option<String>,
    /// Brief intent of the call (truncated prompt excerpt, "vision:<model>",
    /// "transcription:<seconds>s", etc.) for the cost-attribution UI.
    pub description: Option<String>,
    /// What caused the call. `None` only on rows written before origins
    /// were recorded.
    pub origin: Option<Origin>,
    pub billing: Billing,
    /// Whose plan paid (a uuid, `team:<id>`, `agent:<id>` or `install`) for
    /// a subscription row.
    pub payer: Option<String>,
    /// The tier the paying login came from, for a subscription row.
    pub credential_tier: Option<CredentialTier>,
    /// Identifies a call that may be recorded more than once; only the first
    /// row under a key is kept.
    pub ledger_key: Option<String>,
}

/// Metered spend of one agent under one identity (`None`: no identity).
#[derive(Debug, Clone, PartialEq)]
pub struct MeteredSpend {
    pub agent_id: String,
    pub identity_id: Option<String>,
    pub cost_usd: f64,
}

impl CostRecord {
    pub(crate) fn origin_str(&self) -> Option<&'static str> {
        self.origin.map(Origin::as_str)
    }

    pub(crate) fn credential_tier_str(&self) -> Option<&'static str> {
        self.credential_tier.map(CredentialTier::as_str)
    }
}

/// A full task telemetry record for storage.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskTelemetryRecord {
    pub agent_id: String,
    pub model: String,
    pub provider: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub cache_read_input_tokens: u64,
    pub cost_usd: f64,
    pub rounds: u32,
    pub duration_ms: u64,
    pub tool_invocation_count: u32,
    pub success: bool,
    pub error_type: Option<String>,
    pub retry_count: u32,
    pub task_type: Option<String>,
    pub task_id: Option<String>,
    pub started_at: String,
    pub completed_at: String,
    // Cost forecasting fields (DW-249)
    pub forecast_cost_usd: Option<f64>,
    pub forecast_rounds: Option<u32>,
    pub forecast_confidence: Option<f64>,
    // Team context (DW-217)
    pub team_id: Option<String>,
    pub session_id: Option<String>,
    // Model switches tracking (DW-267)
    pub model_switches: Option<String>,
    // DW-400: per-ticket cost tracking
    pub ticket_id: Option<String>,
    /// Canonical identity UUID of the user who triggered this task.
    pub identity_id: Option<String>,
    /// Brief description / excerpt of the task prompt for cost attribution UI.
    pub description: Option<String>,
}

/// Per-round telemetry detail.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoundTelemetryRecord {
    pub round_number: u32,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub latency_ms: u64,
    pub cache_creation_tokens: u64,
    pub cache_read_tokens: u64,
    pub prompt_size_bytes: u64,
    pub response_size_bytes: u64,
    pub tool_count: u32,
    pub stop_reason: String,
}

/// Per-tool-call telemetry detail.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolTelemetryRecord {
    pub round_number: u32,
    pub tool_name: String,
    pub success: bool,
    pub duration_ms: u64,
    pub policy_decision: String,
    pub input_size_bytes: u64,
    pub output_size_bytes: u64,
    /// Structured denial reason from the policy engine (DW-187).
    pub denial_reason: Option<String>,
}

/// Agent's preferred model for a task type.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentModelPreference {
    pub agent_id: String,
    pub task_type: String,
    pub preferred_model: String,
    pub reason: Option<String>,
    pub updated_at: String,
}

/// Summary of an agent's usage stats.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentUsageSummary {
    pub group_key: String,
    pub total_tasks: u64,
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub total_cost: f64,
    pub avg_rounds: f64,
    pub avg_duration_ms: f64,
    pub success_rate: f64,
    pub total_cache_created_tokens: u64,
    pub total_cache_read_tokens: u64,
}

/// Per-model comparison stats.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelComparison {
    pub model: String,
    pub total_tasks: u64,
    pub avg_input_tokens: f64,
    pub avg_output_tokens: f64,
    pub avg_cost: f64,
    pub avg_rounds: f64,
    pub avg_latency_ms: f64,
    pub success_rate: f64,
    pub total_cost: f64,
    pub avg_cache_read_tokens: f64,
    pub total_cache_read_tokens: f64,
}

/// A task history entry (summary view).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskHistoryEntry {
    pub id: i64,
    pub agent_id: String,
    pub model: String,
    pub task_type: Option<String>,
    pub task_id: Option<String>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub cache_read_input_tokens: u64,
    pub cost_usd: f64,
    pub rounds: u32,
    pub duration_ms: u64,
    pub success: bool,
    pub completed_at: String,
    pub model_switches: Option<String>,
    // DW-400: per-ticket cost tracking
    pub ticket_id: Option<String>,
    /// Canonical identity UUID of the user who triggered this task.
    pub identity_id: Option<String>,
    /// Number of rounds recorded against this task (sub-query).
    pub round_count: u32,
    /// Number of tool calls recorded against this task (sub-query).
    pub tool_call_count: u32,
}

/// Filter for cursor-paginated task history queries.
///
/// Optional fields are AND-ed. The cursor is opaque (base64-encoded
/// `(completed_at, id)`) and is decoded by the call site via
/// [`crate::pagination::decode_cursor`] before being passed in. `limit`
/// is clamped to a sane range by [`crate::pagination::clamp_limit`].
#[derive(Debug, Clone, Default)]
pub struct TaskHistoryFilter {
    pub identity_id: Option<String>,
    pub agent_id: Option<String>,
    pub task_type: Option<String>,
    pub model: Option<String>,
    pub since: Option<DateTime<Utc>>,
    pub until: Option<DateTime<Utc>>,
    /// Decoded cursor — `None` means "first page". `Some((ts, id))` returns
    /// rows strictly older than `(ts, id)` lexicographically.
    pub cursor: Option<(DateTime<Utc>, i64)>,
    pub limit: usize,
}

/// One page of paginated task history. `next_cursor` is `Some` exactly
/// when another page exists.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskHistoryPage {
    pub entries: Vec<TaskHistoryEntry>,
    pub next_cursor: Option<String>,
}

/// End-to-end trace for one task.
///
/// Joins `task_telemetry` with its `round_telemetry`, `tool_telemetry`
/// children and every `cost_records` row whose `task_id` matches. The
/// matching cost records may be more than one when a task spans multiple
/// LLM calls. `task` is `None` only when the application-layer `task_id`
/// has no matching telemetry row.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskTrace {
    pub task: TaskTelemetryRecord,
    pub rounds: Vec<RoundTelemetryRecord>,
    pub tools: Vec<ToolTelemetryRecord>,
    pub cost_records: Vec<CostRecordView>,
}

/// Read-only projection of [`CostRecord`] suitable for trace output —
/// includes the row's timestamp (which `CostRecord` doesn't carry since
/// it's set at insert time) so the UI can order or align cost lines.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CostRecordView {
    pub timestamp: String,
    pub agent_id: String,
    pub model: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub cache_read_input_tokens: u64,
    pub cost_usd: f64,
    pub team_id: Option<String>,
    pub task_id: Option<String>,
    pub task_type: Option<String>,
    pub ticket_id: Option<String>,
    pub identity_id: Option<String>,
    pub origin: Option<String>,
    pub billing: Billing,
    pub payer: Option<String>,
    pub credential_tier: Option<CredentialTier>,
}

/// One row from the cost-attribution ledger (`cost_records`). One row per
/// billable LLM call. `user_id` is an alias for the `identity_id` column.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CostDetailRow {
    pub id: i64,
    pub timestamp: String,
    pub agent_id: String,
    pub model: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub cache_read_input_tokens: u64,
    pub cost_usd: f64,
    pub team_id: Option<String>,
    pub task_id: Option<String>,
    pub task_type: Option<String>,
    pub ticket_id: Option<String>,
    pub user_id: Option<String>,
    pub description: Option<String>,
    pub origin: Option<String>,
    pub billing: Billing,
    pub payer: Option<String>,
    pub credential_tier: Option<CredentialTier>,
}

/// Which calls a cost query covers within its time window.
#[derive(Debug, Clone, Copy, Default)]
pub struct CostScope<'a> {
    pub agent_id: Option<&'a str>,
    /// Matches `identity_id`.
    pub user_id: Option<&'a str>,
}

/// The cost ledger page's filter: a scope, and one billing or both.
#[derive(Debug, Clone, Copy, Default)]
pub struct CostDetailFilter<'a> {
    pub scope: CostScope<'a>,
    pub billing: Option<Billing>,
}

/// The tier a cost row's `credential_tier` column names, if any.
pub(crate) fn tier_column(value: Option<String>) -> Option<CredentialTier> {
    value.as_deref().and_then(CredentialTier::from_column)
}

/// Calls and their cost under one agent, billing, origin, payer and tier.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CostSplitRow {
    pub agent_id: String,
    pub billing: Billing,
    /// `None` on rows recorded before origins were.
    pub origin: Option<String>,
    pub payer: Option<String>,
    pub credential_tier: Option<CredentialTier>,
    pub calls: u64,
    pub cost_usd: f64,
}

/// What a set of calls cost on metered keys, and what the ones a
/// subscription served would have cost at list price.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct BillingSplit {
    pub metered_cost_usd: f64,
    pub subscription_cost_usd: f64,
    pub subscription_calls: u64,
}

impl BillingSplit {
    pub fn of<'a>(rows: impl IntoIterator<Item = &'a CostSplitRow>) -> Self {
        rows.into_iter().map(Self::from).sum()
    }

    /// Metered spend and subscription value together.
    pub fn total_cost_usd(&self) -> f64 {
        self.metered_cost_usd + self.subscription_cost_usd
    }
}

impl From<&CostSplitRow> for BillingSplit {
    fn from(row: &CostSplitRow) -> Self {
        match row.billing {
            Billing::Subscription => Self {
                subscription_cost_usd: row.cost_usd,
                subscription_calls: row.calls,
                ..Self::default()
            },
            Billing::Metered => Self {
                metered_cost_usd: row.cost_usd,
                ..Self::default()
            },
        }
    }
}

impl std::ops::Add for BillingSplit {
    type Output = Self;

    fn add(self, other: Self) -> Self {
        Self {
            metered_cost_usd: self.metered_cost_usd + other.metered_cost_usd,
            subscription_cost_usd: self.subscription_cost_usd + other.subscription_cost_usd,
            subscription_calls: self.subscription_calls + other.subscription_calls,
        }
    }
}

impl std::iter::Sum for BillingSplit {
    fn sum<I: Iterator<Item = Self>>(splits: I) -> Self {
        splits.fold(Self::default(), std::ops::Add::add)
    }
}

/// A page of cost-attribution rows plus distinct dimensions for UI filters.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CostDetailPage {
    pub records: Vec<CostDetailRow>,
    pub total: u64,
    pub distinct_users: Vec<String>,
    pub distinct_agents: Vec<String>,
}

/// One row from the scheduled-job execution log (rows in `task_telemetry`
/// where `provider IN ('job_runner', 'scheduler')`). `cost_usd` is non-zero
/// only for `scheduler` rows (agent-turn jobs); pure tool/shell/monitor jobs
/// emit no LLM calls and therefore record zero.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScheduledJobRow {
    pub id: i64,
    pub agent_id: String,
    pub task_type: Option<String>,
    pub task_id: Option<String>,
    pub description: Option<String>,
    pub success: bool,
    pub error_type: Option<String>,
    pub duration_ms: u64,
    pub started_at: String,
    pub completed_at: String,
    pub user_id: Option<String>,
    pub cost_usd: f64,
}

/// A page of scheduled-job execution rows.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScheduledJobHistoryPage {
    pub records: Vec<ScheduledJobRow>,
    pub total: u64,
}

/// A time-bucketed efficiency trend data point.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EfficiencyTrendPoint {
    pub bucket: String,
    pub total_tasks: u64,
    pub avg_input_tokens: f64,
    pub avg_output_tokens: f64,
    pub avg_cost: f64,
    pub avg_rounds: f64,
    pub avg_duration_ms: f64,
    pub success_rate: f64,
    pub avg_cache_read_tokens: f64,
}

#[derive(Debug, Clone, Default)]
pub struct WorkflowInstanceRow {
    pub id: String,
    pub workflow_id: String,
    pub status: String,
    pub current_step_id: Option<String>,
    /// JSON-serialized Vec<StepRecord>.
    pub step_history: String,
    /// JSON-serialized variables bag.
    pub variables: String,
    /// JSON-serialized TriggerInfo.
    pub trigger_json: String,
    pub agent_id: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    /// Parent workflow instance ID (for cascading abort).
    pub parent_instance_id: Option<String>,
    /// Slack channel ID — first-class column for O(1) command routing.
    pub channel_id: Option<String>,
    /// Slack thread timestamp — first-class column for O(1) command routing.
    pub thread_id: Option<String>,
    /// The hub process holding the execution lease. Read only: the lease
    /// methods write it, a row save never does.
    pub owner_instance: Option<String>,
    /// When the execution lease lapses (RFC 3339). Read only, as
    /// `owner_instance`.
    pub lease_expires_at: Option<String>,
    /// Moves on with every content write, never with the lease. Read only:
    /// [`SqliteStore::save_workflow_instance_fenced`] is conditional on it.
    pub version: i64,
}

/// Which workflow instances a filtered listing returns, most recently
/// updated first. An absent constraint admits every instance; an empty list
/// admits none.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct InstanceFilter {
    pub ids: Option<Vec<String>>,
    /// Bare workflow ids, each matching the system definition and every
    /// member's copy of it (`{owner}/{id}`).
    pub workflows: Option<Vec<String>>,
    pub statuses: Option<Vec<String>>,
    /// Updated at or after this moment.
    pub since: Option<DateTime<Utc>>,
    pub runs_as: Option<RunsAs>,
    pub limit: usize,
}

/// The instances that execute as one agent: those that recorded it at start,
/// and those that recorded none whose workflow key is in `declaring`, the
/// definitions that declare it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RunsAs {
    pub agent: String,
    pub declaring: Vec<String>,
}

/// Flat row representation of a task for storage.
#[derive(Debug, Clone)]
pub struct TaskRow {
    pub id: String,
    pub title: String,
    pub description: String,
    pub status: String,
    pub priority: String,
    pub assignee: Option<String>,
    pub team_id: Option<String>,
    pub labels: String,
    pub parent_id: Option<String>,
    pub result_json: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub started_at: Option<String>,
    pub completed_at: Option<String>,
}

fn parse_ts_col(idx: usize, raw: String) -> rusqlite::Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(&raw)
        .map(|d| d.with_timezone(&Utc))
        .map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(idx, rusqlite::types::Type::Text, Box::new(e))
        })
}

fn row_to_relationship_event(
    row: &rusqlite::Row,
) -> rusqlite::Result<crate::telemetry::RelationshipEvent> {
    Ok(crate::telemetry::RelationshipEvent {
        timestamp: parse_ts_col(0, row.get(0)?)?,
        agent_id: row.get(1)?,
        identity_id: row.get(2)?,
        session_id: row.get(3)?,
        message_id: row.get(4)?,
        valence: row.get(5)?,
        intensity: row.get(6)?,
        emotion_label: row.get(7)?,
        reaction: row.get(8)?,
        is_key_moment: row.get::<_, i64>(9)? != 0,
        confidence: row.get(10)?,
        source: row.get(11)?,
        signals: row.get(12)?,
    })
}

fn row_to_audit(row: &rusqlite::Row) -> rusqlite::Result<AuditEntry> {
    let outcome_str: String = row.get(5)?;
    Ok(AuditEntry {
        timestamp: row.get(0)?,
        actor: row.get(1)?,
        action: row.get(2)?,
        resource: row.get(3)?,
        detail: row.get(4)?,
        outcome: match outcome_str.as_str() {
            "denied" => AuditOutcome::Denied,
            "error" => AuditOutcome::Error,
            _ => AuditOutcome::Success,
        },
    })
}

/// Marks the cost rows written before `billing` existed that a subscription
/// paid for, by the ledger key's route prefix. Bedrock-billed Claude Code
/// rounds are metered.
pub(crate) const SUBSCRIPTION_BILLING_BACKFILL: &str =
    "UPDATE cost_records SET billing = 'subscription'
 WHERE (model LIKE 'claude-code:%' AND model NOT LIKE 'claude-code:bedrock:%')
    OR model LIKE 'codex:%' OR model LIKE 'claude-pilot%';";

/// The columns [`row_to_workflow_instance`] reads, in its order. SQLite keeps
/// the lease time as `datetime()` text; it is read back as RFC 3339.
const INSTANCE_COLUMNS: &str = "id, workflow_id, status, current_step_id, step_history,
     variables, trigger_json, agent_id, created_at, updated_at,
     parent_instance_id, channel_id, thread_id, owner_instance,
     strftime('%Y-%m-%dT%H:%M:%SZ', lease_expires_at), version";

fn row_to_workflow_instance(row: &rusqlite::Row) -> rusqlite::Result<WorkflowInstanceRow> {
    Ok(WorkflowInstanceRow {
        id: row.get(0)?,
        workflow_id: row.get(1)?,
        status: row.get(2)?,
        current_step_id: row.get(3)?,
        step_history: row.get(4)?,
        variables: row.get(5)?,
        trigger_json: row.get(6)?,
        agent_id: row.get(7)?,
        created_at: row.get(8)?,
        updated_at: row.get(9)?,
        parent_instance_id: row.get(10)?,
        channel_id: row.get(11)?,
        thread_id: row.get(12)?,
        owner_instance: row.get(13)?,
        lease_expires_at: row.get(14)?,
        version: row.get(15)?,
    })
}

const MODEL_COMPARISON_PROJECTION: &str = "
    SELECT model,
           COUNT(*),
           AVG(input_tokens),
           AVG(output_tokens),
           AVG(cost_usd),
           AVG(rounds),
           AVG(duration_ms),
           AVG(CASE WHEN success THEN 1.0 ELSE 0.0 END),
           SUM(cost_usd),
           AVG(cache_read_input_tokens),
           SUM(cache_read_input_tokens)
    FROM task_telemetry
";

const TOOL_BREAKDOWN_PROJECTION: &str = "
    SELECT tt.tool_name,
           COUNT(*) as invocations,
           SUM(tt.duration_ms) as total_duration,
           AVG(tt.duration_ms) as avg_duration,
           SUM(tt.input_size_bytes) as total_input_bytes,
           SUM(tt.output_size_bytes) as total_output_bytes,
           SUM(CASE WHEN tt.success = 1 THEN 1 ELSE 0 END) as successes
    FROM tool_telemetry tt
    JOIN task_telemetry t ON tt.task_telemetry_id = t.id
";

const EFFICIENCY_TRENDS_PROJECTION: &str = "
    SELECT DATE(completed_at) as bucket,
           COUNT(*),
           AVG(input_tokens),
           AVG(output_tokens),
           AVG(cost_usd),
           AVG(rounds),
           AVG(duration_ms),
           AVG(CASE WHEN success THEN 1.0 ELSE 0.0 END),
           AVG(cache_read_input_tokens)
    FROM task_telemetry
";

fn row_to_efficiency_trend(row: &rusqlite::Row) -> rusqlite::Result<EfficiencyTrendPoint> {
    Ok(EfficiencyTrendPoint {
        bucket: row.get(0)?,
        total_tasks: row.get::<_, i64>(1)? as u64,
        avg_input_tokens: row.get(2)?,
        avg_output_tokens: row.get(3)?,
        avg_cost: row.get(4)?,
        avg_rounds: row.get(5)?,
        avg_duration_ms: row.get(6)?,
        success_rate: row.get(7)?,
        avg_cache_read_tokens: row.get(8)?,
    })
}

/// Compute mean / stddev / p50 / p90 / p99 from an ascending-sorted
/// slice of round counts. Returns `None` for an empty input so callers
/// can short-circuit without dividing by zero.
pub(crate) fn round_stats_from(round_counts: &[u32]) -> Option<RoundStats> {
    if round_counts.is_empty() {
        return None;
    }
    let total_tasks = round_counts.len() as u64;
    let mean = round_counts.iter().map(|&r| r as f64).sum::<f64>() / total_tasks as f64;
    let variance = round_counts
        .iter()
        .map(|&r| (r as f64 - mean).powi(2))
        .sum::<f64>()
        / total_tasks as f64;
    let stddev = variance.sqrt();
    let last = round_counts.len() - 1;
    Some(RoundStats {
        mean,
        stddev,
        p50: round_counts[round_counts.len() / 2],
        p90: round_counts[(round_counts.len() as f64 * 0.9) as usize],
        p99: round_counts[((round_counts.len() as f64 * 0.99) as usize).min(last)],
        total_tasks,
    })
}

fn token_history_filtered<P>(
    store: &SqliteStore,
    where_clause: &str,
    bind: P,
) -> Result<Vec<TokenHistoryPoint>>
where
    P: rusqlite::Params,
{
    let conn = store
        .conn
        .lock()
        .map_err(|e| eyre::eyre!("Lock poisoned: {}", e))?;
    let sql = format!(
        "SELECT substr(timestamp, 1, 10) AS day,
                COALESCE(SUM(input_tokens), 0),
                COALESCE(SUM(output_tokens), 0),
                COALESCE(SUM(cache_creation_input_tokens), 0),
                COALESCE(SUM(cache_read_input_tokens), 0)
         FROM cost_records
         WHERE {where_clause}
         GROUP BY day ORDER BY day ASC"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(bind, |row| {
            let day: String = row.get(0)?;
            let bucket_start = chrono::NaiveDate::parse_from_str(&day, "%Y-%m-%d")
                .map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        0,
                        rusqlite::types::Type::Text,
                        Box::new(e),
                    )
                })?
                .and_hms_opt(0, 0, 0)
                .unwrap()
                .and_utc();
            Ok(TokenHistoryPoint {
                bucket_start,
                input_tokens: row.get::<_, i64>(1)? as u64,
                output_tokens: row.get::<_, i64>(2)? as u64,
                cache_creation_input_tokens: row.get::<_, i64>(3)? as u64,
                cache_read_input_tokens: row.get::<_, i64>(4)? as u64,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

fn row_to_agent_usage_summary(row: &rusqlite::Row) -> rusqlite::Result<AgentUsageSummary> {
    Ok(AgentUsageSummary {
        group_key: row.get(0)?,
        total_tasks: row.get::<_, i64>(1)? as u64,
        total_input_tokens: row.get::<_, i64>(2)? as u64,
        total_output_tokens: row.get::<_, i64>(3)? as u64,
        total_cost: row.get(4)?,
        avg_rounds: row.get(5)?,
        avg_duration_ms: row.get(6)?,
        success_rate: row.get(7)?,
        total_cache_created_tokens: row.get::<_, i64>(8)? as u64,
        total_cache_read_tokens: row.get::<_, i64>(9)? as u64,
    })
}

fn row_to_model_comparison(row: &rusqlite::Row) -> rusqlite::Result<ModelComparison> {
    Ok(ModelComparison {
        model: row.get(0)?,
        total_tasks: row.get::<_, i64>(1)? as u64,
        avg_input_tokens: row.get(2)?,
        avg_output_tokens: row.get(3)?,
        avg_cost: row.get(4)?,
        avg_rounds: row.get(5)?,
        avg_latency_ms: row.get(6)?,
        success_rate: row.get(7)?,
        total_cost: row.get(8)?,
        avg_cache_read_tokens: row.get(9)?,
        total_cache_read_tokens: row.get::<_, f64>(10)?,
    })
}

fn row_to_tool_cost_entry(row: &rusqlite::Row) -> rusqlite::Result<ToolCostEntry> {
    let invocations = row.get::<_, i64>(1)? as u64;
    let successes = row.get::<_, i64>(6)? as u64;
    let total_output_bytes = row.get::<_, i64>(5)? as u64;
    Ok(ToolCostEntry {
        tool_name: row.get(0)?,
        invocations,
        total_duration_ms: row.get::<_, i64>(2)? as u64,
        avg_duration_ms: row.get(3)?,
        total_input_bytes: row.get::<_, i64>(4)? as u64,
        total_output_bytes,
        successes,
        success_rate_pct: if invocations > 0 {
            successes as f64 / invocations as f64 * 100.0
        } else {
            0.0
        },
        estimated_token_cost: total_output_bytes / 4,
    })
}

fn row_to_task_telemetry_record(row: &rusqlite::Row) -> rusqlite::Result<TaskTelemetryRecord> {
    Ok(TaskTelemetryRecord {
        agent_id: row.get(1)?,
        model: row.get(2)?,
        provider: row.get(3)?,
        input_tokens: row.get::<_, i64>(4)? as u64,
        output_tokens: row.get::<_, i64>(5)? as u64,
        cache_creation_input_tokens: row.get::<_, i64>(6)? as u64,
        cache_read_input_tokens: row.get::<_, i64>(7)? as u64,
        cost_usd: row.get(8)?,
        rounds: row.get::<_, i64>(9)? as u32,
        duration_ms: row.get::<_, i64>(10)? as u64,
        tool_invocation_count: row.get::<_, i64>(11)? as u32,
        success: row.get::<_, i64>(12)? != 0,
        error_type: row.get(13)?,
        retry_count: row.get::<_, i64>(14)? as u32,
        task_type: row.get(15)?,
        task_id: row.get(16)?,
        started_at: row.get(17)?,
        completed_at: row.get(18)?,
        forecast_cost_usd: row.get(19)?,
        forecast_rounds: row.get::<_, Option<i64>>(20)?.map(|r| r as u32),
        forecast_confidence: row.get(21)?,
        team_id: row.get(22)?,
        session_id: row.get(23)?,
        model_switches: row.get(24)?,
        ticket_id: row.get(25)?,
        identity_id: row.get(26)?,
        description: row.get(27)?,
    })
}

/// Projection used by every `TaskHistoryEntry`-yielding query so the
/// `row.get(...)` indices in [`row_to_task_history`] stay in sync.
/// Includes per-row `round_count` and `tool_call_count` subqueries so
/// callers can render task depth without a separate round-trip.
const TASK_HISTORY_COLUMNS: &str = "
    id, agent_id, model, task_type, task_id,
    input_tokens, output_tokens,
    cache_creation_input_tokens, cache_read_input_tokens,
    cost_usd, rounds, duration_ms, success, completed_at,
    model_switches, ticket_id, identity_id,
    (SELECT COUNT(*) FROM round_telemetry r WHERE r.task_telemetry_id = task_telemetry.id) AS round_count,
    (SELECT COUNT(*) FROM tool_telemetry t  WHERE t.task_telemetry_id = task_telemetry.id) AS tool_call_count
";

fn row_to_task_history(row: &rusqlite::Row) -> rusqlite::Result<TaskHistoryEntry> {
    Ok(TaskHistoryEntry {
        id: row.get(0)?,
        agent_id: row.get(1)?,
        model: row.get(2)?,
        task_type: row.get(3)?,
        task_id: row.get(4)?,
        input_tokens: row.get::<_, i64>(5)? as u64,
        output_tokens: row.get::<_, i64>(6)? as u64,
        cache_creation_input_tokens: row.get::<_, i64>(7)? as u64,
        cache_read_input_tokens: row.get::<_, i64>(8)? as u64,
        cost_usd: row.get(9)?,
        rounds: row.get::<_, i64>(10)? as u32,
        duration_ms: row.get::<_, i64>(11)? as u64,
        success: row.get::<_, i64>(12)? != 0,
        completed_at: row.get(13)?,
        model_switches: row.get(14)?,
        ticket_id: row.get(15)?,
        identity_id: row.get(16)?,
        round_count: row.get::<_, i64>(17)? as u32,
        tool_call_count: row.get::<_, i64>(18)? as u32,
    })
}

fn row_to_task(row: &rusqlite::Row) -> rusqlite::Result<TaskRow> {
    Ok(TaskRow {
        id: row.get(0)?,
        title: row.get(1)?,
        description: row.get(2)?,
        status: row.get(3)?,
        priority: row.get(4)?,
        assignee: row.get(5)?,
        team_id: row.get(6)?,
        labels: row.get(7)?,
        parent_id: row.get(8)?,
        result_json: row.get(9)?,
        created_at: row.get(10)?,
        updated_at: row.get(11)?,
        started_at: row.get(12)?,
        completed_at: row.get(13)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relationship_events_round_trip_and_idempotency() {
        use crate::telemetry::{RelationshipCursor, RelationshipEvent};
        let store = SqliteStore::in_memory().unwrap();
        let t0 = Utc::now();
        let mk = |msg: &str, identity: &str, valence: f64| RelationshipEvent {
            timestamp: t0,
            agent_id: "atlas".into(),
            identity_id: identity.into(),
            session_id: "sess-1".into(),
            message_id: msg.into(),
            valence,
            intensity: 0.5,
            emotion_label: Some("grateful".into()),
            reaction: "gratitude".into(),
            is_key_moment: true,
            confidence: 0.8,
            source: "code".into(),
            signals: "{}".into(),
        };

        assert!(store
            .record_relationship_event(&mk("m1", "alice", 0.7))
            .unwrap());
        assert!(
            !store
                .record_relationship_event(&mk("m1", "alice", -0.9))
                .unwrap(),
            "same (session_id, message_id) must be ignored"
        );
        store
            .record_relationship_event(&mk("m2", "bob", -0.3))
            .unwrap();

        let from = t0 - chrono::Duration::hours(1);
        let to = t0 + chrono::Duration::hours(1);

        let alice = store
            .relationship_events_in_range(Some("atlas"), Some("alice"), from, to)
            .unwrap();
        assert_eq!(alice.len(), 1, "identity scope must isolate users");
        assert_eq!(alice[0].valence, 0.7, "conflict must keep the first row");
        assert_eq!(alice[0].reaction, "gratitude");
        assert!(alice[0].is_key_moment);

        let all = store
            .relationship_events_in_range(Some("atlas"), None, from, to)
            .unwrap();
        assert_eq!(all.len(), 2);

        assert!(store.get_relationship_cursor("sess-1").unwrap().is_none());
        let mut cursor = RelationshipCursor {
            session_id: "sess-1".into(),
            agent_id: "atlas".into(),
            last_message_ts: t0,
            last_message_id: "m2".into(),
            updated_at: t0,
        };
        store.upsert_relationship_cursor(&cursor).unwrap();
        assert_eq!(
            store
                .get_relationship_cursor("sess-1")
                .unwrap()
                .unwrap()
                .last_message_id,
            "m2"
        );
        cursor.last_message_id = "m9".into();
        store.upsert_relationship_cursor(&cursor).unwrap();
        assert_eq!(
            store
                .get_relationship_cursor("sess-1")
                .unwrap()
                .unwrap()
                .last_message_id,
            "m9"
        );
    }

    #[test]
    fn create_and_retrieve_task() {
        let store = SqliteStore::in_memory().unwrap();
        let now = Utc::now().to_rfc3339();

        let task = TaskRow {
            id: "task-1".to_string(),
            title: "Build feature".to_string(),
            description: "Implement the thing".to_string(),
            status: "created".to_string(),
            priority: "normal".to_string(),
            assignee: None,
            team_id: Some("dev".to_string()),
            labels: "[]".to_string(),
            parent_id: None,
            result_json: None,
            created_at: now.clone(),
            updated_at: now,
            started_at: None,
            completed_at: None,
        };

        store.save_task(&task).unwrap();

        let retrieved = store.get_task("task-1").unwrap().unwrap();
        assert_eq!(retrieved.title, "Build feature");
        assert_eq!(retrieved.team_id, Some("dev".to_string()));

        assert!(store.get_task("nonexistent").unwrap().is_none());
    }

    #[test]
    fn list_tasks_with_filter() {
        let store = SqliteStore::in_memory().unwrap();
        let now = Utc::now().to_rfc3339();

        for (i, status) in ["created", "in_progress", "complete"].iter().enumerate() {
            store
                .save_task(&TaskRow {
                    id: format!("task-{}", i),
                    title: format!("Task {}", i),
                    description: String::new(),
                    status: status.to_string(),
                    priority: "normal".to_string(),
                    assignee: None,
                    team_id: None,
                    labels: "[]".to_string(),
                    parent_id: None,
                    result_json: None,
                    created_at: now.clone(),
                    updated_at: now.clone(),
                    started_at: None,
                    completed_at: None,
                })
                .unwrap();
        }

        assert_eq!(store.list_tasks(None).unwrap().len(), 3);
        assert_eq!(store.list_tasks(Some("created")).unwrap().len(), 1);
        assert_eq!(store.list_tasks(Some("complete")).unwrap().len(), 1);
    }

    #[test]
    fn update_task_status() {
        let store = SqliteStore::in_memory().unwrap();
        let now = Utc::now().to_rfc3339();

        store
            .save_task(&TaskRow {
                id: "task-1".to_string(),
                title: "Task".to_string(),
                description: String::new(),
                status: "created".to_string(),
                priority: "normal".to_string(),
                assignee: None,
                team_id: None,
                labels: "[]".to_string(),
                parent_id: None,
                result_json: None,
                created_at: now.clone(),
                updated_at: now,
                started_at: None,
                completed_at: None,
            })
            .unwrap();

        store.update_task_status("task-1", "in_progress").unwrap();
        let task = store.get_task("task-1").unwrap().unwrap();
        assert_eq!(task.status, "in_progress");
    }

    #[test]
    fn audit_log_roundtrip() {
        let store = SqliteStore::in_memory().unwrap();

        store
            .record_audit(
                "agent-1",
                "tool.execute",
                "filesystem.read",
                Some("/tmp/test"),
                AuditOutcome::Success,
            )
            .unwrap();
        store
            .record_audit(
                "agent-2",
                "task.complete",
                "task-123",
                None,
                AuditOutcome::Error,
            )
            .unwrap();

        let recent = store.recent_audit(10).unwrap();
        assert_eq!(recent.len(), 2);
        assert_eq!(recent[0].actor, "agent-2"); // Most recent first

        let search = store.search_audit("filesystem", 10).unwrap();
        assert_eq!(search.len(), 1);
        assert_eq!(search[0].actor, "agent-1");
    }

    #[test]
    fn cost_tracking() {
        let store = SqliteStore::in_memory().unwrap();

        store
            .record_cost(&CostRecord {
                agent_id: "agent-1".into(),
                model: "claude-sonnet".into(),
                input_tokens: 1000,
                output_tokens: 500,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 0,
                cost_usd: 0.015,
                team_id: Some("dev".into()),
                task_id: Some("task-1".into()),
                task_type: None,
                ticket_id: None,
                identity_id: None,
                description: None,
                ..Default::default()
            })
            .unwrap();
        store
            .record_cost(&CostRecord {
                agent_id: "agent-2".into(),
                model: "claude-sonnet".into(),
                input_tokens: 2000,
                output_tokens: 1000,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 0,
                cost_usd: 0.03,
                team_id: Some("dev".into()),
                task_id: Some("task-2".into()),
                task_type: None,
                ticket_id: None,
                identity_id: None,
                description: None,
                ..Default::default()
            })
            .unwrap();

        let from = Utc::now() - chrono::Duration::hours(1);
        let to = Utc::now() + chrono::Duration::hours(1);

        let total = store.total_cost(from, to).unwrap();
        assert!((total - 0.045).abs() < 0.001);

        let by_agent = store.cost_by_agent(from, to).unwrap();
        assert_eq!(by_agent.len(), 2);
        assert_eq!(by_agent[0].0, "agent-2"); // Highest cost first
    }

    #[test]
    fn cost_token_history_by_day_buckets_and_zero_fills() {
        let store = SqliteStore::in_memory().unwrap();

        store
            .record_cost(&CostRecord {
                agent_id: "agent-1".into(),
                model: "claude-sonnet".into(),
                input_tokens: 1_000,
                output_tokens: 500,
                cache_creation_input_tokens: 100,
                cache_read_input_tokens: 200,
                cost_usd: 0.01,
                team_id: None,
                task_id: None,
                task_type: None,
                ticket_id: None,
                identity_id: None,
                description: None,
                ..Default::default()
            })
            .unwrap();
        store
            .record_cost(&CostRecord {
                agent_id: "agent-2".into(),
                model: "claude-sonnet".into(),
                input_tokens: 3_000,
                output_tokens: 1_500,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 0,
                cost_usd: 0.03,
                team_id: None,
                task_id: None,
                task_type: None,
                ticket_id: None,
                identity_id: None,
                description: None,
                ..Default::default()
            })
            .unwrap();

        // Backdate one record by two days via raw SQL — `record_cost` always
        // stamps `Utc::now()`, so this is the only way to exercise multi-day
        // bucketing in a unit test.
        let two_days_ago = (Utc::now() - chrono::Duration::days(2)).to_rfc3339();
        {
            let conn = store.conn.lock().unwrap();
            conn.execute(
                "INSERT INTO cost_records (timestamp, agent_id, model, input_tokens, output_tokens,
                    cache_creation_input_tokens, cache_read_input_tokens, cost_usd)
                 VALUES (?1, 'agent-3', 'claude-sonnet', 7000, 2000, 0, 0, 0.05)",
                params![two_days_ago],
            )
            .unwrap();
        }

        let from = Utc::now() - chrono::Duration::days(5);
        let to = Utc::now() + chrono::Duration::hours(1);
        let buckets = store.cost_token_history_by_day(from, to).unwrap();

        // Two distinct UTC days returned, sorted ascending.
        assert_eq!(buckets.len(), 2);
        assert!(buckets[0].bucket_start < buckets[1].bucket_start);

        let backdated = &buckets[0];
        assert_eq!(backdated.input_tokens, 7_000);
        assert_eq!(backdated.output_tokens, 2_000);

        let today = &buckets[1];
        assert_eq!(today.input_tokens, 4_000);
        assert_eq!(today.output_tokens, 2_000);
        assert_eq!(today.cache_creation_input_tokens, 100);
        assert_eq!(today.cache_read_input_tokens, 200);

        // Empty range yields empty vec (callers handle zero-fill).
        let empty = store
            .cost_token_history_by_day(
                Utc::now() - chrono::Duration::days(30),
                Utc::now() - chrono::Duration::days(20),
            )
            .unwrap();
        assert!(empty.is_empty());
    }

    #[test]
    fn telemetry_record_and_query() {
        let store = SqliteStore::in_memory().unwrap();

        let task = TaskTelemetryRecord {
            agent_id: "agent-1".to_string(),
            model: "claude-sonnet-4-20250514".to_string(),
            provider: "anthropic".to_string(),
            input_tokens: 5000,
            output_tokens: 2000,
            cache_creation_input_tokens: 1000,
            cache_read_input_tokens: 500,
            cost_usd: 0.045,
            rounds: 3,
            duration_ms: 5000,
            tool_invocation_count: 2,
            success: true,
            error_type: None,
            retry_count: 0,
            task_type: Some("coding".to_string()),
            task_id: Some("task-1".to_string()),
            started_at: Utc::now().to_rfc3339(),
            completed_at: Utc::now().to_rfc3339(),
            forecast_cost_usd: None,
            forecast_rounds: None,
            forecast_confidence: None,
            team_id: None,
            session_id: None,
            model_switches: None,
            ticket_id: None,
            identity_id: None,
            description: None,
        };

        let rounds = vec![
            RoundTelemetryRecord {
                round_number: 1,
                input_tokens: 2000,
                output_tokens: 800,
                latency_ms: 1500,
                cache_creation_tokens: 500,
                cache_read_tokens: 200,
                prompt_size_bytes: 4000,
                response_size_bytes: 1600,
                tool_count: 1,
                stop_reason: "tool_use".to_string(),
            },
            RoundTelemetryRecord {
                round_number: 2,
                input_tokens: 3000,
                output_tokens: 1200,
                latency_ms: 2000,
                cache_creation_tokens: 500,
                cache_read_tokens: 300,
                prompt_size_bytes: 6000,
                response_size_bytes: 2400,
                tool_count: 1,
                stop_reason: "end_turn".to_string(),
            },
        ];

        let tools = vec![ToolTelemetryRecord {
            round_number: 1,
            tool_name: "filesystem.read".to_string(),
            success: true,
            duration_ms: 50,
            policy_decision: "allow".to_string(),
            input_size_bytes: 100,
            output_size_bytes: 500,
            denial_reason: None,
        }];

        let telemetry_id = store.record_task_telemetry(&task, &rounds, &tools).unwrap();
        assert!(telemetry_id > 0);

        // Query usage summary
        let from = Utc::now() - chrono::Duration::hours(1);
        let to = Utc::now() + chrono::Duration::hours(1);
        let summary = store
            .get_agent_usage_summary("agent-1", from, to, "model")
            .unwrap();
        assert_eq!(summary.len(), 1);
        assert_eq!(summary[0].group_key, "claude-sonnet-4-20250514");
        assert_eq!(summary[0].total_tasks, 1);

        // Query task history
        let history = store.get_task_history("agent-1", 10, None, None).unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].rounds, 3);
        assert!(history[0].success);

        // Compare models
        let comparison = store.compare_models_for_agent("agent-1", from, to).unwrap();
        assert_eq!(comparison.len(), 1);
        assert!((comparison[0].success_rate - 1.0).abs() < 0.01);
    }

    #[test]
    fn agent_model_preference_crud() {
        let store = SqliteStore::in_memory().unwrap();

        let pref = AgentModelPreference {
            agent_id: "agent-1".to_string(),
            task_type: "coding".to_string(),
            preferred_model: "claude-opus-4-20250514".to_string(),
            reason: Some("Better at complex code tasks".to_string()),
            updated_at: Utc::now().to_rfc3339(),
        };

        store.upsert_agent_model_preference(&pref).unwrap();

        let retrieved = store
            .get_agent_model_preference("agent-1", "coding")
            .unwrap()
            .unwrap();
        assert_eq!(retrieved.preferred_model, "claude-opus-4-20250514");

        // Update preference
        let updated_pref = AgentModelPreference {
            preferred_model: "claude-sonnet-4-20250514".to_string(),
            reason: Some("Good enough and cheaper".to_string()),
            updated_at: Utc::now().to_rfc3339(),
            ..pref
        };
        store.upsert_agent_model_preference(&updated_pref).unwrap();

        let retrieved = store
            .get_agent_model_preference("agent-1", "coding")
            .unwrap()
            .unwrap();
        assert_eq!(retrieved.preferred_model, "claude-sonnet-4-20250514");

        // Non-existent preference
        assert!(store
            .get_agent_model_preference("agent-1", "writing")
            .unwrap()
            .is_none());
    }

    #[test]
    fn model_health_event_recording() {
        let store = SqliteStore::in_memory().unwrap();
        store
            .record_model_health_event(
                "claude-opus-4-20250514",
                "rate_limit",
                Some("429 Too Many Requests"),
                Some("agent-1"),
            )
            .unwrap();
    }

    #[test]
    fn efficiency_trends() {
        let store = SqliteStore::in_memory().unwrap();

        for i in 0..3 {
            let task = TaskTelemetryRecord {
                agent_id: "agent-1".to_string(),
                model: "claude-sonnet-4-20250514".to_string(),
                provider: "anthropic".to_string(),
                input_tokens: 1000 * (i + 1),
                output_tokens: 500 * (i + 1),
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 0,
                cost_usd: 0.01 * (i + 1) as f64,
                rounds: 2,
                duration_ms: 3000,
                tool_invocation_count: 1,
                success: true,
                error_type: None,
                retry_count: 0,
                task_type: Some("coding".to_string()),
                task_id: Some(format!("task-{}", i)),
                started_at: Utc::now().to_rfc3339(),
                completed_at: Utc::now().to_rfc3339(),
                forecast_cost_usd: None,
                forecast_rounds: None,
                forecast_confidence: None,
                team_id: None,
                session_id: None,
                model_switches: None,
                ticket_id: None,
                identity_id: None,
                description: None,
            };
            store.record_task_telemetry(&task, &[], &[]).unwrap();
        }

        let from = Utc::now() - chrono::Duration::hours(1);
        let to = Utc::now() + chrono::Duration::hours(1);
        let trends = store.get_efficiency_trends("agent-1", from, to).unwrap();
        assert_eq!(trends.len(), 1); // All same day
        assert_eq!(trends[0].total_tasks, 3);
    }

    #[test]
    fn task_history_filters() {
        let store = SqliteStore::in_memory().unwrap();

        // Record tasks with different models and types
        for (model, task_type) in [
            ("claude-sonnet-4-20250514", "coding"),
            ("claude-opus-4-20250514", "coding"),
            ("claude-sonnet-4-20250514", "writing"),
        ] {
            let task = TaskTelemetryRecord {
                agent_id: "agent-1".to_string(),
                model: model.to_string(),
                provider: "anthropic".to_string(),
                input_tokens: 1000,
                output_tokens: 500,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 0,
                cost_usd: 0.01,
                rounds: 1,
                duration_ms: 1000,
                tool_invocation_count: 0,
                success: true,
                error_type: None,
                retry_count: 0,
                task_type: Some(task_type.to_string()),
                task_id: None,
                started_at: Utc::now().to_rfc3339(),
                completed_at: Utc::now().to_rfc3339(),
                forecast_cost_usd: None,
                forecast_rounds: None,
                forecast_confidence: None,
                team_id: None,
                session_id: None,
                model_switches: None,
                ticket_id: None,
                identity_id: None,
                description: None,
            };
            store.record_task_telemetry(&task, &[], &[]).unwrap();
        }

        // All tasks
        assert_eq!(
            store
                .get_task_history("agent-1", 10, None, None)
                .unwrap()
                .len(),
            3
        );
        // Filter by task_type
        assert_eq!(
            store
                .get_task_history("agent-1", 10, Some("coding"), None)
                .unwrap()
                .len(),
            2
        );
        // Filter by model
        assert_eq!(
            store
                .get_task_history("agent-1", 10, None, Some("claude-opus-4-20250514"))
                .unwrap()
                .len(),
            1
        );
        // Filter by both
        assert_eq!(
            store
                .get_task_history(
                    "agent-1",
                    10,
                    Some("coding"),
                    Some("claude-sonnet-4-20250514")
                )
                .unwrap()
                .len(),
            1
        );
        // Limit
        assert_eq!(
            store
                .get_task_history("agent-1", 1, None, None)
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn policy_proposal_crud() {
        let store = SqliteStore::in_memory().unwrap();

        // Create proposals
        let id1 = store
            .create_policy_proposal(
                "agent-1",
                r#"{"name":"allow-git","scope":"global","tool":"git.*","action":"allow"}"#,
                "Agents need git access for code tasks",
            )
            .unwrap();
        let id2 = store
            .create_policy_proposal(
                "agent-2",
                r#"{"name":"allow-exec","scope":"global","tool":"exec.*","action":"allow"}"#,
                "Need to run tests",
            )
            .unwrap();
        assert!(id1 > 0);
        assert!(id2 > id1);

        // List all
        let all = store.list_policy_proposals(None, 100).unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].id, id2); // Most recent first

        // List by status
        let pending = store.list_policy_proposals(Some("pending"), 100).unwrap();
        assert_eq!(pending.len(), 2);
        let approved = store.list_policy_proposals(Some("approved"), 100).unwrap();
        assert_eq!(approved.len(), 0);

        // Get by ID
        let proposal = store.get_policy_proposal(id1).unwrap().unwrap();
        assert_eq!(proposal.agent_id, "agent-1");
        assert_eq!(proposal.status, "pending");
        assert!(proposal.reviewed_by.is_none());

        // Non-existent
        assert!(store.get_policy_proposal(999).unwrap().is_none());

        // Approve
        store
            .review_policy_proposal(id1, "approved", "admin", Some("Looks good"))
            .unwrap();
        let approved = store.get_policy_proposal(id1).unwrap().unwrap();
        assert_eq!(approved.status, "approved");
        assert_eq!(approved.reviewed_by, Some("admin".to_string()));
        assert_eq!(approved.review_note, Some("Looks good".to_string()));
        assert!(approved.reviewed_at.is_some());

        // Reject
        store
            .review_policy_proposal(id2, "rejected", "admin", None)
            .unwrap();
        let rejected = store.get_policy_proposal(id2).unwrap().unwrap();
        assert_eq!(rejected.status, "rejected");

        // List filtered after review
        let pending = store.list_policy_proposals(Some("pending"), 100).unwrap();
        assert_eq!(pending.len(), 0);
        let approved = store.list_policy_proposals(Some("approved"), 100).unwrap();
        assert_eq!(approved.len(), 1);
    }

    #[test]
    fn listing_by_workflow_id_matches_the_bare_id_and_owned_copies_only() {
        let store = SqliteStore::in_memory().unwrap();
        let now = Utc::now().to_rfc3339();
        let row = |id: &str, workflow_id: &str| WorkflowInstanceRow {
            id: id.to_string(),
            workflow_id: workflow_id.to_string(),
            status: "completed".to_string(),
            current_step_id: None,
            step_history: "[]".to_string(),
            variables: "{}".to_string(),
            trigger_json: "{}".to_string(),
            agent_id: None,
            created_at: now.clone(),
            updated_at: now.clone(),
            parent_instance_id: None,
            channel_id: None,
            thread_id: None,
            ..Default::default()
        };
        for (id, workflow) in [
            ("a", "codex-smoke"),
            ("b", "69116b82/codex-smoke"),
            ("c", "enable-users"),
            ("d", "smoke"),
            ("e", "69116b82/codex_smoke"),
        ] {
            store.save_workflow_instance(&row(id, workflow)).unwrap();
        }
        let mut ids: Vec<String> = store
            .list_workflow_instances(None, Some("codex-smoke"))
            .unwrap()
            .into_iter()
            .map(|r| r.id)
            .collect();
        ids.sort();
        assert_eq!(ids, vec!["a", "b"]);
        assert!(store
            .list_workflow_instances(None, Some("smoke"))
            .unwrap()
            .iter()
            .all(|r| r.id == "d"));
    }

    #[test]
    fn identity_billing_follows_a_merged_person() {
        crate::telemetry::conformance::identity_billing_follows_a_merged_person(
            &SqliteStore::in_memory().unwrap(),
        );
    }

    #[test]
    fn identity_signals_count_exactly_what_a_move_and_an_erase_change() {
        crate::telemetry::conformance::identity_signals_count_exactly_what_a_move_and_an_erase_change(
            &SqliteStore::in_memory().unwrap(),
        );
    }

    #[test]
    fn activity_is_pruned_by_completion() {
        let store = SqliteStore::in_memory().unwrap();
        crate::telemetry::conformance::activity_is_pruned_by_completion(&store);
        let children: (i64, i64) = store
            .conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT (SELECT COUNT(*) FROM round_telemetry), (SELECT COUNT(*) FROM tool_telemetry)",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            children,
            (1, 1),
            "a pruned task's rounds and tool calls go with it"
        );
    }

    #[test]
    fn costs_are_pruned_but_a_jobs_are_kept() {
        crate::telemetry::conformance::costs_are_pruned_but_a_jobs_are_kept(
            &SqliteStore::in_memory().unwrap(),
        );
    }

    #[test]
    fn a_prune_runs_batch_after_batch_until_none_is_left() {
        let store = SqliteStore::in_memory().unwrap();
        let old = Utc::now() - chrono::Duration::days(500);
        let total = crate::telemetry::PRUNE_BATCH + 7;
        {
            let mut conn = store.conn.lock().unwrap();
            let tx = conn.transaction().unwrap();
            for _ in 0..total {
                tx.execute(
                    "INSERT INTO cost_records (timestamp, agent_id, model, input_tokens, output_tokens, cost_usd)
                     VALUES (?1, 'agent', 'm', 0, 0, 0.01)",
                    params![old.to_rfc3339()],
                )
                .unwrap();
            }
            tx.commit().unwrap();
        }
        for i in 0..total {
            crate::telemetry::conformance::record_task(&store, &format!("t{i}"), old);
        }
        let cutoff = Utc::now() - chrono::Duration::days(400);
        assert_eq!(store.prune_costs(cutoff).unwrap(), total);
        assert_eq!(store.prune_activity(cutoff).unwrap(), total);
        assert_eq!(store.prune_activity(cutoff).unwrap(), 0);
    }

    #[test]
    fn a_fenced_save_applies_only_at_the_version_it_read() {
        crate::telemetry::conformance::a_fenced_save_applies_only_at_the_version_it_read(
            &SqliteStore::in_memory().unwrap(),
        );
    }

    #[test]
    fn every_content_write_moves_the_version_and_the_lease_does_not() {
        crate::telemetry::conformance::every_content_write_moves_the_version_and_the_lease_does_not(
            &SqliteStore::in_memory().unwrap(),
        );
    }

    #[test]
    fn a_fenced_save_after_a_targeted_cancel_is_refused() {
        crate::telemetry::conformance::a_fenced_save_after_a_targeted_cancel_is_refused(
            &SqliteStore::in_memory().unwrap(),
        );
    }

    #[test]
    fn workflow_instances_are_deleted() {
        crate::telemetry::conformance::instances_are_deleted(&SqliteStore::in_memory().unwrap());
    }

    #[test]
    fn instances_are_walked_in_pages_oldest_first() {
        crate::telemetry::conformance::instances_walked_in_pages(
            &SqliteStore::in_memory().unwrap(),
        );
    }

    #[test]
    fn filtered_instance_listing_honours_every_constraint() {
        crate::telemetry::conformance::filtered_instance_listing(
            &SqliteStore::in_memory().unwrap(),
        );
    }

    #[test]
    fn get_workflow_instances_returns_the_named_rows_only() {
        let store = SqliteStore::in_memory().unwrap();
        let now = Utc::now().to_rfc3339();
        for id in ["run-a", "run-b", "run-c"] {
            store
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
        let mut found = store.get_workflow_instances(&named).unwrap();
        found.sort_by(|a, b| a.id.cmp(&b.id));
        assert_eq!(
            found.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            ["run-a", "run-c"]
        );
        assert_eq!(found[0].agent_id.as_deref(), Some("atlas"));
        assert_eq!(found[0].step_history, r#"[{"step_id":"implement"}]"#);
        assert!(store.get_workflow_instances(&[]).unwrap().is_empty());
    }

    #[test]
    fn workflow_instance_crud() {
        let store = SqliteStore::in_memory().unwrap();
        let now = Utc::now().to_rfc3339();

        let instance = WorkflowInstanceRow {
            id: "wf-inst-1".to_string(),
            workflow_id: "bug-fix".to_string(),
            status: "running".to_string(),
            current_step_id: Some("investigate".to_string()),
            step_history: "[]".to_string(),
            variables: r#"{"ticket_id":"DW-100"}"#.to_string(),
            trigger_json: r#"{"source":{"type":"cli"},"payload":{}}"#.to_string(),
            agent_id: Some("allan".to_string()),
            created_at: now.clone(),
            updated_at: now.clone(),
            parent_instance_id: None,
            channel_id: Some("C123".to_string()),
            thread_id: Some("1234567890.123456".to_string()),
            ..Default::default()
        };

        store.save_workflow_instance(&instance).unwrap();

        // Get by ID
        let retrieved = store.get_workflow_instance("wf-inst-1").unwrap().unwrap();
        assert_eq!(retrieved.workflow_id, "bug-fix");
        assert_eq!(retrieved.status, "running");
        assert_eq!(retrieved.current_step_id, Some("investigate".to_string()));
        assert_eq!(retrieved.agent_id, Some("allan".to_string()));

        // Non-existent
        assert!(store
            .get_workflow_instance("nonexistent")
            .unwrap()
            .is_none());

        // List all
        let all = store.list_workflow_instances(None, None).unwrap();
        assert_eq!(all.len(), 1);

        // List by status
        let running = store
            .list_workflow_instances(Some("running"), None)
            .unwrap();
        assert_eq!(running.len(), 1);
        let paused = store.list_workflow_instances(Some("paused"), None).unwrap();
        assert_eq!(paused.len(), 0);

        // List by workflow_id
        let bug_fix = store
            .list_workflow_instances(None, Some("bug-fix"))
            .unwrap();
        assert_eq!(bug_fix.len(), 1);
        let other = store.list_workflow_instances(None, Some("other")).unwrap();
        assert_eq!(other.len(), 0);

        // Update status
        store
            .update_workflow_instance_status("wf-inst-1", "paused", Some("open-pr"))
            .unwrap();
        let updated = store.get_workflow_instance("wf-inst-1").unwrap().unwrap();
        assert_eq!(updated.status, "paused");
        assert_eq!(updated.current_step_id, Some("open-pr".to_string()));

        // Save with updated step_history (upsert)
        let mut updated_row = retrieved;
        updated_row.step_history = r#"[{"step_id":"investigate","outcome":null}]"#.to_string();
        updated_row.status = "paused".to_string();
        store.save_workflow_instance(&updated_row).unwrap();
        let final_row = store.get_workflow_instance("wf-inst-1").unwrap().unwrap();
        assert!(final_row.step_history.contains("investigate"));
    }

    #[test]
    fn workflow_execution_lease_semantics() {
        let store = SqliteStore::in_memory().unwrap();
        let now = Utc::now().to_rfc3339();
        let row = WorkflowInstanceRow {
            id: "wf-lease-1".to_string(),
            workflow_id: "bug-fix".to_string(),
            status: "running".to_string(),
            current_step_id: None,
            step_history: "[]".to_string(),
            variables: "{}".to_string(),
            trigger_json: "{}".to_string(),
            agent_id: None,
            created_at: now.clone(),
            updated_at: now.clone(),
            parent_instance_id: None,
            channel_id: None,
            thread_id: None,
            ..Default::default()
        };
        store.save_workflow_instance(&row).unwrap();

        // First claim wins; a different owner is refused; re-claim by the
        // holder is idempotent.
        assert!(store
            .try_claim_workflow_instance("wf-lease-1", "hub-a", 120)
            .unwrap());
        assert!(!store
            .try_claim_workflow_instance("wf-lease-1", "hub-b", 120)
            .unwrap());
        assert!(store
            .try_claim_workflow_instance("wf-lease-1", "hub-a", 120)
            .unwrap());

        // Renewal only works for the holder.
        assert!(store
            .renew_workflow_lease("wf-lease-1", "hub-a", 120)
            .unwrap());
        assert!(!store
            .renew_workflow_lease("wf-lease-1", "hub-b", 120)
            .unwrap());

        // Fenced save applies for the holder, is refused for anyone else.
        let mut update = row.clone();
        update.status = "running".to_string();
        update.step_history = r#"[{"step_id":"s1"}]"#.to_string();
        assert!(store
            .save_workflow_instance_owned(&update, "hub-a")
            .unwrap());
        assert!(!store
            .save_workflow_instance_owned(&update, "hub-b")
            .unwrap());

        // An expired lease is claimable by a new owner (steal).
        {
            let conn = store.conn.lock().unwrap();
            conn.execute(
                "UPDATE workflow_instances
                 SET lease_expires_at = datetime('now', '-60 seconds')
                 WHERE id = 'wf-lease-1'",
                [],
            )
            .unwrap();
        }
        assert!(store
            .try_claim_workflow_instance("wf-lease-1", "hub-b", 120)
            .unwrap());
        assert!(!store
            .save_workflow_instance_owned(&update, "hub-a")
            .unwrap());

        // Release clears ownership only for the holder; then anyone claims.
        store
            .release_workflow_instance("wf-lease-1", "hub-a")
            .unwrap();
        assert!(!store
            .try_claim_workflow_instance("wf-lease-1", "hub-a", 120)
            .unwrap());
        store
            .release_workflow_instance("wf-lease-1", "hub-b")
            .unwrap();
        assert!(store
            .try_claim_workflow_instance("wf-lease-1", "hub-a", 120)
            .unwrap());

        // Targeted cancel flips status + records the reason without touching
        // step_history — and works regardless of the lease.
        store
            .cancel_workflow_instance("wf-lease-1", "cancelled", "_cancellation_reason", "user")
            .unwrap();
        let after = store.get_workflow_instance("wf-lease-1").unwrap().unwrap();
        assert_eq!(after.status, "cancelled");
        assert!(after.variables.contains("_cancellation_reason"));
        assert!(after.step_history.contains("s1"));
    }

    #[test]
    fn model_pricing_crud() {
        let store = SqliteStore::in_memory().unwrap();
        let now = Utc::now().to_rfc3339();

        // Insert
        let pricing = StoredModelPricing {
            model: "claude-sonnet-4-20250514".to_string(),
            provider: "anthropic".to_string(),
            input_per_million: 3.0,
            output_per_million: 15.0,
            source: "default".to_string(),
            fetched_at: now.clone(),
            updated_at: now.clone(),
        };
        store.upsert_model_pricing(&pricing).unwrap();

        // Get
        let retrieved = store
            .get_model_pricing("claude-sonnet-4-20250514")
            .unwrap()
            .unwrap();
        assert_eq!(retrieved.provider, "anthropic");
        assert!((retrieved.input_per_million - 3.0).abs() < 0.001);
        assert!((retrieved.output_per_million - 15.0).abs() < 0.001);
        assert_eq!(retrieved.source, "default");

        // Non-existent
        assert!(store.get_model_pricing("nonexistent").unwrap().is_none());

        // Upsert (update)
        let updated = StoredModelPricing {
            input_per_million: 4.0,
            source: "manual".to_string(),
            ..pricing.clone()
        };
        store.upsert_model_pricing(&updated).unwrap();
        let retrieved = store
            .get_model_pricing("claude-sonnet-4-20250514")
            .unwrap()
            .unwrap();
        assert!((retrieved.input_per_million - 4.0).abs() < 0.001);
        assert_eq!(retrieved.source, "manual");

        // Insert a second entry
        store
            .upsert_model_pricing(&StoredModelPricing {
                model: "grok-4".to_string(),
                provider: "grok".to_string(),
                input_per_million: 3.0,
                output_per_million: 15.0,
                source: "api_fetch".to_string(),
                fetched_at: now.clone(),
                updated_at: now,
            })
            .unwrap();

        // List
        let all = store.list_model_pricing().unwrap();
        assert_eq!(all.len(), 2);

        // Delete
        store
            .delete_model_pricing("claude-sonnet-4-20250514")
            .unwrap();
        assert!(store
            .get_model_pricing("claude-sonnet-4-20250514")
            .unwrap()
            .is_none());
        assert_eq!(store.list_model_pricing().unwrap().len(), 1);
    }

    #[test]
    fn test_ticket_cost_aggregation() {
        let store = SqliteStore::in_memory().unwrap();

        // Insert telemetry records for the same ticket
        for i in 0..3 {
            let task = TaskTelemetryRecord {
                agent_id: "agent-1".to_string(),
                model: "claude-sonnet-4-20250514".to_string(),
                provider: "anthropic".to_string(),
                input_tokens: 1000 * (i + 1),
                output_tokens: 500 * (i + 1),
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 0,
                cost_usd: 0.01 * (i + 1) as f64,
                rounds: 2,
                duration_ms: 3000,
                tool_invocation_count: 1,
                success: true,
                error_type: None,
                retry_count: 0,
                task_type: Some("coding".to_string()),
                task_id: Some(format!("task-{}", i)),
                started_at: Utc::now().to_rfc3339(),
                completed_at: Utc::now().to_rfc3339(),
                forecast_cost_usd: None,
                forecast_rounds: None,
                forecast_confidence: None,
                team_id: None,
                session_id: None,
                model_switches: None,
                ticket_id: Some("DW-400".to_string()),
                identity_id: None,
                description: None,
            };
            store.record_task_telemetry(&task, &[], &[]).unwrap();
        }

        // Insert a record with no ticket_id
        let unrelated = TaskTelemetryRecord {
            agent_id: "agent-1".to_string(),
            model: "claude-sonnet-4-20250514".to_string(),
            provider: "anthropic".to_string(),
            input_tokens: 9999,
            output_tokens: 9999,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
            cost_usd: 99.0,
            rounds: 10,
            duration_ms: 50000,
            tool_invocation_count: 0,
            success: true,
            error_type: None,
            retry_count: 0,
            task_type: Some("coding".to_string()),
            task_id: None,
            started_at: Utc::now().to_rfc3339(),
            completed_at: Utc::now().to_rfc3339(),
            forecast_cost_usd: None,
            forecast_rounds: None,
            forecast_confidence: None,
            team_id: None,
            session_id: None,
            model_switches: None,
            ticket_id: None,
            identity_id: None,
            description: None,
        };
        store.record_task_telemetry(&unrelated, &[], &[]).unwrap();

        // Query by ticket
        let summary = store.cost_by_ticket("DW-400").unwrap();
        assert_eq!(summary.ticket_id, "DW-400");
        assert_eq!(summary.step_count, 3);
        assert!((summary.total_cost_usd - 0.06).abs() < 0.001);
        assert_eq!(summary.total_input_tokens, 6000); // 1000+2000+3000
        assert_eq!(summary.total_output_tokens, 3000); // 500+1000+1500
        assert_eq!(summary.total_rounds, 6); // 2*3
        assert!(!summary.models_used.is_empty());

        // Non-existent ticket returns zero-value summary
        let empty = store.cost_by_ticket("DW-999").unwrap();
        assert_eq!(empty.step_count, 0);
        assert!((empty.total_cost_usd - 0.0).abs() < 0.001);
    }

    #[test]
    fn test_cost_by_tickets_bulk() {
        let store = SqliteStore::in_memory().unwrap();

        // Insert records for two different tickets
        for (ticket, cost) in [("DW-100", 0.05), ("DW-200", 0.10)] {
            let task = TaskTelemetryRecord {
                agent_id: "agent-1".to_string(),
                model: "claude-sonnet-4-20250514".to_string(),
                provider: "anthropic".to_string(),
                input_tokens: 1000,
                output_tokens: 500,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 0,
                cost_usd: cost,
                rounds: 2,
                duration_ms: 3000,
                tool_invocation_count: 0,
                success: true,
                error_type: None,
                retry_count: 0,
                task_type: Some("coding".to_string()),
                task_id: None,
                started_at: Utc::now().to_rfc3339(),
                completed_at: Utc::now().to_rfc3339(),
                forecast_cost_usd: None,
                forecast_rounds: None,
                forecast_confidence: None,
                team_id: None,
                session_id: None,
                model_switches: None,
                ticket_id: Some(ticket.to_string()),
                identity_id: None,
                description: None,
            };
            store.record_task_telemetry(&task, &[], &[]).unwrap();
        }

        let from = Utc::now() - chrono::Duration::hours(1);
        let to = Utc::now() + chrono::Duration::hours(1);
        let results = store.cost_by_tickets(from, to).unwrap();
        assert_eq!(results.len(), 2);
        // Ordered by cost DESC
        assert_eq!(results[0].ticket_id, "DW-200");
        assert_eq!(results[1].ticket_id, "DW-100");
    }

    fn cost_record_for_identity(identity: Option<&str>, cost: f64) -> CostRecord {
        CostRecord {
            agent_id: "agent-1".into(),
            model: "claude-sonnet".into(),
            input_tokens: 1000,
            output_tokens: 500,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
            cost_usd: cost,
            team_id: None,
            task_id: None,
            task_type: None,
            ticket_id: None,
            identity_id: identity.map(String::from),
            description: None,
            ..Default::default()
        }
    }

    #[test]
    fn cost_by_identity_groups_unknown_for_missing_id() {
        let store = SqliteStore::in_memory().unwrap();
        store
            .record_cost(&cost_record_for_identity(Some("alice"), 0.10))
            .unwrap();
        store
            .record_cost(&cost_record_for_identity(Some("bob"), 0.05))
            .unwrap();
        store
            .record_cost(&cost_record_for_identity(None, 0.03))
            .unwrap();
        store
            .record_cost(&cost_record_for_identity(Some("alice"), 0.02))
            .unwrap();

        let from = Utc::now() - chrono::Duration::hours(1);
        let to = Utc::now() + chrono::Duration::hours(1);
        let by_id = store.cost_by_identity(from, to).unwrap();
        // Order: alice (0.12), bob (0.05), unknown (0.03)
        assert_eq!(by_id.len(), 3);
        assert_eq!(by_id[0].0, "alice");
        assert!((by_id[0].1 - 0.12).abs() < 1e-6);
        assert_eq!(by_id[1].0, "bob");
        assert_eq!(by_id[2].0, "unknown");
    }

    fn task_with_identity(identity: Option<&str>, agent: &str, model: &str) -> TaskTelemetryRecord {
        TaskTelemetryRecord {
            agent_id: agent.into(),
            model: model.into(),
            provider: "anthropic".into(),
            input_tokens: 1000,
            output_tokens: 500,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
            cost_usd: 0.05,
            rounds: 2,
            duration_ms: 1500,
            tool_invocation_count: 1,
            success: true,
            error_type: None,
            retry_count: 0,
            task_type: Some("coding".into()),
            task_id: Some(format!("{agent}-{model}")),
            started_at: Utc::now().to_rfc3339(),
            completed_at: Utc::now().to_rfc3339(),
            forecast_cost_usd: None,
            forecast_rounds: None,
            forecast_confidence: None,
            team_id: None,
            session_id: None,
            model_switches: None,
            ticket_id: None,
            identity_id: identity.map(String::from),
            description: None,
        }
    }

    #[test]
    fn agent_tool_turns_lists_each_turns_distinct_tools_oldest_first() {
        let store = SqliteStore::in_memory().unwrap();
        let tool = |name: &str| ToolTelemetryRecord {
            round_number: 1,
            tool_name: name.to_string(),
            success: true,
            duration_ms: 1,
            policy_decision: "allow".to_string(),
            input_size_bytes: 0,
            output_size_bytes: 0,
            denial_reason: None,
        };
        let at =
            |minutes_ago: i64| (Utc::now() - chrono::Duration::minutes(minutes_ago)).to_rfc3339();
        let turn = |agent: &str, task_type: &str, started: i64| TaskTelemetryRecord {
            task_type: Some(task_type.into()),
            started_at: at(started),
            completed_at: at(started - 1),
            ..task_with_identity(None, agent, "claude-sonnet")
        };
        store
            .record_task_telemetry(
                &turn("a", "chat", 30),
                &[],
                &[tool("memory"), tool("memory"), tool("vault.set")],
            )
            .unwrap();
        store
            .record_task_telemetry(&turn("a", "chat", 20), &[], &[])
            .unwrap();
        store
            .record_task_telemetry(&turn("a", "job:report", 10), &[], &[tool("weekly report")])
            .unwrap();
        store
            .record_task_telemetry(&turn("b", "chat", 5), &[], &[tool("memory")])
            .unwrap();
        store
            .record_task_telemetry(&turn("a", "chat", 60 * 24 * 30), &[], &[tool("old")])
            .unwrap();

        let turns = store
            .agent_tool_turns("a", Utc::now() - chrono::Duration::days(14), 100)
            .unwrap();
        let tools: Vec<std::collections::BTreeSet<String>> = turns
            .iter()
            .map(|t| t.tools.iter().cloned().collect())
            .collect();
        assert_eq!(
            tools,
            [
                std::collections::BTreeSet::from(["memory".to_string(), "vault.set".to_string()]),
                std::collections::BTreeSet::new(),
            ]
        );
        assert!(turns[0].started_at < turns[1].started_at);
    }

    #[test]
    fn task_history_paginated_uses_cursor() {
        let store = SqliteStore::in_memory().unwrap();
        for i in 0..5 {
            let mut t = task_with_identity(Some("alice"), "agent-1", "claude-sonnet");
            t.task_id = Some(format!("task-{i}"));
            // Spread completed_at so cursor ordering is deterministic.
            t.completed_at = (Utc::now() - chrono::Duration::seconds(i)).to_rfc3339();
            store.record_task_telemetry(&t, &[], &[]).unwrap();
        }

        let mut filter = TaskHistoryFilter {
            identity_id: Some("alice".into()),
            limit: 2,
            ..Default::default()
        };
        let page1 = store.task_history_paginated(&filter).unwrap();
        assert_eq!(page1.entries.len(), 2);
        assert!(page1.next_cursor.is_some());

        // Decode cursor and fetch the next page.
        let (cursor_ts, cursor_id) =
            crate::pagination::decode_cursor(page1.next_cursor.as_ref().unwrap()).unwrap();
        filter.cursor = Some((cursor_ts, cursor_id));
        let page2 = store.task_history_paginated(&filter).unwrap();
        assert_eq!(page2.entries.len(), 2);
        assert!(page2.next_cursor.is_some());

        let (cursor_ts, cursor_id) =
            crate::pagination::decode_cursor(page2.next_cursor.as_ref().unwrap()).unwrap();
        filter.cursor = Some((cursor_ts, cursor_id));
        let page3 = store.task_history_paginated(&filter).unwrap();
        // Last page: 1 entry, no cursor.
        assert_eq!(page3.entries.len(), 1);
        assert!(page3.next_cursor.is_none());
    }

    #[test]
    fn migration_backfills_subscription_billing_excluding_bedrock() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("telemetry.db");
        let legacy = [
            "claude-code:sonnet",
            "claude-code:bedrock:anthropic.claude-sonnet-5",
            "codex:gpt-5.6-sol",
            "codex_api:gpt-5.6-sol",
            "claude-pilot:sonnet",
            "gemini-3.1-pro-preview",
        ];
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE cost_records (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    timestamp TEXT NOT NULL,
                    agent_id TEXT NOT NULL,
                    model TEXT NOT NULL,
                    input_tokens INTEGER NOT NULL,
                    output_tokens INTEGER NOT NULL,
                    cost_usd REAL NOT NULL,
                    team_id TEXT,
                    task_id TEXT,
                    task_type TEXT
                );",
            )
            .unwrap();
            for model in legacy {
                conn.execute(
                    "INSERT INTO cost_records (timestamp, agent_id, model, input_tokens, output_tokens, cost_usd)
                     VALUES ('2026-09-01T00:00:00Z', 'allan', ?1, 1, 1, 0.1)",
                    params![model],
                )
                .unwrap();
            }
        }

        let store = SqliteStore::open(&path).unwrap();
        store
            .record_cost(&CostRecord {
                agent_id: "allan".into(),
                model: "codex:gpt-6.1-sol".into(),
                billing: Billing::Subscription,
                credential_tier: Some(CredentialTier::AgentShared),
                ..Default::default()
            })
            .unwrap();
        drop(store);
        let reopened = SqliteStore::open(&path).unwrap();

        let billing: Vec<(String, String)> = {
            let conn = reopened.conn.lock().unwrap();
            let mut stmt = conn
                .prepare("SELECT model, billing FROM cost_records ORDER BY id")
                .unwrap();
            stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap()
        };
        let expected = [
            ("claude-code:sonnet", "subscription"),
            ("claude-code:bedrock:anthropic.claude-sonnet-5", "metered"),
            ("codex:gpt-5.6-sol", "subscription"),
            ("codex_api:gpt-5.6-sol", "metered"),
            ("claude-pilot:sonnet", "subscription"),
            ("gemini-3.1-pro-preview", "metered"),
            ("codex:gpt-6.1-sol", "subscription"),
        ]
        .map(|(model, billing)| (model.to_string(), billing.to_string()));
        assert_eq!(billing, expected);
    }

    #[test]
    fn get_task_trace_joins_rounds_tools_and_costs() {
        let store = SqliteStore::in_memory().unwrap();
        let mut task = task_with_identity(Some("alice"), "agent-1", "claude-sonnet");
        task.task_id = Some("task-trace".into());
        let pk = store
            .record_task_telemetry(
                &task,
                &[RoundTelemetryRecord {
                    round_number: 1,
                    input_tokens: 1000,
                    output_tokens: 500,
                    latency_ms: 1500,
                    cache_creation_tokens: 0,
                    cache_read_tokens: 0,
                    prompt_size_bytes: 200,
                    response_size_bytes: 100,
                    tool_count: 1,
                    stop_reason: "end_turn".into(),
                }],
                &[ToolTelemetryRecord {
                    round_number: 1,
                    tool_name: "filesystem.read".into(),
                    success: true,
                    duration_ms: 50,
                    policy_decision: "allow".into(),
                    input_size_bytes: 50,
                    output_size_bytes: 100,
                    denial_reason: None,
                }],
            )
            .unwrap();
        assert!(pk > 0);
        store
            .record_cost(&CostRecord {
                agent_id: "agent-1".into(),
                model: "claude-sonnet".into(),
                input_tokens: 1000,
                output_tokens: 500,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 0,
                cost_usd: 0.05,
                team_id: None,
                task_id: Some("task-trace".into()),
                task_type: Some("coding".into()),
                ticket_id: None,
                identity_id: Some("alice".into()),
                description: None,
                ..Default::default()
            })
            .unwrap();

        let trace = store.get_task_trace("task-trace").unwrap().unwrap();
        assert_eq!(trace.task.identity_id.as_deref(), Some("alice"));
        assert_eq!(trace.rounds.len(), 1);
        assert_eq!(trace.tools.len(), 1);
        assert_eq!(trace.cost_records.len(), 1);
        assert_eq!(trace.tools[0].tool_name, "filesystem.read");

        // Missing task_id returns None.
        assert!(store.get_task_trace("nonexistent").unwrap().is_none());
    }

    // ─── cost_detail_records / scheduled_job_history ──────────────────────────

    fn cost_record(
        agent_id: &str,
        identity_id: Option<&str>,
        cost_usd: f64,
        description: Option<&str>,
    ) -> CostRecord {
        CostRecord {
            agent_id: agent_id.into(),
            model: "claude-sonnet-4-6".into(),
            input_tokens: 1_000,
            output_tokens: 500,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
            cost_usd,
            team_id: None,
            task_id: None,
            task_type: Some("chat".into()),
            ticket_id: None,
            identity_id: identity_id.map(String::from),
            description: description.map(String::from),
            ..Default::default()
        }
    }

    fn seed_cost_records(store: &SqliteStore) {
        // 4 records: 2 agents × 2 users
        store
            .record_cost(&cost_record(
                "agent-a",
                Some("alice"),
                0.10,
                Some("vision:gpt-4o"),
            ))
            .unwrap();
        store
            .record_cost(&cost_record(
                "agent-a",
                Some("bob"),
                0.20,
                Some("subagent prompt"),
            ))
            .unwrap();
        store
            .record_cost(&cost_record(
                "agent-b",
                Some("alice"),
                0.30,
                Some("transcription:42s"),
            ))
            .unwrap();
        store
            .record_cost(&cost_record("agent-b", None, 0.40, None))
            .unwrap();
    }

    #[test]
    fn cost_detail_records_filter_pagination_and_distinct_lists() {
        let store = SqliteStore::in_memory().unwrap();
        seed_cost_records(&store);

        let from = Utc::now() - chrono::Duration::hours(1);
        let to = Utc::now() + chrono::Duration::hours(1);

        // No filters: full set, distinct lists complete.
        let page = store
            .cost_detail_records(from, to, CostDetailFilter::default(), 100, 0)
            .unwrap();
        assert_eq!(page.total, 4);
        assert_eq!(page.records.len(), 4);
        assert_eq!(page.distinct_users, vec!["alice", "bob"]);
        assert_eq!(page.distinct_agents, vec!["agent-a", "agent-b"]);

        // Agent filter narrows page rows AND distinct_users (since users
        // shown should be those visible under the active agent).
        let page = store
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
        assert_eq!(page.distinct_users, vec!["alice", "bob"]);
        // distinct_agents is intentionally not narrowed by the agent filter
        // — the dropdown must remain switchable.
        assert_eq!(page.distinct_agents, vec!["agent-a", "agent-b"]);

        // User filter narrows rows but does NOT narrow distinct_users
        // — the dropdown must remain switchable.
        let page = store
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

        // Pagination: limit 2, offset 2 → second page of 4 → 2 rows.
        let page = store
            .cost_detail_records(from, to, CostDetailFilter::default(), 2, 2)
            .unwrap();
        assert_eq!(page.total, 4);
        assert_eq!(page.records.len(), 2);
    }

    /// A metered call and two subscription calls by Alice on agent-a, one
    /// on her own plan and one on the install's, plus a metered call on
    /// agent-b recorded before origins were. Costs sum exactly.
    fn seed_billed_cost_records(store: &SqliteStore) {
        let billed = |agent: &str,
                      cost: f64,
                      origin: Option<Origin>,
                      plan: Option<(&str, CredentialTier)>| CostRecord {
            origin,
            billing: plan.map_or(Billing::Metered, |_| Billing::Subscription),
            payer: plan.map(|(payer, _)| payer.to_string()),
            credential_tier: plan.map(|(_, tier)| tier),
            ..cost_record(agent, Some("alice"), cost, None)
        };
        for record in [
            billed("agent-a", 0.125, Some(Origin::Interactive), None),
            billed(
                "agent-a",
                0.25,
                Some(Origin::Interactive),
                Some(("alice", CredentialTier::GlobalUser)),
            ),
            billed(
                "agent-a",
                0.5,
                Some(Origin::Heartbeat),
                Some(("install", CredentialTier::SystemShared)),
            ),
            billed("agent-b", 1.0, None, None),
        ] {
            store.record_cost(&record).unwrap();
        }
    }

    fn window() -> (DateTime<Utc>, DateTime<Utc>) {
        (
            Utc::now() - chrono::Duration::hours(1),
            Utc::now() + chrono::Duration::hours(1),
        )
    }

    #[test]
    fn cost_split_groups_by_billing_origin_payer_tier() {
        let store = SqliteStore::in_memory().unwrap();
        seed_billed_cost_records(&store);
        store
            .record_cost(&CostRecord {
                origin: Some(Origin::Interactive),
                ..cost_record("agent-a", Some("bob"), 0.0625, None)
            })
            .unwrap();
        let (from, to) = window();

        let rows = store.cost_split(from, to, CostScope::default()).unwrap();
        let row = |agent: &str,
                   billing: Billing,
                   origin: Option<&str>,
                   payer: Option<&str>,
                   tier: Option<&str>,
                   calls: u64,
                   cost_usd: f64| CostSplitRow {
            agent_id: agent.into(),
            billing,
            origin: origin.map(String::from),
            payer: payer.map(String::from),
            credential_tier: tier.map(|tier| CredentialTier::from_column(tier).unwrap()),
            calls,
            cost_usd,
        };
        assert_eq!(
            rows,
            [
                row(
                    "agent-a",
                    Billing::Metered,
                    Some("interactive"),
                    None,
                    None,
                    2,
                    0.1875
                ),
                row(
                    "agent-a",
                    Billing::Subscription,
                    Some("heartbeat"),
                    Some("install"),
                    Some("system_shared"),
                    1,
                    0.5
                ),
                row(
                    "agent-a",
                    Billing::Subscription,
                    Some("interactive"),
                    Some("alice"),
                    Some("global_user"),
                    1,
                    0.25
                ),
                row("agent-b", Billing::Metered, None, None, None, 1, 1.0),
            ]
        );

        let scoped = store
            .cost_split(
                from,
                to,
                CostScope {
                    agent_id: Some("agent-a"),
                    user_id: Some("bob"),
                },
            )
            .unwrap();
        assert_eq!(
            scoped,
            [row(
                "agent-a",
                Billing::Metered,
                Some("interactive"),
                None,
                None,
                1,
                0.0625
            )]
        );
    }

    #[test]
    fn cost_detail_rows_carry_billing_columns_and_filter() {
        let store = SqliteStore::in_memory().unwrap();
        seed_billed_cost_records(&store);
        let (from, to) = window();

        let page = store
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
        let mut plans: Vec<(String, Option<String>, Option<String>, Option<String>)> = page
            .records
            .into_iter()
            .map(|r| {
                (
                    r.billing.as_str().to_string(),
                    r.origin,
                    r.payer,
                    r.credential_tier.map(|tier| tier.as_str().to_string()),
                )
            })
            .collect();
        plans.sort();
        assert_eq!(
            plans,
            [
                (
                    "subscription".to_string(),
                    Some("heartbeat".to_string()),
                    Some("install".to_string()),
                    Some("system_shared".to_string())
                ),
                (
                    "subscription".to_string(),
                    Some("interactive".to_string()),
                    Some("alice".to_string()),
                    Some("global_user".to_string())
                ),
            ]
        );

        let metered = store
            .cost_detail_records(
                from,
                to,
                CostDetailFilter {
                    billing: Some(Billing::Metered),
                    ..CostDetailFilter::default()
                },
                100,
                0,
            )
            .unwrap();
        assert_eq!(metered.total, 2);
        assert!(metered
            .records
            .iter()
            .all(|r| r.billing == Billing::Metered && r.payer.is_none()));
        assert_eq!(metered.distinct_users, ["alice"]);
    }

    #[test]
    fn billing_totals_ignore_the_billing_filter() {
        let store = SqliteStore::in_memory().unwrap();
        seed_billed_cost_records(&store);
        let (from, to) = window();
        let filter = CostDetailFilter {
            scope: CostScope {
                agent_id: Some("agent-a"),
                ..CostScope::default()
            },
            billing: Some(Billing::Subscription),
        };

        let page = store.cost_detail_records(from, to, filter, 100, 0).unwrap();
        let totals = BillingSplit::of(&store.cost_split(from, to, filter.scope).unwrap());
        assert_eq!(page.total, 2);
        assert_eq!(
            totals,
            BillingSplit {
                metered_cost_usd: 0.125,
                subscription_cost_usd: 0.75,
                subscription_calls: 2,
            }
        );
    }

    #[test]
    fn cost_detail_sum_matches_total_cost_summary() {
        // Invariant: the sum of cost_usd over the cost_detail page must
        // equal the value returned by total_cost() for the same range.
        // This is what guarantees "every LLM call shows up correctly in
        // the dashboards".
        let store = SqliteStore::in_memory().unwrap();
        seed_cost_records(&store);

        let from = Utc::now() - chrono::Duration::hours(1);
        let to = Utc::now() + chrono::Duration::hours(1);

        let page = store
            .cost_detail_records(from, to, CostDetailFilter::default(), 1000, 0)
            .unwrap();
        let detail_sum: f64 = page.records.iter().map(|r| r.cost_usd).sum();
        let summary_total = store.total_cost(from, to).unwrap();
        assert!(
            (detail_sum - summary_total).abs() < 1e-9,
            "detail sum {detail_sum} != summary total {summary_total}"
        );
    }

    #[test]
    fn scheduled_job_history_returns_only_job_runner_rows() {
        let store = SqliteStore::in_memory().unwrap();

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
            started_at: Utc::now().to_rfc3339(),
            completed_at: Utc::now().to_rfc3339(),
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

        store
            .record_task_telemetry(&mk("job_runner", "scheduler-1", true), &[], &[])
            .unwrap();
        store
            .record_task_telemetry(&mk("job_runner", "scheduler-2", false), &[], &[])
            .unwrap();
        // A non-job_runner row (regular agent run) — must be excluded.
        store
            .record_task_telemetry(&mk("anthropic", "agent-x", true), &[], &[])
            .unwrap();

        let from = Utc::now() - chrono::Duration::hours(1);
        let to = Utc::now() + chrono::Duration::hours(1);

        let page = store
            .scheduled_job_history(from, to, CostScope::default(), 100, 0)
            .unwrap();
        assert_eq!(page.total, 2);
        assert_eq!(page.records.len(), 2);
        assert!(page
            .records
            .iter()
            .all(|r| r.agent_id.starts_with("scheduler-")));

        let page = store
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
            store
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
    }

    #[test]
    fn scheduled_job_history_includes_scheduler_provider_rows() {
        // Both 'job_runner' (Tool/Shell/Monitor) AND 'scheduler' (AgentTurn
        // wrapper) rows must surface in the scheduler history view.
        let store = SqliteStore::in_memory().unwrap();
        let mk = |provider: &str, task_id: Option<&str>| TaskTelemetryRecord {
            agent_id: "allan".into(),
            model: String::new(),
            provider: provider.into(),
            input_tokens: 0,
            output_tokens: 0,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
            cost_usd: 0.0,
            rounds: 1,
            duration_ms: 100,
            tool_invocation_count: 0,
            success: true,
            error_type: None,
            retry_count: 0,
            task_type: Some("scheduler:job-1".into()),
            task_id: task_id.map(String::from),
            started_at: Utc::now().to_rfc3339(),
            completed_at: Utc::now().to_rfc3339(),
            forecast_cost_usd: None,
            forecast_rounds: None,
            forecast_confidence: None,
            team_id: None,
            session_id: None,
            model_switches: None,
            ticket_id: None,
            identity_id: None,
            description: Some("a job".into()),
        };
        store
            .record_task_telemetry(&mk("job_runner", Some("job-runner-id")), &[], &[])
            .unwrap();
        store
            .record_task_telemetry(&mk("scheduler", Some("scheduler:job-1:42")), &[], &[])
            .unwrap();
        store
            .record_task_telemetry(&mk("anthropic", Some("scheduler:job-1:42")), &[], &[])
            .unwrap();

        let from = Utc::now() - chrono::Duration::hours(1);
        let to = Utc::now() + chrono::Duration::hours(1);

        let page = store
            .scheduled_job_history(from, to, CostScope::default(), 100, 0)
            .unwrap();
        assert_eq!(page.total, 2, "want only job_runner + scheduler rows");
        let providers: Vec<&str> = page
            .records
            .iter()
            .filter_map(|r| r.task_type.as_deref())
            .collect();
        assert_eq!(providers.len(), 2);
    }

    #[test]
    fn scheduled_job_history_cost_usd_comes_from_cost_records_by_task_id() {
        // The handler must NOT pull cost_usd from task_telemetry (which
        // would double-count the inner provider row). Instead it must
        // SUM cost_records by task_id.
        let store = SqliteStore::in_memory().unwrap();
        let task_id = "scheduler:job-7:99";

        // Three cost_records with the matching task_id → $0.42 total.
        for amount in [0.10, 0.12, 0.20] {
            store
                .record_cost(&CostRecord {
                    agent_id: "allan".into(),
                    model: "claude-sonnet-4-6".into(),
                    input_tokens: 0,
                    output_tokens: 0,
                    cache_creation_input_tokens: 0,
                    cache_read_input_tokens: 0,
                    cost_usd: amount,
                    team_id: None,
                    task_id: Some(task_id.into()),
                    task_type: None,
                    ticket_id: None,
                    identity_id: None,
                    description: None,
                    ..Default::default()
                })
                .unwrap();
        }

        // Scheduler-marker row with cost_usd=0 in task_telemetry.
        store
            .record_task_telemetry(
                &TaskTelemetryRecord {
                    agent_id: "allan".into(),
                    model: String::new(),
                    provider: "scheduler".into(),
                    input_tokens: 0,
                    output_tokens: 0,
                    cache_creation_input_tokens: 0,
                    cache_read_input_tokens: 0,
                    cost_usd: 0.0,
                    rounds: 0,
                    duration_ms: 100,
                    tool_invocation_count: 0,
                    success: true,
                    error_type: None,
                    retry_count: 0,
                    task_type: Some("scheduler:job-7".into()),
                    task_id: Some(task_id.into()),
                    started_at: Utc::now().to_rfc3339(),
                    completed_at: Utc::now().to_rfc3339(),
                    forecast_cost_usd: None,
                    forecast_rounds: None,
                    forecast_confidence: None,
                    team_id: None,
                    session_id: None,
                    model_switches: None,
                    ticket_id: None,
                    identity_id: None,
                    description: Some("daily report".into()),
                },
                &[],
                &[],
            )
            .unwrap();

        let from = Utc::now() - chrono::Duration::hours(1);
        let to = Utc::now() + chrono::Duration::hours(1);
        let page = store
            .scheduled_job_history(from, to, CostScope::default(), 100, 0)
            .unwrap();
        assert_eq!(page.records.len(), 1);
        assert!(
            (page.records[0].cost_usd - 0.42).abs() < 1e-9,
            "want $0.42 from cost_records SUM, got {}",
            page.records[0].cost_usd
        );
    }
}
