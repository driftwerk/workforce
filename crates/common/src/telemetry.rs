//! `TelemetryStore` trait — backend-agnostic abstraction over the data
//! that lives in `persistence.rs::SqliteStore` today.
//!
//! This is **Phase 1A** of the multi-store Postgres migration roadmap
//! (see `docs/POSTGRES.md`). It defines the trait surface and provides
//! a delegating impl for the existing [`SqliteStore`] so every existing
//! call site can switch from `Arc<SqliteStore>` to
//! `Arc<dyn TelemetryStore>` without any behavioural change. A
//! `PostgresTelemetryStore` impl will land in a follow-up PR.
//!
//! The trait is intentionally kept synchronous to mirror the existing
//! `SqliteStore` API. The future Postgres impl will use blocking
//! `tokio::task::spawn_blocking` semantics or move to an async trait
//! when the broader codebase is ready — that decision is deferred to
//! Phase 1B.

use chrono::{DateTime, Utc};
use eyre::Result;

use crate::activity_log::{ActivityQuery, ActivityRecord};
use crate::admin::{AuditEntry, AuditOutcome};
use crate::cost::TokenHistoryPoint;
use crate::persistence::{
    AgentModelPreference, AgentUsageSummary, CacheStatusEntry, CostDetailFilter, CostDetailPage,
    CostRecord, CostScope, CostSplitRow, DailyUsageRow, EfficiencyTrendPoint, InstanceFilter,
    MeteredSpend, ModelComparison, PolicyProposalRow, RoundStats, RoundTelemetryRecord,
    ScheduledJobHistoryPage, SqliteStore, StoredModelPricing, TaskHistoryEntry, TaskHistoryFilter,
    TaskHistoryPage, TaskRow, TaskTelemetryRecord, TaskTrace, TaskUsageRow, TicketCostSummary,
    ToolCostEntry, ToolTelemetryRecord, ToolTurn, WorkflowInstanceRow,
};
use crate::security_audit::{SecurityEvent, SecurityEventFilter, SecurityEventRow, SecurityStats};

/// One row in the append-only `lesson_events` log. Backs the admin Lessons
/// dashboard (corpus growth, reinforcement, cross-day recurrence, per-lesson
/// prevention rate). Numeric fields are `f64`/`i64` so the same value binds
/// cleanly to both the SQLite (`REAL`/`INTEGER`) and Postgres
/// (`DOUBLE PRECISION`/`BIGINT`) columns.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct LessonEvent {
    pub timestamp: DateTime<Utc>,
    pub agent_id: String,
    pub identity_id: Option<String>,
    pub lesson_id: Option<String>,
    /// `extracted` | `injected` | `reinforced_positive` | `reinforced_negative`
    /// | `merged_recurrence` | `pruned` | `superseded` | `hit` | `recall_miss`
    pub event_type: String,
    pub learning_kind: Option<String>,
    pub recall_score: Option<f64>,
    pub importance_delta: Option<f64>,
    pub confidence_after: Option<f64>,
    pub applied_after: Option<i64>,
    pub session_id: Option<String>,
    /// Trigger handles (without the `trigger:` prefix) — needed for the
    /// prevention-rate join (injected → later same-trigger recurrence).
    pub trigger_tags: Vec<String>,
}

impl LessonEvent {
    /// Construct a minimally-populated event at the given time; callers set the
    /// optional fields relevant to the event type via the `with_*` helpers.
    pub fn new(
        timestamp: DateTime<Utc>,
        agent_id: impl Into<String>,
        event_type: impl Into<String>,
    ) -> Self {
        Self {
            timestamp,
            agent_id: agent_id.into(),
            identity_id: None,
            lesson_id: None,
            event_type: event_type.into(),
            learning_kind: None,
            recall_score: None,
            importance_delta: None,
            confidence_after: None,
            applied_after: None,
            session_id: None,
            trigger_tags: Vec::new(),
        }
    }

    pub fn with_identity(mut self, identity_id: impl Into<String>) -> Self {
        self.identity_id = Some(identity_id.into());
        self
    }

    pub fn with_session(mut self, session_id: Option<String>) -> Self {
        self.session_id = session_id;
        self
    }
}

/// One scored human DM turn, backing per-user relationship tracking. The
/// rolled-up summary (decayed sentiment, reaction counts, key moments) is
/// computed in-process over a time window. Numeric fields are `f64` so the same
/// value binds to both SQLite (`REAL`) and Postgres (`DOUBLE PRECISION`).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RelationshipEvent {
    pub timestamp: DateTime<Utc>,
    pub agent_id: String,
    pub identity_id: String,
    pub session_id: String,
    pub message_id: String,
    pub valence: f64,
    pub intensity: f64,
    pub emotion_label: Option<String>,
    pub reaction: String,
    pub is_key_moment: bool,
    pub confidence: f64,
    pub source: String,
    /// JSON object of raw marker counts, kept so the summary stays interrogable.
    pub signals: String,
}

/// Where telemetry holds a person beyond billing amounts: the rows of each
/// table whose `identity_id` names them and that its filter keeps. Counting,
/// moving and erasing are all built from this one list, so a move or an
/// erase changes exactly the rows a count counts. A move is to the same
/// person under another identity, so a described row goes with them, its
/// excerpt kept; the rest of their billing moves by [`BILLING_NAMES`].
pub(crate) const SIGNAL_SOURCES: [SignalSource; 4] = [
    SignalSource {
        table: "relationship_events",
        filter: "",
        signal: Signal::Relationship,
        moved: Change::Rename,
        erased: Change::Remove,
    },
    SignalSource {
        table: "lesson_events",
        filter: "",
        signal: Signal::Lesson,
        moved: Change::Rename,
        erased: Change::Unname,
    },
    SignalSource {
        table: "cost_records",
        filter: " AND description IS NOT NULL",
        signal: Signal::Described,
        moved: Change::Rename,
        erased: Change::Clear,
    },
    SignalSource {
        table: "task_telemetry",
        filter: " AND description IS NOT NULL",
        signal: Signal::Described,
        moved: Change::Rename,
        erased: Change::Clear,
    },
];

pub(crate) struct SignalSource {
    table: &'static str,
    filter: &'static str,
    pub(crate) signal: Signal,
    pub(crate) moved: Change,
    pub(crate) erased: Change,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Signal {
    Relationship,
    Lesson,
    Described,
}

/// What a move or an erase does to a row naming the person.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Change {
    /// It names the identity moved to instead.
    Rename,
    /// It names nobody.
    Unname,
    Remove,
    /// Its prompt excerpt goes and the row stays, as billing.
    Clear,
}

/// Every column naming the person a billing row was spent by or paid for:
/// kept whatever happens to them, and renamed when they are merged into
/// another identity, so their spend, budget and usage history stay theirs.
/// `credential_tier` and `ledger_key` name no one.
pub(crate) const BILLING_NAMES: [(&str, &str); 3] = [
    ("cost_records", "identity_id"),
    ("cost_records", "payer"),
    ("task_telemetry", "identity_id"),
];

/// Rename the identity bound to `{mark}1` in `table`'s `column` to the one
/// bound to `{mark}2`.
pub(crate) fn rename_sql((table, column): (&str, &str), mark: char) -> String {
    format!("UPDATE {table} SET {column} = {mark}2 WHERE {column} = {mark}1")
}

impl SignalSource {
    /// Rows naming the identity bound to the first parameter, `{mark}1`.
    fn naming(&self, mark: char) -> String {
        format!("identity_id = {mark}1{}", self.filter)
    }

    pub(crate) fn count_sql(&self, mark: char) -> String {
        format!(
            "SELECT COUNT(*) FROM {} WHERE {}",
            self.table,
            self.naming(mark)
        )
    }

    /// `change`, for the identity bound to `{mark}1`; a rename names the
    /// one bound to `{mark}2`.
    pub(crate) fn change_sql(&self, change: Change, mark: char) -> String {
        let (table, naming) = (self.table, self.naming(mark));
        match change {
            Change::Rename => format!("UPDATE {table} SET identity_id = {mark}2 WHERE {naming}"),
            Change::Unname => format!("UPDATE {table} SET identity_id = NULL WHERE {naming}"),
            Change::Remove => format!("DELETE FROM {table} WHERE {naming}"),
            Change::Clear => format!("UPDATE {table} SET description = NULL WHERE {naming}"),
        }
    }
}

/// Every identity holding signals, with how many rows: plain SQL both
/// backends run.
pub(crate) fn signal_holders_sql() -> String {
    let rows = SIGNAL_SOURCES
        .iter()
        .map(|source| {
            format!(
                "SELECT identity_id FROM {} WHERE identity_id IS NOT NULL{}",
                source.table, source.filter
            )
        })
        .collect::<Vec<_>>()
        .join(" UNION ALL ");
    format!("SELECT identity_id, COUNT(*) FROM ({rows}) AS signals GROUP BY identity_id")
}

/// How much of a person telemetry holds outside its billing amounts, or
/// how much of it a move or an erase changed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct IdentitySignals {
    pub relationship_events: usize,
    pub lesson_events: usize,
    /// Cost and task rows carrying a prompt excerpt.
    pub described_rows: usize,
}

impl IdentitySignals {
    pub fn total(&self) -> usize {
        self.relationship_events + self.lesson_events + self.described_rows
    }

    pub(crate) fn add(mut self, signal: Signal, rows: usize) -> Self {
        *match signal {
            Signal::Relationship => &mut self.relationship_events,
            Signal::Lesson => &mut self.lesson_events,
            Signal::Described => &mut self.described_rows,
        } += rows;
        self
    }
}

/// Per-session high-water mark the code-only periodic relationship scan advances
/// so it never re-scores a turn.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RelationshipCursor {
    pub session_id: String,
    pub agent_id: String,
    pub last_message_ts: DateTime<Utc>,
    pub last_message_id: String,
    pub updated_at: DateTime<Utc>,
}

/// Rows one prune statement removes, so no delete holds its locks for long.
pub const PRUNE_BATCH: usize = 5_000;

/// Batches one prune pass runs at most, so a first pass over years of rows
/// is bounded and the rest waits for the next pass.
pub const PRUNE_PASS_BATCHES: usize = 100;

/// Run `batch`, one delete of at most [`PRUNE_BATCH`] rows, until one comes
/// up short or [`PRUNE_PASS_BATCHES`] have run; returns the rows removed.
pub fn prune_in_batches(mut batch: impl FnMut() -> Result<usize>) -> Result<usize> {
    let mut removed = 0;
    for _ in 0..PRUNE_PASS_BATCHES {
        let deleted = batch()?;
        removed += deleted;
        if deleted < PRUNE_BATCH {
            break;
        }
    }
    Ok(removed)
}

/// The `task_id` prefix of every scheduled job's run
/// (`"scheduler:{job_id}:{ts}"`).
pub const JOB_TASK_PREFIX: &str = "scheduler:";

/// Backend-agnostic telemetry / cost / audit / workflow store.
///
/// Implementors persist task history, cost records, telemetry,
/// model-health events, agent preferences, model pricing, policy
/// proposals, workflow instance state, and security events. SQLite is
/// the default backend; Postgres lands in Phase 1B.
///
/// All methods are synchronous and take `&self` so the trait can be
/// used as `Arc<dyn TelemetryStore>` from any thread.
pub trait TelemetryStore: Send + Sync + 'static {
    // ─── Tasks ───
    fn save_task(&self, task: &TaskRow) -> Result<()>;
    fn get_task(&self, id: &str) -> Result<Option<TaskRow>>;
    fn list_tasks(&self, status: Option<&str>) -> Result<Vec<TaskRow>>;
    fn update_task_status(&self, id: &str, status: &str) -> Result<()>;

    // ─── Audit log ───
    fn record_audit(
        &self,
        actor: &str,
        action: &str,
        resource: &str,
        detail: Option<&str>,
        outcome: AuditOutcome,
    ) -> Result<()>;
    fn recent_audit(&self, limit: usize) -> Result<Vec<AuditEntry>>;
    fn search_audit(&self, query: &str, limit: usize) -> Result<Vec<AuditEntry>>;

    // ─── Cost records ───
    /// Record a cost row, returning whether it was inserted: a row whose
    /// `ledger_key` is already recorded is not.
    fn record_cost(&self, record: &CostRecord) -> Result<bool>;
    fn total_cost(&self, from: DateTime<Utc>, to: DateTime<Utc>) -> Result<f64>;
    fn cost_by_agent(&self, from: DateTime<Utc>, to: DateTime<Utc>) -> Result<Vec<(String, f64)>>;
    /// Lifetime metered cost over records whose `task_id` starts with
    /// `prefix`. Scheduler runs stamp `task_id = "scheduler:{job_id}:{ts}"`,
    /// so the prefix `"scheduler:{job_id}:"` totals one job's spend.
    fn metered_cost_by_task_prefix(&self, prefix: &str) -> Result<f64>;
    /// Metered spend in the window, grouped by agent and identity: the spend
    /// budgets count. A subscription call's estimated value is not money
    /// spent, so it never consumes a budget.
    fn metered_spend(&self, from: DateTime<Utc>, to: DateTime<Utc>) -> Result<Vec<MeteredSpend>>;
    fn cost_by_model(&self, from: DateTime<Utc>, to: DateTime<Utc>) -> Result<Vec<(String, f64)>>;
    fn cost_token_totals(
        &self,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<(u64, u64, usize)>;
    fn cost_token_history_by_day(
        &self,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<Vec<TokenHistoryPoint>>;
    fn cost_token_history_by_month(
        &self,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<Vec<TokenHistoryPoint>>;
    fn cost_by_provider(
        &self,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<Vec<(String, f64)>>;
    /// Paginated per-call cost-attribution ledger backing the /costs page.
    /// One row per billable LLM call (agent runs, vision, transcription,
    /// workflow steps).
    fn cost_detail_records(
        &self,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
        filter: CostDetailFilter<'_>,
        limit: u32,
        offset: u32,
    ) -> Result<CostDetailPage>;
    /// Calls and cost in `scope`, per agent, billing, origin, payer and
    /// credential tier: the one aggregation every billing split folds.
    fn cost_split(
        &self,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
        scope: CostScope<'_>,
    ) -> Result<Vec<CostSplitRow>>;
    /// Paginated execution log for scheduled jobs (`provider IN
    /// ('job_runner', 'scheduler')` rows in `task_telemetry`). Backs the
    /// scheduler UI's history/cost tab. Per-run `cost_usd` is back-
    /// attributed via a LEFT JOIN on `cost_records.task_id`. The scope's
    /// `user_id` matches the run's `identity_id`.
    fn scheduled_job_history(
        &self,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
        scope: CostScope<'_>,
        limit: u32,
        offset: u32,
    ) -> Result<ScheduledJobHistoryPage>;

    // ─── Team / agent usage ───
    fn get_team_usage_summary(
        &self,
        team_id: &str,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<Vec<AgentUsageSummary>>;
    fn get_team_cost(&self, team_id: &str, from: DateTime<Utc>, to: DateTime<Utc>) -> Result<f64>;
    fn cost_by_team(&self, from: DateTime<Utc>, to: DateTime<Utc>) -> Result<Vec<(String, f64)>>;

    // ─── Telemetry ───
    fn record_task_telemetry(
        &self,
        task: &TaskTelemetryRecord,
        rounds: &[RoundTelemetryRecord],
        tools: &[ToolTelemetryRecord],
    ) -> Result<i64>;
    fn get_agent_usage_summary(
        &self,
        agent_id: &str,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
        group_by: &str,
    ) -> Result<Vec<AgentUsageSummary>>;
    fn compare_models_for_agent(
        &self,
        agent_id: &str,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<Vec<ModelComparison>>;
    fn get_task_history(
        &self,
        agent_id: &str,
        limit: usize,
        task_type: Option<&str>,
        model: Option<&str>,
    ) -> Result<Vec<TaskHistoryEntry>>;
    fn get_task_history_filtered(
        &self,
        agent_id: &str,
        limit: usize,
        task_type: Option<&str>,
        model: Option<&str>,
        since: Option<&str>,
        sort_by: Option<&str>,
    ) -> Result<Vec<TaskHistoryEntry>>;

    // ─── Agent model preferences ───
    fn upsert_agent_model_preference(&self, pref: &AgentModelPreference) -> Result<()>;
    fn get_agent_model_preference(
        &self,
        agent_id: &str,
        task_type: &str,
    ) -> Result<Option<AgentModelPreference>>;

    // ─── Model health ───
    fn record_model_health_event(
        &self,
        model: &str,
        event_type: &str,
        detail: Option<&str>,
        agent_id: Option<&str>,
    ) -> Result<()>;

    // ─── Lessons-learned events ───
    //
    // Default no-op bodies so non-persistent / mock implementors are unaffected;
    // the SQLite and Postgres stores override both.
    /// Append one lesson-pipeline event (extraction, injection, reinforcement,
    /// recurrence, prune, supersede). Fire-and-forget at the call sites.
    fn record_lesson_event(&self, _event: &LessonEvent) -> Result<()> {
        Ok(())
    }
    /// Fetch all lesson events in `[from, to]`, optionally scoped to one agent,
    /// ordered by time ascending. The admin dashboard computes daily counts,
    /// recurrence, and prevention rate from this window in-process.
    fn lesson_events_in_range(
        &self,
        _agent_id: Option<&str>,
        _from: DateTime<Utc>,
        _to: DateTime<Utc>,
    ) -> Result<Vec<LessonEvent>> {
        Ok(Vec::new())
    }

    // ─── Relationship events ───
    //
    // Default no-op bodies so non-persistent / mock implementors are unaffected;
    // the SQLite and Postgres stores override all four.
    /// Append one relationship event; `true` when a new row was inserted (a
    /// re-attempt of the same `(session_id, message_id)` is ignored).
    fn record_relationship_event(&self, _event: &RelationshipEvent) -> Result<bool> {
        Ok(false)
    }
    fn relationship_events_in_range(
        &self,
        _agent_id: Option<&str>,
        _identity_id: Option<&str>,
        _from: DateTime<Utc>,
        _to: DateTime<Utc>,
    ) -> Result<Vec<RelationshipEvent>> {
        Ok(Vec::new())
    }
    fn get_relationship_cursor(&self, _session_id: &str) -> Result<Option<RelationshipCursor>> {
        Ok(None)
    }
    fn upsert_relationship_cursor(&self, _cursor: &RelationshipCursor) -> Result<()> {
        Ok(())
    }

    // ─── Cache and tool stats ───
    fn get_cache_status(
        &self,
        agent_id: &str,
        from: DateTime<Utc>,
    ) -> Result<Vec<CacheStatusEntry>>;
    fn get_round_cache_stats(&self, agent_id: &str, from: DateTime<Utc>) -> Result<(u64, u64)>;
    fn get_tool_cost_breakdown(
        &self,
        agent_id: &str,
        from: DateTime<Utc>,
        limit: usize,
    ) -> Result<Vec<ToolCostEntry>>;
    fn get_routing_history(
        &self,
        agent_id: &str,
        since: DateTime<Utc>,
        limit: usize,
    ) -> Result<Vec<TaskHistoryEntry>>;
    fn agent_tool_turns(
        &self,
        agent_id: &str,
        from: DateTime<Utc>,
        limit: usize,
    ) -> Result<Vec<ToolTurn>>;

    // ─── Per-ticket cost ───
    fn cost_by_ticket(&self, ticket_id: &str) -> Result<TicketCostSummary>;
    fn cost_by_tickets(
        &self,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<Vec<TicketCostSummary>>;

    // ─── Model pricing ───
    fn upsert_model_pricing(&self, pricing: &StoredModelPricing) -> Result<()>;
    fn get_model_pricing(&self, model: &str) -> Result<Option<StoredModelPricing>>;
    fn list_model_pricing(&self) -> Result<Vec<StoredModelPricing>>;
    fn delete_model_pricing(&self, model: &str) -> Result<()>;

    // ─── Policy proposals ───
    fn create_policy_proposal(
        &self,
        agent_id: &str,
        proposed_rule: &str,
        justification: &str,
    ) -> Result<i64>;
    fn list_policy_proposals(
        &self,
        status: Option<&str>,
        limit: usize,
    ) -> Result<Vec<PolicyProposalRow>>;
    fn get_policy_proposal(&self, id: i64) -> Result<Option<PolicyProposalRow>>;
    fn review_policy_proposal(
        &self,
        id: i64,
        status: &str,
        reviewed_by: &str,
        note: Option<&str>,
    ) -> Result<()>;

    // ─── Round outliers / efficiency ───
    fn get_round_outliers(
        &self,
        agent_id: &str,
        since: DateTime<Utc>,
        threshold_multiplier: f64,
        limit: usize,
    ) -> Result<(RoundStats, Vec<TaskHistoryEntry>)>;
    fn get_efficiency_trends(
        &self,
        agent_id: &str,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<Vec<EfficiencyTrendPoint>>;

    // ─── Workflow instances ───
    fn save_workflow_instance(&self, row: &WorkflowInstanceRow) -> Result<()>;
    fn get_workflow_instance(&self, id: &str) -> Result<Option<WorkflowInstanceRow>>;
    /// The instances named by `ids` that exist, in no particular order.
    fn get_workflow_instances(&self, ids: &[String]) -> Result<Vec<WorkflowInstanceRow>>;
    fn list_workflow_instances(
        &self,
        status: Option<&str>,
        workflow_id: Option<&str>,
    ) -> Result<Vec<WorkflowInstanceRow>>;
    /// The instances `filter` admits, most recently updated first, at most
    /// `filter.limit` of them.
    fn list_workflow_instances_filtered(
        &self,
        filter: &InstanceFilter,
    ) -> Result<Vec<WorkflowInstanceRow>>;
    /// One page of a walk through every instance, oldest update first: up to
    /// `limit` instances updated after the moment `after`, or at it with an
    /// id past `after_id`. A page's last row is the next page's `after`.
    fn workflow_instances_after(
        &self,
        after: DateTime<Utc>,
        after_id: &str,
        limit: usize,
    ) -> Result<Vec<WorkflowInstanceRow>>;
    fn list_child_instances(&self, parent_id: &str) -> Result<Vec<WorkflowInstanceRow>>;
    fn update_workflow_instance_status(
        &self,
        id: &str,
        status: &str,
        current_step_id: Option<&str>,
    ) -> Result<()>;
    fn find_instance_by_thread(
        &self,
        channel_id: &str,
        thread_id: &str,
        statuses: &[&str],
    ) -> Result<Option<WorkflowInstanceRow>>;
    fn set_instance_thread(&self, id: &str, channel_id: &str, thread_id: &str) -> Result<()>;

    /// Atomically claim an instance for execution by `owner`. Wins when the
    /// row is unowned, the lease has expired, or `owner` already holds it —
    /// the cross-instance guard against duplicate execute loops.
    fn try_claim_workflow_instance(&self, id: &str, owner: &str, ttl_secs: i64) -> Result<bool>;
    /// Extend the lease; `false` when `owner` no longer holds it.
    fn renew_workflow_lease(&self, id: &str, owner: &str, ttl_secs: i64) -> Result<bool>;
    /// Release the lease if `owner` holds it (no-op otherwise).
    fn release_workflow_instance(&self, id: &str, owner: &str) -> Result<()>;
    /// Fenced full-row save: applies only while `owner` holds the row.
    /// `false` means the caller was fenced out and must stop executing.
    fn save_workflow_instance_owned(&self, row: &WorkflowInstanceRow, owner: &str) -> Result<bool>;
    /// Full-row save from outside the execute loop: applies only while the
    /// row is still at `expected_version`, the version it was read at, and
    /// moves the version on. `false` means the row changed meanwhile; read it
    /// again and redo the change.
    fn save_workflow_instance_fenced(
        &self,
        row: &WorkflowInstanceRow,
        expected_version: i64,
    ) -> Result<bool>;
    /// Targeted terminal-status write from ANY instance (abort/cancel): sets
    /// status and records the reason under `reason_key` in `variables`
    /// without touching `step_history` — the owning loop notices at its next
    /// step boundary.
    fn cancel_workflow_instance(
        &self,
        id: &str,
        status: &str,
        reason_key: &str,
        reason: &str,
    ) -> Result<()>;

    /// Delete one workflow instance outright. `true` when a row went.
    fn delete_workflow_instance(&self, id: &str) -> Result<bool>;

    // ─── A person's signals (identity lifecycle) ───
    /// What telemetry holds about `identity` beyond its billing amounts.
    fn identity_signals(&self, identity: &str) -> Result<IdentitySignals>;
    /// [`Self::identity_signals`]' total for every identity holding any.
    fn identity_signal_holders(&self) -> Result<std::collections::BTreeMap<String, usize>>;
    /// Make everything [`Self::identity_signals`] counts for `from` name
    /// `to` ([`SIGNAL_SOURCES`]). Returns the rows changed.
    fn reassign_identity_signals(&self, from: &str, to: &str) -> Result<IdentitySignals>;
    /// Make every billing column naming `from` name `to`
    /// ([`BILLING_NAMES`]), amounts untouched. Returns the cells changed.
    fn reassign_identity_billing(&self, from: &str, to: &str) -> Result<usize>;
    /// Erase what [`Self::identity_signals`] counts for `identity`: its
    /// relationship events, the identity on its lesson events, and the
    /// prompt excerpts on its cost and task rows. The amounts and the
    /// identity on them stay, as billing records. Returns the rows changed.
    fn forget_identity_signals(&self, identity: &str) -> Result<IdentitySignals>;

    // ─── Activity history (joins task / tool / round telemetry) ───
    fn query_activities(&self, query: &ActivityQuery) -> Result<Vec<ActivityRecord>>;

    // ─── Retention ───
    // Each prune runs one bounded pass (`prune_in_batches`) and returns
    // how many rows went; a backlog is left to the next call.
    /// Remove tasks that completed before `before`, with their rounds and
    /// tool calls.
    fn prune_activity(&self, before: DateTime<Utc>) -> Result<usize>;
    /// Remove cost records made before `before`, except a scheduled job's
    /// ([`JOB_TASK_PREFIX`]): its budget counts what the job has spent over
    /// its whole life.
    fn prune_costs(&self, before: DateTime<Utc>) -> Result<usize>;

    // ─── Cost analytics row fetchers ───
    fn task_usage_rows(&self, from: DateTime<Utc>, to: DateTime<Utc>) -> Result<Vec<TaskUsageRow>>;
    fn daily_usage_rows(&self, since: DateTime<Utc>) -> Result<Vec<DailyUsageRow>>;

    // ─── Security event audit log ───
    fn insert_security_event(&self, event: &SecurityEvent) -> Result<()>;
    fn query_security_events(&self, filter: &SecurityEventFilter) -> Result<Vec<SecurityEventRow>>;
    fn security_event_stats(&self, since: &str) -> Result<SecurityStats>;
    fn cleanup_security_events(&self, before: &str) -> Result<usize>;

    // ─── Per-identity stats slices ───
    /// Total cost grouped by `identity_id`. Identities never set
    /// (system-initiated tasks, pre-migration rows) are reported under the
    /// literal key `"unknown"` so the UI can surface them as a single
    /// bucket without filtering.
    fn cost_by_identity(
        &self,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<Vec<(String, f64)>>;

    fn cost_token_history_by_identity_day(
        &self,
        identity_id: &str,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<Vec<TokenHistoryPoint>>;

    fn cost_token_history_by_agent_day(
        &self,
        agent_id: &str,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<Vec<TokenHistoryPoint>>;

    /// Mirror of [`Self::get_agent_usage_summary`] sliced by identity.
    /// `group_by` is one of `"model" | "task_type" | "provider" | "agent"`.
    fn get_identity_usage_summary(
        &self,
        identity_id: &str,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
        group_by: &str,
    ) -> Result<Vec<AgentUsageSummary>>;

    fn compare_models_for_identity(
        &self,
        identity_id: &str,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<Vec<ModelComparison>>;

    fn tool_breakdown_for_identity(
        &self,
        identity_id: &str,
        from: DateTime<Utc>,
        limit: usize,
    ) -> Result<Vec<ToolCostEntry>>;

    /// Identities that have triggered this agent in the window, ordered
    /// by total cost descending. `None` cost means the identity has run
    /// tasks against this agent but their cost was zero.
    fn agent_identity_breakdown(
        &self,
        agent_id: &str,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<Vec<(String, f64)>>;

    /// Cost grouped by `task_type`. Used by the global cost-breakdown
    /// endpoint so the UI can show one of "where did the money go" axes.
    fn cost_by_task_type(
        &self,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<Vec<(String, f64)>>;

    /// Cursor-paginated task history with optional identity / agent /
    /// task_type / model / time-range filters. Cursor encodes
    /// `(completed_at, id)` so pagination is stable under concurrent
    /// inserts.
    fn task_history_paginated(&self, filter: &TaskHistoryFilter) -> Result<TaskHistoryPage>;

    /// End-to-end trace for one task (`task_id` from the application
    /// layer, not the bigserial primary key). Returns the
    /// `task_telemetry` row, every round, every tool call, and every
    /// matching `cost_records` row in one shot.
    fn get_task_trace(&self, task_id: &str) -> Result<Option<TaskTrace>>;

    // ─── Global (cross-agent) variants ───
    /// Tool cost breakdown across every agent in the window.
    fn tool_breakdown_global(
        &self,
        from: DateTime<Utc>,
        limit: usize,
    ) -> Result<Vec<ToolCostEntry>>;

    /// Daily efficiency trend across every agent in the window.
    fn efficiency_trends_global(
        &self,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<Vec<EfficiencyTrendPoint>>;

    /// Round outliers across every agent in the window.
    fn round_outliers_global(
        &self,
        since: DateTime<Utc>,
        threshold_multiplier: f64,
        limit: usize,
    ) -> Result<(RoundStats, Vec<TaskHistoryEntry>)>;
}

// ─── Delegating impl for the existing SqliteStore ─────────────────────
//
// Each method forwards to the inherent `SqliteStore` method via fully
// qualified syntax so dispatch is unambiguous (no recursion). This is a
// pure mechanical bridge — every call goes straight through to the
// existing implementation, so behaviour is preserved bit-for-bit.

impl TelemetryStore for SqliteStore {
    fn save_task(&self, task: &TaskRow) -> Result<()> {
        SqliteStore::save_task(self, task)
    }
    fn get_task(&self, id: &str) -> Result<Option<TaskRow>> {
        SqliteStore::get_task(self, id)
    }
    fn list_tasks(&self, status: Option<&str>) -> Result<Vec<TaskRow>> {
        SqliteStore::list_tasks(self, status)
    }
    fn update_task_status(&self, id: &str, status: &str) -> Result<()> {
        SqliteStore::update_task_status(self, id, status)
    }

    fn record_audit(
        &self,
        actor: &str,
        action: &str,
        resource: &str,
        detail: Option<&str>,
        outcome: AuditOutcome,
    ) -> Result<()> {
        SqliteStore::record_audit(self, actor, action, resource, detail, outcome)
    }
    fn recent_audit(&self, limit: usize) -> Result<Vec<AuditEntry>> {
        SqliteStore::recent_audit(self, limit)
    }
    fn search_audit(&self, query: &str, limit: usize) -> Result<Vec<AuditEntry>> {
        SqliteStore::search_audit(self, query, limit)
    }

    fn record_cost(&self, record: &CostRecord) -> Result<bool> {
        SqliteStore::record_cost(self, record)
    }
    fn total_cost(&self, from: DateTime<Utc>, to: DateTime<Utc>) -> Result<f64> {
        SqliteStore::total_cost(self, from, to)
    }
    fn cost_by_agent(&self, from: DateTime<Utc>, to: DateTime<Utc>) -> Result<Vec<(String, f64)>> {
        SqliteStore::cost_by_agent(self, from, to)
    }
    fn cost_by_model(&self, from: DateTime<Utc>, to: DateTime<Utc>) -> Result<Vec<(String, f64)>> {
        SqliteStore::cost_by_model(self, from, to)
    }
    fn cost_token_totals(
        &self,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<(u64, u64, usize)> {
        SqliteStore::cost_token_totals(self, from, to)
    }
    fn cost_token_history_by_day(
        &self,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<Vec<TokenHistoryPoint>> {
        SqliteStore::cost_token_history_by_day(self, from, to)
    }
    fn cost_token_history_by_month(
        &self,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<Vec<TokenHistoryPoint>> {
        SqliteStore::cost_token_history_by_month(self, from, to)
    }
    fn cost_by_provider(
        &self,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<Vec<(String, f64)>> {
        SqliteStore::cost_by_provider(self, from, to)
    }
    fn cost_detail_records(
        &self,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
        filter: CostDetailFilter<'_>,
        limit: u32,
        offset: u32,
    ) -> Result<CostDetailPage> {
        SqliteStore::cost_detail_records(self, from, to, filter, limit, offset)
    }
    fn cost_split(
        &self,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
        scope: CostScope<'_>,
    ) -> Result<Vec<CostSplitRow>> {
        SqliteStore::cost_split(self, from, to, scope)
    }
    fn scheduled_job_history(
        &self,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
        scope: CostScope<'_>,
        limit: u32,
        offset: u32,
    ) -> Result<ScheduledJobHistoryPage> {
        SqliteStore::scheduled_job_history(self, from, to, scope, limit, offset)
    }

    fn get_team_usage_summary(
        &self,
        team_id: &str,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<Vec<AgentUsageSummary>> {
        SqliteStore::get_team_usage_summary(self, team_id, from, to)
    }
    fn get_team_cost(&self, team_id: &str, from: DateTime<Utc>, to: DateTime<Utc>) -> Result<f64> {
        SqliteStore::get_team_cost(self, team_id, from, to)
    }
    fn cost_by_team(&self, from: DateTime<Utc>, to: DateTime<Utc>) -> Result<Vec<(String, f64)>> {
        SqliteStore::cost_by_team(self, from, to)
    }

    fn record_task_telemetry(
        &self,
        task: &TaskTelemetryRecord,
        rounds: &[RoundTelemetryRecord],
        tools: &[ToolTelemetryRecord],
    ) -> Result<i64> {
        SqliteStore::record_task_telemetry(self, task, rounds, tools)
    }
    fn get_agent_usage_summary(
        &self,
        agent_id: &str,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
        group_by: &str,
    ) -> Result<Vec<AgentUsageSummary>> {
        SqliteStore::get_agent_usage_summary(self, agent_id, from, to, group_by)
    }
    fn compare_models_for_agent(
        &self,
        agent_id: &str,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<Vec<ModelComparison>> {
        SqliteStore::compare_models_for_agent(self, agent_id, from, to)
    }
    fn get_task_history(
        &self,
        agent_id: &str,
        limit: usize,
        task_type: Option<&str>,
        model: Option<&str>,
    ) -> Result<Vec<TaskHistoryEntry>> {
        SqliteStore::get_task_history(self, agent_id, limit, task_type, model)
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
        SqliteStore::get_task_history_filtered(
            self, agent_id, limit, task_type, model, since, sort_by,
        )
    }

    fn upsert_agent_model_preference(&self, pref: &AgentModelPreference) -> Result<()> {
        SqliteStore::upsert_agent_model_preference(self, pref)
    }
    fn get_agent_model_preference(
        &self,
        agent_id: &str,
        task_type: &str,
    ) -> Result<Option<AgentModelPreference>> {
        SqliteStore::get_agent_model_preference(self, agent_id, task_type)
    }

    fn record_model_health_event(
        &self,
        model: &str,
        event_type: &str,
        detail: Option<&str>,
        agent_id: Option<&str>,
    ) -> Result<()> {
        SqliteStore::record_model_health_event(self, model, event_type, detail, agent_id)
    }

    fn record_lesson_event(&self, event: &LessonEvent) -> Result<()> {
        SqliteStore::record_lesson_event(self, event)
    }
    fn lesson_events_in_range(
        &self,
        agent_id: Option<&str>,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<Vec<LessonEvent>> {
        SqliteStore::lesson_events_in_range(self, agent_id, from, to)
    }

    fn record_relationship_event(&self, event: &RelationshipEvent) -> Result<bool> {
        SqliteStore::record_relationship_event(self, event)
    }
    fn relationship_events_in_range(
        &self,
        agent_id: Option<&str>,
        identity_id: Option<&str>,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<Vec<RelationshipEvent>> {
        SqliteStore::relationship_events_in_range(self, agent_id, identity_id, from, to)
    }
    fn get_relationship_cursor(&self, session_id: &str) -> Result<Option<RelationshipCursor>> {
        SqliteStore::get_relationship_cursor(self, session_id)
    }
    fn upsert_relationship_cursor(&self, cursor: &RelationshipCursor) -> Result<()> {
        SqliteStore::upsert_relationship_cursor(self, cursor)
    }

    fn get_cache_status(
        &self,
        agent_id: &str,
        from: DateTime<Utc>,
    ) -> Result<Vec<CacheStatusEntry>> {
        SqliteStore::get_cache_status(self, agent_id, from)
    }
    fn get_round_cache_stats(&self, agent_id: &str, from: DateTime<Utc>) -> Result<(u64, u64)> {
        SqliteStore::get_round_cache_stats(self, agent_id, from)
    }
    fn get_tool_cost_breakdown(
        &self,
        agent_id: &str,
        from: DateTime<Utc>,
        limit: usize,
    ) -> Result<Vec<ToolCostEntry>> {
        SqliteStore::get_tool_cost_breakdown(self, agent_id, from, limit)
    }
    fn agent_tool_turns(
        &self,
        agent_id: &str,
        from: DateTime<Utc>,
        limit: usize,
    ) -> Result<Vec<ToolTurn>> {
        SqliteStore::agent_tool_turns(self, agent_id, from, limit)
    }
    fn get_routing_history(
        &self,
        agent_id: &str,
        since: DateTime<Utc>,
        limit: usize,
    ) -> Result<Vec<TaskHistoryEntry>> {
        SqliteStore::get_routing_history(self, agent_id, since, limit)
    }

    fn cost_by_ticket(&self, ticket_id: &str) -> Result<TicketCostSummary> {
        SqliteStore::cost_by_ticket(self, ticket_id)
    }
    fn cost_by_tickets(
        &self,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<Vec<TicketCostSummary>> {
        SqliteStore::cost_by_tickets(self, from, to)
    }

    fn upsert_model_pricing(&self, pricing: &StoredModelPricing) -> Result<()> {
        SqliteStore::upsert_model_pricing(self, pricing)
    }
    fn get_model_pricing(&self, model: &str) -> Result<Option<StoredModelPricing>> {
        SqliteStore::get_model_pricing(self, model)
    }
    fn list_model_pricing(&self) -> Result<Vec<StoredModelPricing>> {
        SqliteStore::list_model_pricing(self)
    }
    fn delete_model_pricing(&self, model: &str) -> Result<()> {
        SqliteStore::delete_model_pricing(self, model)
    }

    fn create_policy_proposal(
        &self,
        agent_id: &str,
        proposed_rule: &str,
        justification: &str,
    ) -> Result<i64> {
        SqliteStore::create_policy_proposal(self, agent_id, proposed_rule, justification)
    }
    fn list_policy_proposals(
        &self,
        status: Option<&str>,
        limit: usize,
    ) -> Result<Vec<PolicyProposalRow>> {
        SqliteStore::list_policy_proposals(self, status, limit)
    }
    fn get_policy_proposal(&self, id: i64) -> Result<Option<PolicyProposalRow>> {
        SqliteStore::get_policy_proposal(self, id)
    }
    fn review_policy_proposal(
        &self,
        id: i64,
        status: &str,
        reviewed_by: &str,
        note: Option<&str>,
    ) -> Result<()> {
        SqliteStore::review_policy_proposal(self, id, status, reviewed_by, note)
    }

    fn get_round_outliers(
        &self,
        agent_id: &str,
        since: DateTime<Utc>,
        threshold_multiplier: f64,
        limit: usize,
    ) -> Result<(RoundStats, Vec<TaskHistoryEntry>)> {
        SqliteStore::get_round_outliers(self, agent_id, since, threshold_multiplier, limit)
    }
    fn get_efficiency_trends(
        &self,
        agent_id: &str,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<Vec<EfficiencyTrendPoint>> {
        SqliteStore::get_efficiency_trends(self, agent_id, from, to)
    }

    fn save_workflow_instance(&self, row: &WorkflowInstanceRow) -> Result<()> {
        SqliteStore::save_workflow_instance(self, row)
    }
    fn get_workflow_instance(&self, id: &str) -> Result<Option<WorkflowInstanceRow>> {
        SqliteStore::get_workflow_instance(self, id)
    }
    fn get_workflow_instances(&self, ids: &[String]) -> Result<Vec<WorkflowInstanceRow>> {
        SqliteStore::get_workflow_instances(self, ids)
    }
    fn list_workflow_instances(
        &self,
        status: Option<&str>,
        workflow_id: Option<&str>,
    ) -> Result<Vec<WorkflowInstanceRow>> {
        SqliteStore::list_workflow_instances(self, status, workflow_id)
    }
    fn list_workflow_instances_filtered(
        &self,
        filter: &InstanceFilter,
    ) -> Result<Vec<WorkflowInstanceRow>> {
        SqliteStore::list_workflow_instances_filtered(self, filter)
    }
    fn list_child_instances(&self, parent_id: &str) -> Result<Vec<WorkflowInstanceRow>> {
        SqliteStore::list_child_instances(self, parent_id)
    }
    fn workflow_instances_after(
        &self,
        after: DateTime<Utc>,
        after_id: &str,
        limit: usize,
    ) -> Result<Vec<WorkflowInstanceRow>> {
        SqliteStore::workflow_instances_after(self, after, after_id, limit)
    }
    fn update_workflow_instance_status(
        &self,
        id: &str,
        status: &str,
        current_step_id: Option<&str>,
    ) -> Result<()> {
        SqliteStore::update_workflow_instance_status(self, id, status, current_step_id)
    }
    fn find_instance_by_thread(
        &self,
        channel_id: &str,
        thread_id: &str,
        statuses: &[&str],
    ) -> Result<Option<WorkflowInstanceRow>> {
        SqliteStore::find_instance_by_thread(self, channel_id, thread_id, statuses)
    }
    fn set_instance_thread(&self, id: &str, channel_id: &str, thread_id: &str) -> Result<()> {
        SqliteStore::set_instance_thread(self, id, channel_id, thread_id)
    }
    fn try_claim_workflow_instance(&self, id: &str, owner: &str, ttl_secs: i64) -> Result<bool> {
        SqliteStore::try_claim_workflow_instance(self, id, owner, ttl_secs)
    }
    fn renew_workflow_lease(&self, id: &str, owner: &str, ttl_secs: i64) -> Result<bool> {
        SqliteStore::renew_workflow_lease(self, id, owner, ttl_secs)
    }
    fn release_workflow_instance(&self, id: &str, owner: &str) -> Result<()> {
        SqliteStore::release_workflow_instance(self, id, owner)
    }
    fn save_workflow_instance_owned(&self, row: &WorkflowInstanceRow, owner: &str) -> Result<bool> {
        SqliteStore::save_workflow_instance_owned(self, row, owner)
    }
    fn save_workflow_instance_fenced(
        &self,
        row: &WorkflowInstanceRow,
        expected_version: i64,
    ) -> Result<bool> {
        SqliteStore::save_workflow_instance_fenced(self, row, expected_version)
    }
    fn cancel_workflow_instance(
        &self,
        id: &str,
        status: &str,
        reason_key: &str,
        reason: &str,
    ) -> Result<()> {
        SqliteStore::cancel_workflow_instance(self, id, status, reason_key, reason)
    }
    fn delete_workflow_instance(&self, id: &str) -> Result<bool> {
        SqliteStore::delete_workflow_instance(self, id)
    }
    fn identity_signals(&self, identity: &str) -> Result<IdentitySignals> {
        SqliteStore::identity_signals(self, identity)
    }
    fn identity_signal_holders(&self) -> Result<std::collections::BTreeMap<String, usize>> {
        SqliteStore::identity_signal_holders(self)
    }
    fn reassign_identity_signals(&self, from: &str, to: &str) -> Result<IdentitySignals> {
        SqliteStore::reassign_identity_signals(self, from, to)
    }
    fn forget_identity_signals(&self, identity: &str) -> Result<IdentitySignals> {
        SqliteStore::forget_identity_signals(self, identity)
    }
    fn reassign_identity_billing(&self, from: &str, to: &str) -> Result<usize> {
        SqliteStore::reassign_identity_billing(self, from, to)
    }

    fn query_activities(&self, query: &ActivityQuery) -> Result<Vec<ActivityRecord>> {
        SqliteStore::query_activities(self, query)
    }
    fn prune_activity(&self, before: DateTime<Utc>) -> Result<usize> {
        SqliteStore::prune_activity(self, before)
    }
    fn prune_costs(&self, before: DateTime<Utc>) -> Result<usize> {
        SqliteStore::prune_costs(self, before)
    }

    fn task_usage_rows(&self, from: DateTime<Utc>, to: DateTime<Utc>) -> Result<Vec<TaskUsageRow>> {
        SqliteStore::task_usage_rows(self, from, to)
    }
    fn daily_usage_rows(&self, since: DateTime<Utc>) -> Result<Vec<DailyUsageRow>> {
        SqliteStore::daily_usage_rows(self, since)
    }

    fn insert_security_event(&self, event: &SecurityEvent) -> Result<()> {
        SqliteStore::insert_security_event(self, event)
    }
    fn query_security_events(&self, filter: &SecurityEventFilter) -> Result<Vec<SecurityEventRow>> {
        SqliteStore::query_security_events(self, filter)
    }
    fn security_event_stats(&self, since: &str) -> Result<SecurityStats> {
        SqliteStore::security_event_stats(self, since)
    }
    fn cleanup_security_events(&self, before: &str) -> Result<usize> {
        SqliteStore::cleanup_security_events(self, before)
    }

    fn metered_cost_by_task_prefix(&self, prefix: &str) -> Result<f64> {
        SqliteStore::metered_cost_by_task_prefix(self, prefix)
    }
    fn metered_spend(&self, from: DateTime<Utc>, to: DateTime<Utc>) -> Result<Vec<MeteredSpend>> {
        SqliteStore::metered_spend(self, from, to)
    }

    // ─── Per-identity stats slices ───
    fn cost_by_identity(
        &self,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<Vec<(String, f64)>> {
        SqliteStore::cost_by_identity(self, from, to)
    }
    fn cost_token_history_by_identity_day(
        &self,
        identity_id: &str,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<Vec<TokenHistoryPoint>> {
        SqliteStore::cost_token_history_by_identity_day(self, identity_id, from, to)
    }
    fn cost_token_history_by_agent_day(
        &self,
        agent_id: &str,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<Vec<TokenHistoryPoint>> {
        SqliteStore::cost_token_history_by_agent_day(self, agent_id, from, to)
    }
    fn get_identity_usage_summary(
        &self,
        identity_id: &str,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
        group_by: &str,
    ) -> Result<Vec<AgentUsageSummary>> {
        SqliteStore::get_identity_usage_summary(self, identity_id, from, to, group_by)
    }
    fn compare_models_for_identity(
        &self,
        identity_id: &str,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<Vec<ModelComparison>> {
        SqliteStore::compare_models_for_identity(self, identity_id, from, to)
    }
    fn tool_breakdown_for_identity(
        &self,
        identity_id: &str,
        from: DateTime<Utc>,
        limit: usize,
    ) -> Result<Vec<ToolCostEntry>> {
        SqliteStore::tool_breakdown_for_identity(self, identity_id, from, limit)
    }
    fn agent_identity_breakdown(
        &self,
        agent_id: &str,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<Vec<(String, f64)>> {
        SqliteStore::agent_identity_breakdown(self, agent_id, from, to)
    }
    fn cost_by_task_type(
        &self,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<Vec<(String, f64)>> {
        SqliteStore::cost_by_task_type(self, from, to)
    }
    fn task_history_paginated(&self, filter: &TaskHistoryFilter) -> Result<TaskHistoryPage> {
        SqliteStore::task_history_paginated(self, filter)
    }
    fn get_task_trace(&self, task_id: &str) -> Result<Option<TaskTrace>> {
        SqliteStore::get_task_trace(self, task_id)
    }
    fn tool_breakdown_global(
        &self,
        from: DateTime<Utc>,
        limit: usize,
    ) -> Result<Vec<ToolCostEntry>> {
        SqliteStore::tool_breakdown_global(self, from, limit)
    }
    fn efficiency_trends_global(
        &self,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<Vec<EfficiencyTrendPoint>> {
        SqliteStore::efficiency_trends_global(self, from, to)
    }
    fn round_outliers_global(
        &self,
        since: DateTime<Utc>,
        threshold_multiplier: f64,
        limit: usize,
    ) -> Result<(RoundStats, Vec<TaskHistoryEntry>)> {
        SqliteStore::round_outliers_global(self, since, threshold_multiplier, limit)
    }
}

/// Behaviour every [`TelemetryStore`] backend must share, run against each.
#[cfg(test)]
pub(crate) mod conformance {
    use chrono::{DateTime, Duration, Utc};

    use super::TelemetryStore;
    use crate::persistence::{InstanceFilter, RunsAs, WorkflowInstanceRow};

    /// Every column of the signal tables that names a person is seeded for
    /// `alice`. A move and an erase each change exactly the rows a count
    /// counts and leave nothing counted behind; a move keeps the excerpts it
    /// carries, an erase clears them, and neither touches a billing field.
    pub(crate) fn identity_signals_count_exactly_what_a_move_and_an_erase_change(
        store: &dyn TelemetryStore,
    ) {
        use std::collections::BTreeMap;

        use super::{IdentitySignals, LessonEvent, RelationshipEvent};
        use crate::persistence::CostRecord;
        use crate::subscription::{Billing, CredentialTier};
        let now = Utc::now();
        let relationship = |identity: &str, message: &str| RelationshipEvent {
            timestamp: now,
            agent_id: "agent".into(),
            identity_id: identity.into(),
            session_id: "s".into(),
            message_id: message.into(),
            valence: 0.5,
            intensity: 0.5,
            emotion_label: None,
            reaction: "warm".into(),
            is_key_moment: false,
            confidence: 0.9,
            source: "llm".into(),
            signals: "{}".into(),
        };
        let cost = |identity: &str, payer: &str, description: Option<&str>, key: &str| CostRecord {
            agent_id: "agent".into(),
            model: "m".into(),
            cost_usd: 1.5,
            task_id: Some("t1".into()),
            identity_id: Some(identity.into()),
            description: description.map(String::from),
            billing: Billing::Subscription,
            payer: Some(payer.into()),
            credential_tier: Some(CredentialTier::GlobalUser),
            ledger_key: Some(key.into()),
            ..Default::default()
        };
        for (identity, message) in [("alice", "m1"), ("carol", "m2")] {
            store
                .record_relationship_event(&relationship(identity, message))
                .unwrap();
        }
        store
            .record_lesson_event(&LessonEvent::new(now, "agent", "inject").with_identity("alice"))
            .unwrap();
        for record in [
            cost("alice", "alice", Some("summarise my medical letters"), "k1"),
            cost("alice", "alice", None, "k2"),
            cost("carol", "alice", Some("carol's own words"), "k3"),
        ] {
            assert!(store.record_cost(&record).unwrap());
        }
        store
            .record_task_telemetry(
                &crate::persistence::TaskTelemetryRecord {
                    identity_id: Some("alice".into()),
                    description: Some("summarise my medical letters".into()),
                    ..task_completed("t1", now)
                },
                &[],
                &[],
            )
            .unwrap();
        let billing = || {
            let trace = store.get_task_trace("t1").unwrap().expect("t1 is recorded");
            let mut rows: Vec<_> = trace
                .cost_records
                .iter()
                .map(|row| {
                    (
                        row.identity_id.clone(),
                        row.payer.clone(),
                        row.credential_tier.map(CredentialTier::as_str),
                        row.cost_usd.to_bits(),
                    )
                })
                .collect();
            rows.sort();
            rows
        };
        let paid = |rows: &[(Option<String>, Option<String>, Option<&'static str>, u64)]| {
            let mut paid: Vec<_> = rows
                .iter()
                .map(|(_, payer, tier, usd)| (payer.clone(), *tier, *usd))
                .collect();
            paid.sort();
            paid
        };
        let before = billing();

        let alices = store.identity_signals("alice").unwrap();
        assert_eq!(
            alices,
            IdentitySignals {
                relationship_events: 1,
                lesson_events: 1,
                described_rows: 2,
            }
        );
        let carols = store.identity_signals("carol").unwrap();
        assert_eq!(
            store.identity_signal_holders().unwrap(),
            BTreeMap::from([
                ("alice".to_string(), alices.total()),
                ("carol".to_string(), carols.total()),
            ]),
            "the holders count what the per-identity count counts"
        );

        assert_eq!(
            store.reassign_identity_signals("alice", "bob").unwrap(),
            alices,
            "a move changes every row counted"
        );
        assert_eq!(
            store.identity_signals("alice").unwrap(),
            IdentitySignals::default()
        );
        assert_eq!(store.identity_signals("bob").unwrap(), alices);
        assert_eq!(store.identity_signals("carol").unwrap(), carols);
        assert_eq!(
            store.reassign_identity_signals("alice", "bob").unwrap(),
            IdentitySignals::default()
        );
        assert!(!store
            .identity_signal_holders()
            .unwrap()
            .contains_key("alice"));
        let trace = store.get_task_trace("t1").unwrap().expect("t1 is recorded");
        assert_eq!(
            trace.task.description.as_deref(),
            Some("summarise my medical letters"),
            "a move keeps the excerpt"
        );
        let moved = billing();
        assert_eq!(
            paid(&moved),
            paid(&before),
            "a move of signals touches no payer"
        );
        assert_eq!(
            moved.iter().map(|row| row.0.as_deref()).collect::<Vec<_>>(),
            [Some("alice"), Some("bob"), Some("carol")],
            "the described row went with alice"
        );

        let bobs = store.identity_signals("bob").unwrap();
        assert_eq!(store.forget_identity_signals("bob").unwrap(), bobs);
        assert_eq!(store.forget_identity_signals("carol").unwrap(), carols);
        assert!(store.identity_signal_holders().unwrap().is_empty());
        for who in ["alice", "bob", "carol"] {
            assert_eq!(
                store.identity_signals(who).unwrap(),
                IdentitySignals::default()
            );
        }
        assert_eq!(billing(), moved, "an erase keeps billing");
    }

    /// Every billing column naming a person is seeded for `alice`; merging
    /// her into `bob` renames each one and nothing else, and again renames
    /// nothing.
    pub(crate) fn identity_billing_follows_a_merged_person(store: &dyn TelemetryStore) {
        use crate::persistence::CostRecord;
        use crate::subscription::{Billing, CredentialTier};
        let now = Utc::now();
        let cost = |usd: f64, identity: &str, payer: &str| CostRecord {
            agent_id: "agent".into(),
            model: "m".into(),
            cost_usd: usd,
            task_id: Some("t1".into()),
            identity_id: Some(identity.into()),
            description: Some("their words".into()),
            billing: Billing::Subscription,
            payer: Some(payer.into()),
            credential_tier: Some(CredentialTier::GlobalUser),
            ledger_key: Some(format!("k{usd}")),
            ..Default::default()
        };
        for record in [
            cost(1.0, "alice", "alice"),
            cost(2.0, "carol", "alice"),
            cost(3.0, "alice", "team:ops"),
            cost(4.0, "carol", "carol"),
        ] {
            assert!(store.record_cost(&record).unwrap());
        }
        store
            .record_task_telemetry(
                &crate::persistence::TaskTelemetryRecord {
                    identity_id: Some("alice".into()),
                    description: Some("their words".into()),
                    ..task_completed("t1", now)
                },
                &[],
                &[],
            )
            .unwrap();
        let ledger = || {
            let trace = store.get_task_trace("t1").unwrap().expect("t1 is recorded");
            let mut rows: Vec<_> = trace
                .cost_records
                .iter()
                .map(|row| {
                    (
                        row.cost_usd.to_bits(),
                        row.identity_id.clone(),
                        row.payer.clone(),
                        row.credential_tier,
                    )
                })
                .collect();
            rows.sort();
            (rows, trace.task.identity_id)
        };
        let row = |usd: f64, identity: &str, payer: &str| {
            (
                usd.to_bits(),
                Some(identity.to_string()),
                Some(payer.to_string()),
                Some(CredentialTier::GlobalUser),
            )
        };
        let excerpts = |who: &str| store.identity_signals(who).unwrap().described_rows;
        let carried = excerpts("alice");

        assert_eq!(store.reassign_identity_billing("alice", "bob").unwrap(), 5);

        assert_eq!(
            ledger(),
            (
                vec![
                    row(1.0, "bob", "bob"),
                    row(2.0, "carol", "bob"),
                    row(3.0, "bob", "team:ops"),
                    row(4.0, "carol", "carol"),
                ],
                Some("bob".to_string())
            )
        );
        assert_eq!((excerpts("alice"), excerpts("bob")), (0, carried));
        assert_eq!(store.reassign_identity_billing("alice", "bob").unwrap(), 0);
        let spent = store
            .cost_by_identity(now - Duration::hours(1), now + Duration::hours(1))
            .unwrap();
        assert!(!spent.iter().any(|(who, _)| who == "alice"), "{spent:?}");
    }

    fn versioned_run(store: &dyn TelemetryStore, id: &str) -> WorkflowInstanceRow {
        store
            .save_workflow_instance(&WorkflowInstanceRow {
                id: id.into(),
                workflow_id: "w".into(),
                status: "paused".into(),
                step_history: "[]".into(),
                variables: "{}".into(),
                trigger_json: "{}".into(),
                created_at: Utc::now().to_rfc3339(),
                updated_at: Utc::now().to_rfc3339(),
                ..Default::default()
            })
            .unwrap();
        store.get_workflow_instance(id).unwrap().unwrap()
    }

    /// A fenced save applies only while the row is at the version it was read
    /// at, and moves the version on; a second save at that version is refused.
    pub(crate) fn a_fenced_save_applies_only_at_the_version_it_read(store: &dyn TelemetryStore) {
        let read = versioned_run(store, "f1");
        let first = WorkflowInstanceRow {
            status: "running".into(),
            ..read.clone()
        };
        let second = WorkflowInstanceRow {
            status: "cancelled".into(),
            ..read.clone()
        };
        assert!(store
            .save_workflow_instance_fenced(&first, read.version)
            .unwrap());
        assert!(!store
            .save_workflow_instance_fenced(&second, read.version)
            .unwrap());
        let now = store.get_workflow_instance("f1").unwrap().unwrap();
        assert_eq!(
            (now.status.as_str(), now.version),
            ("running", read.version + 1)
        );
        assert!(!store
            .save_workflow_instance_fenced(
                &WorkflowInstanceRow {
                    id: "absent".into(),
                    ..first
                },
                0
            )
            .unwrap());
    }

    /// The upsert, the owned save, the status write, the cancel and the
    /// thread write each move the version on; claiming, renewing and
    /// releasing the lease leave it alone.
    pub(crate) fn every_content_write_moves_the_version_and_the_lease_does_not(
        store: &dyn TelemetryStore,
    ) {
        let read = versioned_run(store, "v1");
        let version = || store.get_workflow_instance("v1").unwrap().unwrap().version;
        let mut seen = vec![version()];
        let mut step = |write: &dyn Fn()| {
            write();
            seen.push(version());
        };
        step(&|| {
            assert!(store
                .try_claim_workflow_instance("v1", "hub-a", 120)
                .unwrap())
        });
        step(&|| assert!(store.renew_workflow_lease("v1", "hub-a", 120).unwrap()));
        step(&|| store.save_workflow_instance(&read).unwrap());
        step(&|| assert!(store.save_workflow_instance_owned(&read, "hub-a").unwrap()));
        step(&|| store.release_workflow_instance("v1", "hub-a").unwrap());
        step(&|| {
            store
                .update_workflow_instance_status("v1", "running", Some("draft"))
                .unwrap()
        });
        step(&|| store.set_instance_thread("v1", "C1", "1.0").unwrap());
        step(&|| {
            store
                .cancel_workflow_instance("v1", "cancelled", "_failure_reason", "stopped")
                .unwrap()
        });
        let base = seen[0];
        assert_eq!(
            seen,
            [
                base,
                base,
                base,
                base + 1,
                base + 2,
                base + 2,
                base + 3,
                base + 4,
                base + 5
            ]
        );
    }

    /// A targeted cancel moves the version on, so a change read before it is
    /// refused rather than reviving the run.
    pub(crate) fn a_fenced_save_after_a_targeted_cancel_is_refused(store: &dyn TelemetryStore) {
        let read = versioned_run(store, "c1");
        store
            .cancel_workflow_instance("c1", "cancelled", "_failure_reason", "stopped")
            .unwrap();
        assert!(!store
            .save_workflow_instance_fenced(
                &WorkflowInstanceRow {
                    status: "running".into(),
                    ..read.clone()
                },
                read.version
            )
            .unwrap());
        assert_eq!(
            store.get_workflow_instance("c1").unwrap().unwrap().status,
            "cancelled"
        );
    }

    /// A deleted instance is gone; deleting it again deletes nothing.
    pub(crate) fn instances_are_deleted(store: &dyn TelemetryStore) {
        store
            .save_workflow_instance(&WorkflowInstanceRow {
                id: "i1".into(),
                workflow_id: "w".into(),
                status: "running".into(),
                step_history: "[]".into(),
                variables: "{}".into(),
                trigger_json: "{}".into(),
                created_at: Utc::now().to_rfc3339(),
                updated_at: Utc::now().to_rfc3339(),
                ..Default::default()
            })
            .unwrap();
        assert!(store.delete_workflow_instance("i1").unwrap());
        assert!(store.get_workflow_instance("i1").unwrap().is_none());
        assert!(!store.delete_workflow_instance("i1").unwrap());
    }

    /// A walk in pages visits every instance updated since a moment exactly
    /// once, oldest first, instances updated at the same moment included.
    pub(crate) fn instances_walked_in_pages(store: &dyn TelemetryStore) {
        let now = Utc::now();
        let at = |minutes: i64| now - Duration::minutes(minutes);
        for (id, minutes) in [
            ("e", 10),
            ("b", 30),
            ("a", 30),
            ("c", 30),
            ("d", 20),
            ("old", 90),
        ] {
            store
                .save_workflow_instance(&WorkflowInstanceRow {
                    id: id.into(),
                    workflow_id: "ticket-to-pr".into(),
                    status: "completed".into(),
                    step_history: "[]".into(),
                    variables: "{}".into(),
                    trigger_json: "{}".into(),
                    created_at: at(minutes).to_rfc3339(),
                    updated_at: at(minutes).to_rfc3339(),
                    ..Default::default()
                })
                .unwrap();
        }
        let mut walked = Vec::new();
        let (mut after, mut after_id) = (at(60), String::new());
        loop {
            let page = store.workflow_instances_after(after, &after_id, 2).unwrap();
            let Some(last) = page.last() else { break };
            assert!(page.len() <= 2);
            after = DateTime::parse_from_rfc3339(&last.updated_at)
                .unwrap()
                .with_timezone(&Utc);
            after_id = last.id.clone();
            walked.extend(page.into_iter().map(|row| row.id));
        }
        assert_eq!(walked, ["a", "b", "c", "d", "e"]);
        assert!(store
            .workflow_instances_after(now, "", 10)
            .unwrap()
            .is_empty());
    }

    /// The filtered listing honours ids, workflows (a bare id matching member
    /// copies but not a longer id), statuses, a since time, the agent a run
    /// executes as and the limit, newest first across stored time formats,
    /// and reads the lease back.
    pub(crate) fn filtered_instance_listing(store: &dyn TelemetryStore) {
        let now = Utc::now();
        let ago = |minutes: i64| (now - Duration::minutes(minutes)).to_rfc3339();
        for (id, workflow_id, status, minutes, agent) in [
            ("t1", "ticket-to-pr", "completed", 50, Some("atlas")),
            ("t2", "69116b82/ticket-to-pr", "running", 40, None),
            ("t3", "ticket-to-pr", "failed", 30, Some("quill")),
            ("r1", "pr-review", "running", 20, Some("atlas")),
            ("x1", "ticket-to-pr-backtest", "completed", 10, None),
        ] {
            store
                .save_workflow_instance(&WorkflowInstanceRow {
                    id: id.into(),
                    workflow_id: workflow_id.into(),
                    status: status.into(),
                    agent_id: agent.map(str::to_string),
                    step_history: "[]".into(),
                    variables: "{}".into(),
                    trigger_json: "{}".into(),
                    created_at: ago(minutes),
                    updated_at: ago(minutes),
                    ..Default::default()
                })
                .unwrap();
        }
        let every = InstanceFilter {
            limit: 200,
            ..Default::default()
        };
        let ids = |filter: InstanceFilter| -> Vec<String> {
            store
                .list_workflow_instances_filtered(&filter)
                .unwrap()
                .into_iter()
                .map(|row| row.id)
                .collect()
        };
        let texts = |values: &[&str]| Some(values.iter().map(|v| v.to_string()).collect());

        assert_eq!(ids(every.clone()), ["x1", "r1", "t3", "t2", "t1"]);
        assert_eq!(
            ids(InstanceFilter {
                workflows: texts(&["ticket-to-pr"]),
                ..every.clone()
            }),
            ["t3", "t2", "t1"]
        );
        assert_eq!(
            ids(InstanceFilter {
                statuses: texts(&["running", "failed"]),
                ..every.clone()
            }),
            ["r1", "t3", "t2"]
        );
        assert_eq!(
            ids(InstanceFilter {
                workflows: texts(&["ticket-to-pr"]),
                statuses: texts(&["running"]),
                ..every.clone()
            }),
            ["t2"]
        );
        assert_eq!(
            ids(InstanceFilter {
                since: Some(now - Duration::minutes(35)),
                ..every.clone()
            }),
            ["x1", "r1", "t3"]
        );
        assert_eq!(
            ids(InstanceFilter {
                ids: texts(&["t1", "r1", "gone"]),
                ..every.clone()
            }),
            ["r1", "t1"]
        );
        assert!(ids(InstanceFilter {
            ids: Some(Vec::new()),
            ..every.clone()
        })
        .is_empty());
        assert_eq!(
            ids(InstanceFilter {
                limit: 2,
                ..every.clone()
            }),
            ["x1", "r1"]
        );
        let as_sable = InstanceFilter {
            runs_as: Some(RunsAs {
                agent: "atlas".into(),
                declaring: vec!["ticket-to-pr".into(), "69116b82/ticket-to-pr".into()],
            }),
            ..every.clone()
        };
        assert_eq!(
            ids(as_sable.clone()),
            ["r1", "t2", "t1"],
            "a run recording another agent is not atlas's, though atlas declares its workflow"
        );
        assert_eq!(
            ids(InstanceFilter {
                limit: 2,
                ..as_sable
            }),
            ["r1", "t2"],
            "the limit counts only the agent's runs"
        );

        store
            .cancel_workflow_instance("t1", "cancelled", "_failure_reason", "stopped")
            .unwrap();
        assert_eq!(
            ids(InstanceFilter {
                since: Some(now - Duration::minutes(1)),
                ..every.clone()
            }),
            ["t1"],
            "a row stamped by the backend's own clock compares as an instant"
        );

        assert!(store
            .try_claim_workflow_instance("t2", "hub-a", 600)
            .unwrap());
        let rows = store
            .list_workflow_instances_filtered(&InstanceFilter {
                ids: texts(&["t2", "t3"]),
                ..every
            })
            .unwrap();
        let leased = rows.iter().find(|row| row.id == "t2").unwrap();
        assert_eq!(leased.owner_instance.as_deref(), Some("hub-a"));
        let expires = DateTime::parse_from_rfc3339(leased.lease_expires_at.as_deref().unwrap())
            .unwrap()
            .with_timezone(&Utc);
        assert!(
            expires > now + Duration::minutes(9) && expires < now + Duration::minutes(11),
            "{expires}"
        );
        let unleased = rows.iter().find(|row| row.id == "t3").unwrap();
        assert_eq!(
            (&unleased.owner_instance, &unleased.lease_expires_at),
            (&None, &None)
        );
        assert_eq!(
            store
                .get_workflow_instance("t2")
                .unwrap()
                .unwrap()
                .lease_expires_at,
            leased.lease_expires_at
        );
    }

    fn task_completed(
        task_id: &str,
        completed_at: DateTime<Utc>,
    ) -> crate::persistence::TaskTelemetryRecord {
        crate::persistence::TaskTelemetryRecord {
            agent_id: "agent".into(),
            model: "m".into(),
            provider: "anthropic".into(),
            input_tokens: 10,
            output_tokens: 5,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
            cost_usd: 0.01,
            rounds: 1,
            duration_ms: 100,
            tool_invocation_count: 1,
            success: true,
            error_type: None,
            retry_count: 0,
            task_type: None,
            task_id: Some(task_id.into()),
            started_at: completed_at.to_rfc3339(),
            completed_at: completed_at.to_rfc3339(),
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
    }

    /// Record `task_id`, completed at `completed_at`, with one round and
    /// one tool call.
    pub(crate) fn record_task(
        store: &dyn TelemetryStore,
        task_id: &str,
        completed_at: DateTime<Utc>,
    ) {
        use crate::persistence::{RoundTelemetryRecord, ToolTelemetryRecord};
        let round = RoundTelemetryRecord {
            round_number: 1,
            input_tokens: 10,
            output_tokens: 5,
            latency_ms: 50,
            cache_creation_tokens: 0,
            cache_read_tokens: 0,
            prompt_size_bytes: 100,
            response_size_bytes: 50,
            tool_count: 1,
            stop_reason: "end_turn".into(),
        };
        let tool = ToolTelemetryRecord {
            round_number: 1,
            tool_name: "shell.exec".into(),
            success: true,
            duration_ms: 10,
            policy_decision: "allow".into(),
            input_size_bytes: 10,
            output_size_bytes: 10,
            denial_reason: None,
        };
        store
            .record_task_telemetry(&task_completed(task_id, completed_at), &[round], &[tool])
            .unwrap();
    }

    /// A task past the window goes with its rounds and tool calls; one
    /// inside it is untouched.
    pub(crate) fn activity_is_pruned_by_completion(store: &dyn TelemetryStore) {
        let now = Utc::now();
        record_task(store, "old", now - Duration::days(500));
        record_task(store, "recent", now - Duration::days(10));

        assert_eq!(store.prune_activity(now - Duration::days(600)).unwrap(), 0);
        assert_eq!(store.prune_activity(now - Duration::days(400)).unwrap(), 1);
        assert!(store.get_task_trace("old").unwrap().is_none());
        let kept = store.get_task_trace("recent").unwrap().expect("kept");
        assert_eq!((kept.rounds.len(), kept.tools.len()), (1, 1));
        assert_eq!(store.prune_activity(now - Duration::days(400)).unwrap(), 0);
    }

    /// Costs past the window go, except a scheduled job's, whose lifetime
    /// spend its budget caps.
    pub(crate) fn costs_are_pruned_but_a_jobs_are_kept(store: &dyn TelemetryStore) {
        use crate::persistence::CostRecord;
        let cost = |task_id: Option<&str>, cost_usd: f64| CostRecord {
            agent_id: "agent".into(),
            model: "m".into(),
            cost_usd,
            task_id: task_id.map(str::to_string),
            ..Default::default()
        };
        store.record_cost(&cost(None, 1.0)).unwrap();
        store.record_cost(&cost(Some("turn-1"), 2.0)).unwrap();
        store
            .record_cost(&cost(Some("scheduler:job-1:1"), 4.0))
            .unwrap();
        let now = Utc::now();
        let window = (now - Duration::hours(1), now + Duration::hours(1));

        assert_eq!(store.prune_costs(now - Duration::hours(1)).unwrap(), 0);
        assert_eq!(store.total_cost(window.0, window.1).unwrap(), 7.0);
        assert_eq!(store.prune_costs(now + Duration::seconds(5)).unwrap(), 2);
        assert_eq!(store.total_cost(window.0, window.1).unwrap(), 4.0);
        assert_eq!(
            store
                .metered_cost_by_task_prefix("scheduler:job-1:")
                .unwrap(),
            4.0,
            "a job's lifetime spend is intact"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn a_prune_pass_stops_at_a_short_batch_or_its_bound() {
        let run = |sizes: &[usize]| {
            let mut sizes = sizes.iter().copied();
            let mut calls = 0;
            let removed = prune_in_batches(|| {
                calls += 1;
                Ok(sizes.next().unwrap_or(PRUNE_BATCH))
            })
            .unwrap();
            (calls, removed)
        };
        assert_eq!(run(&[PRUNE_BATCH, 7]), (2, PRUNE_BATCH + 7));
        assert_eq!(run(&[0]), (1, 0));
        assert_eq!(
            run(&[]),
            (PRUNE_PASS_BATCHES, PRUNE_PASS_BATCHES * PRUNE_BATCH),
            "a backlog waits for the next pass"
        );
    }

    /// Smoke test: an in-memory `SqliteStore` can be held as
    /// `Arc<dyn TelemetryStore>` and dispatched through the trait
    /// vtable end-to-end.
    #[test]
    fn sqlite_store_implements_telemetry_store() {
        let store: Arc<dyn TelemetryStore> = Arc::new(SqliteStore::in_memory().unwrap());

        // Empty store: every read returns sensible defaults.
        assert!(store.list_tasks(None).unwrap().is_empty());
        assert!(store.recent_audit(10).unwrap().is_empty());
        assert!(store.list_model_pricing().unwrap().is_empty());
        assert_eq!(
            store
                .total_cost(
                    chrono::Utc::now() - chrono::Duration::hours(1),
                    chrono::Utc::now(),
                )
                .unwrap(),
            0.0
        );
    }

    /// Round-trip a single audit entry through the trait surface to
    /// confirm writes also dispatch correctly.
    #[test]
    fn telemetry_store_audit_round_trip_through_trait() {
        let store: Arc<dyn TelemetryStore> = Arc::new(SqliteStore::in_memory().unwrap());
        store
            .record_audit(
                "test-actor",
                "test.action",
                "test-resource",
                Some("hello world"),
                AuditOutcome::Success,
            )
            .unwrap();
        let entries = store.recent_audit(10).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].actor, "test-actor");
        assert_eq!(entries[0].action, "test.action");
        assert_eq!(entries[0].detail.as_deref(), Some("hello world"));
    }
}
