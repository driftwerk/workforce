//! `/api/stats/*` endpoints — rich usage analytics for the UI.
//!
//! All handlers are thin: parse query params, authorize, fan out to
//! [`workforce_common::telemetry::TelemetryStore`] methods, and shape the
//! JSON response. Backend queries already exist in both SQLite and
//! Postgres impls; this module is the HTTP veneer.
//!
//! Auth model (see `api_handlers::identity::require_*`):
//! - `/identities/{id}*` — self OR admin/manager.
//! - everything else — admin/manager.
//!
//! Pagination uses opaque cursors from
//! [`workforce_common::pagination`]. Default page is 50, capped at 200.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use workforce_common::pagination::{clamp_limit, decode_cursor, deserialize_opt_usize};
use workforce_common::persistence::{
    AgentUsageSummary, BillingSplit, CostRecordView, CostScope, CostSplitRow, ModelComparison,
    RoundTelemetryRecord, TaskHistoryEntry, TaskHistoryFilter, TaskTelemetryRecord, ToolCostEntry,
    ToolTelemetryRecord,
};
use workforce_common::subscription::{plan_label, Billing, CredentialTier};
use workforce_common::telemetry::TelemetryStore;

use crate::api_handlers::identity::{require_admin_or_manager, require_self_or_admin};
use crate::gateway::GatewayState;

type ApiResult<T> = Result<Json<T>, (StatusCode, Json<serde_json::Value>)>;

fn err(status: StatusCode, msg: impl Into<String>) -> (StatusCode, Json<serde_json::Value>) {
    (status, Json(serde_json::json!({"error": msg.into()})))
}

fn require_telemetry(
    state: &GatewayState,
) -> Result<&Arc<dyn TelemetryStore>, (StatusCode, Json<serde_json::Value>)> {
    state.telemetry_store.as_ref().ok_or_else(|| {
        err(
            StatusCode::SERVICE_UNAVAILABLE,
            "Telemetry store not configured",
        )
    })
}

// ── Common query types ──────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct RangeQuery {
    /// ISO-8601 lower bound. Defaults to `now - 30d`.
    #[serde(default)]
    from: Option<DateTime<Utc>>,
    /// ISO-8601 upper bound. Defaults to `now`.
    #[serde(default)]
    to: Option<DateTime<Utc>>,
}

impl RangeQuery {
    fn resolve(self) -> (DateTime<Utc>, DateTime<Utc>) {
        let to = self.to.unwrap_or_else(Utc::now);
        let from = self.from.unwrap_or(to - Duration::days(30));
        (from, to)
    }
}

#[derive(Debug, Deserialize)]
pub struct PageQuery {
    #[serde(default)]
    cursor: Option<String>,
    #[serde(default, deserialize_with = "deserialize_opt_usize")]
    limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
pub struct RangePageQuery {
    #[serde(flatten)]
    range: RangeQuery,
    #[serde(flatten)]
    page: PageQuery,
}

#[derive(Debug, Deserialize)]
pub struct ToolsQuery {
    #[serde(flatten)]
    range: RangeQuery,
    #[serde(default, deserialize_with = "deserialize_opt_usize")]
    limit: Option<usize>,
}

// ── Response shapes ─────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct CursorPage<T> {
    pub entries: Vec<T>,
    pub next_cursor: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct GroupedRow {
    pub key: String,
    pub display_name: Option<String>,
    pub cost_usd: f64,
}

/// Window totals. `billing` splits every billable call in `cost_records` by
/// what paid for it. On the overview and cost breakdown `cost_usd` is that
/// same total, metered plus subscription; on an agent's or a person's stats
/// `cost_usd` sums their task runs, so `billing` is a total of its own.
#[derive(Debug, Serialize)]
pub struct TotalsSummary {
    pub task_count: u64,
    pub cost_usd: f64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub cache_read_input_tokens: u64,
    pub billing: BillingSplit,
}

#[derive(Debug, PartialEq, Serialize)]
pub struct AgentBillingRow {
    pub agent_id: String,
    pub display_name: Option<String>,
    pub split: BillingSplit,
}

/// `origin` is `None` for rows recorded before origins were.
#[derive(Debug, PartialEq, Serialize)]
pub struct OriginRow {
    pub origin: Option<String>,
    pub split: BillingSplit,
}

/// The list-price value of the calls one plan paid for, from one tier.
#[derive(Debug, PartialEq, Serialize)]
pub struct PayerRow {
    pub payer: String,
    /// Whose plan it is, as a person reads it ([`plan_label`]).
    pub plan: String,
    pub credential_tier: Option<CredentialTier>,
    pub cost_usd: f64,
    pub calls: u64,
}

#[derive(Debug, Serialize)]
pub struct OverviewResponse {
    pub window: WindowDescriptor,
    pub totals: TotalsSummary,
    pub by_identity: Vec<GroupedRow>,
    pub by_agent: Vec<GroupedRow>,
    pub by_model: Vec<GroupedRow>,
    pub by_provider: Vec<GroupedRow>,
    pub by_team: Vec<GroupedRow>,
    pub by_task_type: Vec<GroupedRow>,
    pub by_agent_billing: Vec<AgentBillingRow>,
    pub by_origin: Vec<OriginRow>,
    pub history: Vec<workforce_common::cost::TokenHistoryPoint>,
}

#[derive(Debug, Serialize)]
pub struct WindowDescriptor {
    pub from: DateTime<Utc>,
    pub to: DateTime<Utc>,
}

#[derive(Debug, Serialize)]
pub struct CostBreakdownResponse {
    pub window: WindowDescriptor,
    pub totals: TotalsSummary,
    pub by_identity: Vec<GroupedRow>,
    pub by_agent: Vec<GroupedRow>,
    pub by_model: Vec<GroupedRow>,
    pub by_provider: Vec<GroupedRow>,
    pub by_team: Vec<GroupedRow>,
    pub by_task_type: Vec<GroupedRow>,
    pub by_ticket: Vec<GroupedRow>,
    pub by_agent_billing: Vec<AgentBillingRow>,
    pub by_origin: Vec<OriginRow>,
    pub history: Vec<workforce_common::cost::TokenHistoryPoint>,
}

#[derive(Debug, Serialize)]
pub struct IdentityRef {
    pub id: String,
    pub display_name: String,
    pub email: Option<String>,
    pub role: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct IdentityStatsResponse {
    pub identity: IdentityRef,
    pub window: WindowDescriptor,
    pub totals: TotalsSummary,
    pub by_model: Vec<AgentUsageSummary>,
    pub by_task_type: Vec<AgentUsageSummary>,
    pub by_agent: Vec<AgentUsageSummary>,
    pub model_comparison: Vec<ModelComparison>,
    pub tool_breakdown: Vec<ToolCostEntry>,
    pub history: Vec<workforce_common::cost::TokenHistoryPoint>,
}

#[derive(Debug, Serialize)]
pub struct AgentStatsResponse {
    pub agent_id: String,
    pub window: WindowDescriptor,
    pub totals: TotalsSummary,
    pub by_model: Vec<AgentUsageSummary>,
    pub by_task_type: Vec<AgentUsageSummary>,
    pub by_identity: Vec<GroupedRow>,
    pub model_comparison: Vec<ModelComparison>,
    pub tool_breakdown: Vec<ToolCostEntry>,
    pub by_origin: Vec<OriginRow>,
    pub by_payer: Vec<PayerRow>,
    pub history: Vec<workforce_common::cost::TokenHistoryPoint>,
    pub round_outliers: Vec<TaskHistoryEntry>,
}

#[derive(Debug, Serialize)]
pub struct EfficiencyResponse {
    pub trends: Vec<workforce_common::persistence::EfficiencyTrendPoint>,
    pub outliers: Vec<TaskHistoryEntry>,
}

#[derive(Debug, Serialize)]
pub struct TaskTraceResponse {
    pub task: TaskTelemetryRecord,
    pub rounds: Vec<RoundTelemetryRecord>,
    pub tools: Vec<ToolTelemetryRecord>,
    pub cost_records: Vec<CostRecordView>,
    pub identity: Option<IdentityRef>,
}

// ── Helpers ─────────────────────────────────────────────────────────────

/// Window-wide totals from cost_records: cost + token sums + record count,
/// and the billing split of `split`, the window's cost rows. Cache totals
/// are only carried in the day-bucket history, so they're zeroed here — the
/// chart series is the source of truth.
fn totals_for_window(
    store: &dyn TelemetryStore,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
    split: &[CostSplitRow],
) -> Result<TotalsSummary, eyre::Report> {
    let total_cost = store.total_cost(from, to)?;
    let (input_tokens, output_tokens, count) = store.cost_token_totals(from, to)?;
    Ok(TotalsSummary {
        task_count: count as u64,
        cost_usd: total_cost,
        input_tokens,
        output_tokens,
        cache_creation_input_tokens: 0,
        cache_read_input_tokens: 0,
        billing: BillingSplit::of(split),
    })
}

/// Roll a per-model/per-task-type usage summary into window totals so
/// the per-identity / per-agent stat cards tie out with the chart
/// series — same source of truth.
fn totals_from_usage_rows(rows: &[AgentUsageSummary], split: &[CostSplitRow]) -> TotalsSummary {
    TotalsSummary {
        task_count: rows.iter().map(|r| r.total_tasks).sum(),
        cost_usd: rows.iter().map(|r| r.total_cost).sum(),
        input_tokens: rows.iter().map(|r| r.total_input_tokens).sum(),
        output_tokens: rows.iter().map(|r| r.total_output_tokens).sum(),
        cache_creation_input_tokens: rows.iter().map(|r| r.total_cache_created_tokens).sum(),
        cache_read_input_tokens: rows.iter().map(|r| r.total_cache_read_tokens).sum(),
        billing: BillingSplit::of(split),
    }
}

/// `rows` split by billing per `key`, in key order.
fn split_by<'a, K: Ord>(
    rows: impl IntoIterator<Item = &'a CostSplitRow>,
    key: impl Fn(&CostSplitRow) -> K,
) -> BTreeMap<K, BillingSplit> {
    rows.into_iter().fold(BTreeMap::new(), |mut groups, row| {
        let split = groups.entry(key(row)).or_default();
        *split = *split + BillingSplit::from(row);
        groups
    })
}

/// By most spend first, metered and subscription together.
fn by_agent_billing(state: &GatewayState, rows: &[CostSplitRow]) -> Vec<AgentBillingRow> {
    let split = split_by(rows, |row| row.agent_id.clone());
    let names = super::identity_display::resolve_agents(&state.registry, split.keys());
    let mut agents: Vec<AgentBillingRow> = split
        .into_iter()
        .map(|(agent_id, split)| AgentBillingRow {
            display_name: names.get(&agent_id).map(|agent| agent.display_name.clone()),
            agent_id,
            split,
        })
        .collect();
    agents.sort_by(|a, b| {
        b.split
            .total_cost_usd()
            .total_cmp(&a.split.total_cost_usd())
    });
    agents
}

fn by_origin(rows: &[CostSplitRow]) -> Vec<OriginRow> {
    split_by(rows, |row| row.origin.clone())
        .into_iter()
        .map(|(origin, split)| OriginRow { origin, split })
        .collect()
}

/// The subscription calls each plan paid for, per tier, by most value
/// first; `names` reads each payer.
fn by_payer(rows: &[CostSplitRow], names: &HashMap<String, String>) -> Vec<PayerRow> {
    let mut payers: Vec<PayerRow> = split_by(
        rows.iter()
            .filter(|row| row.billing == Billing::Subscription),
        |row| (row.payer.clone(), row.credential_tier),
    )
    .into_iter()
    .filter_map(|((payer, credential_tier), split)| {
        let payer = payer?;
        Some(PayerRow {
            plan: plan_label(
                Some(&payer),
                names.get(&payer).map(String::as_str),
                credential_tier,
            ),
            payer,
            credential_tier,
            cost_usd: split.subscription_cost_usd,
            calls: split.subscription_calls,
        })
    })
    .collect();
    payers.sort_by(|a, b| b.cost_usd.total_cmp(&a.cost_usd));
    payers
}

fn cost_split(
    store: &dyn TelemetryStore,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
    scope: CostScope<'_>,
) -> Result<Vec<CostSplitRow>, (StatusCode, Json<serde_json::Value>)> {
    store
        .cost_split(from, to, scope)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

/// Translate `cost_by_identity` rows into `GroupedRow` with hydrated
/// display names. Batches identity-store lookups so we hit the store
/// once per distinct UUID, not once per row.
fn hydrate_identities(state: &GatewayState, rows: Vec<(String, f64)>) -> Vec<GroupedRow> {
    super::identity_display::hydrate_identity_rows(state.identity_store.as_ref(), rows)
        .into_iter()
        .map(|(p, cost_usd)| GroupedRow {
            key: p.id,
            display_name: Some(p.display_name),
            cost_usd,
        })
        .collect()
}

/// Translate `cost_by_agent` rows into `GroupedRow` with the agent's
/// configured display name (falls back to the id).
fn hydrate_agents(state: &GatewayState, rows: Vec<(String, f64)>) -> Vec<GroupedRow> {
    super::identity_display::hydrate_agent_rows(&state.registry, rows)
        .into_iter()
        .map(|(a, cost_usd)| GroupedRow {
            key: a.id,
            display_name: Some(a.display_name),
            cost_usd,
        })
        .collect()
}

/// Slice an already-sorted Vec by integer-offset cursor. Used for the
/// list endpoints whose source SQL aggregates fully (no native LIMIT
/// pagination), where in-memory paging is bounded by identity / agent
/// cardinality and acceptable.
fn paginate_offset<T>(mut all: Vec<T>, page: PageQuery) -> CursorPage<T> {
    let limit = clamp_limit(page.limit);
    let offset: usize = page
        .cursor
        .as_deref()
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    let end = offset.saturating_add(limit).min(all.len());
    let next_cursor = (end < all.len()).then(|| end.to_string());
    let entries = all.drain(offset.min(all.len())..end).collect();
    CursorPage {
        entries,
        next_cursor,
    }
}

fn ungrouped(rows: Vec<(String, f64)>) -> Vec<GroupedRow> {
    rows.into_iter()
        .map(|(key, cost_usd)| GroupedRow {
            key,
            display_name: None,
            cost_usd,
        })
        .collect()
}

/// Resolve an identity for embedding into a stats response (carries email
/// and role on top of what [`identity_display::resolve_principal`] returns).
fn lookup_identity_ref(state: &GatewayState, identity_id: &str) -> IdentityRef {
    let parsed = Uuid::parse_str(identity_id).ok();
    let found = parsed.as_ref().and_then(|uid| {
        state
            .identity_store
            .as_ref()
            .and_then(|s| s.get_by_id(*uid).ok().flatten())
    });
    match found {
        Some(i) => IdentityRef {
            id: i.id.to_string(),
            display_name: i.display_name,
            email: i.email,
            role: Some(i.role.as_str().to_string()),
        },
        None => IdentityRef {
            id: identity_id.to_string(),
            display_name: identity_id.to_string(),
            email: None,
            role: None,
        },
    }
}

// ── Handlers ────────────────────────────────────────────────────────────

pub async fn get_overview(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    Query(range): Query<RangeQuery>,
) -> ApiResult<OverviewResponse> {
    require_admin_or_manager(&state, &headers)?;
    let store = require_telemetry(&state)?.clone();
    let (from, to) = range.resolve();

    let split = cost_split(store.as_ref(), from, to, CostScope::default())?;
    let totals = totals_for_window(store.as_ref(), from, to, &split)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let by_identity = hydrate_identities(
        &state,
        store
            .cost_by_identity(from, to)
            .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?,
    );
    let by_agent = hydrate_agents(
        &state,
        store
            .cost_by_agent(from, to)
            .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?,
    );
    let by_model = ungrouped(
        store
            .cost_by_model(from, to)
            .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?,
    );
    let by_provider = ungrouped(
        store
            .cost_by_provider(from, to)
            .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?,
    );
    let by_team = ungrouped(
        store
            .cost_by_team(from, to)
            .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?,
    );
    let by_task_type = ungrouped(
        store
            .cost_by_task_type(from, to)
            .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?,
    );
    let history = store
        .cost_token_history_by_day(from, to)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(OverviewResponse {
        window: WindowDescriptor { from, to },
        totals,
        by_identity,
        by_agent,
        by_model,
        by_provider,
        by_team,
        by_task_type,
        by_agent_billing: by_agent_billing(&state, &split),
        by_origin: by_origin(&split),
        history,
    }))
}

pub async fn get_cost_breakdown(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    Query(range): Query<RangeQuery>,
) -> ApiResult<CostBreakdownResponse> {
    require_admin_or_manager(&state, &headers)?;
    let store = require_telemetry(&state)?.clone();
    let (from, to) = range.resolve();

    let split = cost_split(store.as_ref(), from, to, CostScope::default())?;
    let totals = totals_for_window(store.as_ref(), from, to, &split)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let by_identity = hydrate_identities(
        &state,
        store
            .cost_by_identity(from, to)
            .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?,
    );
    let by_agent = hydrate_agents(
        &state,
        store
            .cost_by_agent(from, to)
            .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?,
    );
    let by_model = ungrouped(
        store
            .cost_by_model(from, to)
            .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?,
    );
    let by_provider = ungrouped(
        store
            .cost_by_provider(from, to)
            .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?,
    );
    let by_team = ungrouped(
        store
            .cost_by_team(from, to)
            .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?,
    );
    let by_task_type = ungrouped(
        store
            .cost_by_task_type(from, to)
            .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?,
    );
    let by_ticket: Vec<GroupedRow> = store
        .cost_by_tickets(from, to)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .into_iter()
        .map(|t| GroupedRow {
            key: t.ticket_id,
            display_name: None,
            cost_usd: t.total_cost_usd,
        })
        .collect();
    let history = store
        .cost_token_history_by_day(from, to)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(CostBreakdownResponse {
        window: WindowDescriptor { from, to },
        totals,
        by_identity,
        by_agent,
        by_model,
        by_provider,
        by_team,
        by_task_type,
        by_ticket,
        by_agent_billing: by_agent_billing(&state, &split),
        by_origin: by_origin(&split),
        history,
    }))
}

pub async fn list_identities(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    Query(q): Query<RangePageQuery>,
) -> ApiResult<CursorPage<GroupedRow>> {
    require_admin_or_manager(&state, &headers)?;
    let store = require_telemetry(&state)?.clone();
    let (from, to) = q.range.resolve();
    let rows = store
        .cost_by_identity(from, to)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(paginate_offset(
        hydrate_identities(&state, rows),
        q.page,
    )))
}

pub async fn list_agents(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    Query(q): Query<RangePageQuery>,
) -> ApiResult<CursorPage<GroupedRow>> {
    require_admin_or_manager(&state, &headers)?;
    let store = require_telemetry(&state)?.clone();
    let (from, to) = q.range.resolve();
    let rows = store
        .cost_by_agent(from, to)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(paginate_offset(hydrate_agents(&state, rows), q.page)))
}

pub async fn get_identity_stats(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    Path(identity_id): Path<String>,
    Query(range): Query<RangeQuery>,
) -> ApiResult<IdentityStatsResponse> {
    require_self_or_admin(&state, &headers, &identity_id)?;
    let store = require_telemetry(&state)?.clone();
    let (from, to) = range.resolve();

    let by_model = store
        .get_identity_usage_summary(&identity_id, from, to, "model")
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let by_task_type = store
        .get_identity_usage_summary(&identity_id, from, to, "task_type")
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let by_agent = store
        .get_identity_usage_summary(&identity_id, from, to, "agent")
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let model_comparison = store
        .compare_models_for_identity(&identity_id, from, to)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let tool_breakdown = store
        .tool_breakdown_for_identity(&identity_id, from, 50)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let history = store
        .cost_token_history_by_identity_day(&identity_id, from, to)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let split = cost_split(
        store.as_ref(),
        from,
        to,
        CostScope {
            user_id: Some(&identity_id),
            ..CostScope::default()
        },
    )?;
    let totals = totals_from_usage_rows(&by_model, &split);

    Ok(Json(IdentityStatsResponse {
        identity: lookup_identity_ref(&state, &identity_id),
        window: WindowDescriptor { from, to },
        totals,
        by_model,
        by_task_type,
        by_agent,
        model_comparison,
        tool_breakdown,
        history,
    }))
}

pub async fn get_identity_tasks(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    Path(identity_id): Path<String>,
    Query(q): Query<RangePageQuery>,
) -> ApiResult<CursorPage<TaskHistoryEntry>> {
    require_self_or_admin(&state, &headers, &identity_id)?;
    let store = require_telemetry(&state)?.clone();
    let (from, to) = q.range.resolve();
    let cursor = q
        .page
        .cursor
        .as_deref()
        .map(decode_cursor)
        .transpose()
        .map_err(|e| err(StatusCode::BAD_REQUEST, e.to_string()))?;
    let filter = TaskHistoryFilter {
        identity_id: Some(identity_id),
        since: Some(from),
        until: Some(to),
        cursor,
        limit: clamp_limit(q.page.limit),
        ..Default::default()
    };
    let page = store
        .task_history_paginated(&filter)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(CursorPage {
        entries: page.entries,
        next_cursor: page.next_cursor,
    }))
}

pub async fn get_agent_stats(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    Path(agent_id): Path<String>,
    Query(range): Query<RangeQuery>,
) -> ApiResult<AgentStatsResponse> {
    require_admin_or_manager(&state, &headers)?;
    let store = require_telemetry(&state)?.clone();
    let (from, to) = range.resolve();

    let by_model = store
        .get_agent_usage_summary(&agent_id, from, to, "model")
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let by_task_type = store
        .get_agent_usage_summary(&agent_id, from, to, "task_type")
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let by_identity = hydrate_identities(
        &state,
        store
            .agent_identity_breakdown(&agent_id, from, to)
            .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?,
    );
    let model_comparison = store
        .compare_models_for_agent(&agent_id, from, to)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let tool_breakdown = store
        .get_tool_cost_breakdown(&agent_id, from, 50)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let history = store
        .cost_token_history_by_agent_day(&agent_id, from, to)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let (_stats, round_outliers) = store
        .get_round_outliers(&agent_id, from, 2.0, 20)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let split = cost_split(
        store.as_ref(),
        from,
        to,
        CostScope {
            agent_id: Some(&agent_id),
            ..CostScope::default()
        },
    )?;
    let names = super::identity_display::resolve_payers(
        state.identity_store.as_ref(),
        &state.registry,
        split.iter().filter_map(|row| row.payer.as_deref()),
    );
    let totals = totals_from_usage_rows(&by_model, &split);
    let by_payer = by_payer(&split, &names);

    Ok(Json(AgentStatsResponse {
        agent_id,
        window: WindowDescriptor { from, to },
        totals,
        by_model,
        by_task_type,
        by_identity,
        model_comparison,
        tool_breakdown,
        by_origin: by_origin(&split),
        by_payer,
        history,
        round_outliers,
    }))
}

pub async fn get_agent_tasks(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    Path(agent_id): Path<String>,
    Query(q): Query<RangePageQuery>,
) -> ApiResult<CursorPage<TaskHistoryEntry>> {
    require_admin_or_manager(&state, &headers)?;
    let store = require_telemetry(&state)?.clone();
    let (from, to) = q.range.resolve();
    let cursor = q
        .page
        .cursor
        .as_deref()
        .map(decode_cursor)
        .transpose()
        .map_err(|e| err(StatusCode::BAD_REQUEST, e.to_string()))?;
    let filter = TaskHistoryFilter {
        agent_id: Some(agent_id),
        since: Some(from),
        until: Some(to),
        cursor,
        limit: clamp_limit(q.page.limit),
        ..Default::default()
    };
    let page = store
        .task_history_paginated(&filter)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(CursorPage {
        entries: page.entries,
        next_cursor: page.next_cursor,
    }))
}

pub async fn get_task_trace(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    Path(task_id): Path<String>,
) -> ApiResult<TaskTraceResponse> {
    let store = require_telemetry(&state)?.clone();
    let trace = store
        .get_task_trace(&task_id)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "Task not found"))?;

    // Self-or-admin: an identity can read their own task trace, admins
    // can read anyone's. Tasks with no identity (system) are admin-only.
    match trace.task.identity_id.as_deref() {
        Some(id) => require_self_or_admin(&state, &headers, id)?,
        None => require_admin_or_manager(&state, &headers)?,
    };

    let identity = trace
        .task
        .identity_id
        .as_deref()
        .map(|id| lookup_identity_ref(&state, id));

    Ok(Json(TaskTraceResponse {
        task: trace.task,
        rounds: trace.rounds,
        tools: trace.tools,
        cost_records: trace.cost_records,
        identity,
    }))
}

pub async fn get_models(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    Query(range): Query<RangeQuery>,
) -> ApiResult<Vec<GroupedRow>> {
    require_admin_or_manager(&state, &headers)?;
    let store = require_telemetry(&state)?.clone();
    let (from, to) = range.resolve();
    Ok(Json(ungrouped(store.cost_by_model(from, to).map_err(
        |e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    )?)))
}

pub async fn get_tools(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    Query(q): Query<ToolsQuery>,
) -> ApiResult<Vec<ToolCostEntry>> {
    require_admin_or_manager(&state, &headers)?;
    let store = require_telemetry(&state)?.clone();
    let (from, _to) = q.range.resolve();
    let limit = q.limit.unwrap_or(50).min(200);
    Ok(Json(store.tool_breakdown_global(from, limit).map_err(
        |e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    )?))
}

pub async fn get_efficiency(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    Query(range): Query<RangeQuery>,
) -> ApiResult<EfficiencyResponse> {
    require_admin_or_manager(&state, &headers)?;
    let store = require_telemetry(&state)?.clone();
    let (from, to) = range.resolve();
    let trends = store
        .efficiency_trends_global(from, to)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let (_stats, outliers) = store
        .round_outliers_global(from, 2.0, 20)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(EfficiencyResponse { trends, outliers }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use workforce_common::persistence::{CostRecord, SqliteStore};
    use workforce_common::subscription::{CredentialTier, Origin};

    fn split_row(
        billing: Billing,
        origin: Option<&str>,
        plan: Option<(&str, &str)>,
        calls: u64,
        cost_usd: f64,
    ) -> CostSplitRow {
        CostSplitRow {
            agent_id: "allan".into(),
            billing,
            origin: origin.map(String::from),
            payer: plan.map(|(payer, _)| payer.to_string()),
            credential_tier: plan.map(|(_, tier)| CredentialTier::from_column(tier).unwrap()),
            calls,
            cost_usd,
        }
    }

    #[test]
    fn totals_split_keeps_cost_usd_as_the_sum() {
        let store = SqliteStore::in_memory().unwrap();
        for (billing, cost_usd) in [(Billing::Metered, 0.25), (Billing::Subscription, 0.5)] {
            store
                .record_cost(&CostRecord {
                    agent_id: "allan".into(),
                    model: "gpt-6.1-sol".into(),
                    cost_usd,
                    billing,
                    payer: (billing == Billing::Subscription).then(|| "install".to_string()),
                    credential_tier: (billing == Billing::Subscription)
                        .then_some(CredentialTier::SystemShared),
                    ..Default::default()
                })
                .unwrap();
        }
        let (from, to) = (
            Utc::now() - Duration::hours(1),
            Utc::now() + Duration::hours(1),
        );
        let split = store.cost_split(from, to, CostScope::default()).unwrap();

        let totals = totals_for_window(&store, from, to, &split).unwrap();
        assert_eq!(totals.cost_usd, 0.75);
        assert_eq!(
            totals.billing,
            BillingSplit {
                metered_cost_usd: 0.25,
                subscription_cost_usd: 0.5,
                subscription_calls: 1,
            }
        );
    }

    #[test]
    fn pre_v020_rows_fold_to_unrecorded_origin() {
        let rows = [
            split_row(
                Billing::Metered,
                Some(Origin::Heartbeat.as_str()),
                None,
                1,
                0.25,
            ),
            split_row(Billing::Metered, None, None, 3, 1.0),
            split_row(
                Billing::Subscription,
                Some(Origin::Heartbeat.as_str()),
                Some(("install", "system_shared")),
                2,
                0.5,
            ),
        ];
        assert_eq!(
            by_origin(&rows),
            [
                OriginRow {
                    origin: None,
                    split: BillingSplit {
                        metered_cost_usd: 1.0,
                        ..BillingSplit::default()
                    },
                },
                OriginRow {
                    origin: Some("heartbeat".into()),
                    split: BillingSplit {
                        metered_cost_usd: 0.25,
                        subscription_cost_usd: 0.5,
                        subscription_calls: 2,
                    },
                },
            ]
        );
    }

    #[test]
    fn agent_billing_rows_by_most_spend_first() {
        let state = crate::remote_mcp::testing::fixture(false).state;
        let row = |agent_id: &str, billing, cost_usd| CostSplitRow {
            agent_id: agent_id.into(),
            ..split_row(billing, None, None, 1, cost_usd)
        };
        let rows = [
            row("allan", Billing::Metered, 0.5),
            row("bea", Billing::Metered, 0.25),
            row("bea", Billing::Subscription, 1.0),
            row("cal", Billing::Metered, 2.0),
        ];
        let order: Vec<String> = by_agent_billing(&state, &rows)
            .into_iter()
            .map(|row| row.agent_id)
            .collect();
        assert_eq!(order, ["cal", "bea", "allan"]);
    }

    #[test]
    fn payer_rows_sum_each_plan_and_tier_by_most_value() {
        let rows = [
            split_row(Billing::Metered, None, None, 9, 9.0),
            split_row(
                Billing::Subscription,
                Some("interactive"),
                Some(("alice", "global_user")),
                1,
                0.25,
            ),
            split_row(
                Billing::Subscription,
                Some("heartbeat"),
                Some(("alice", "global_user")),
                2,
                0.25,
            ),
            split_row(
                Billing::Subscription,
                Some("heartbeat"),
                Some(("install", "system_shared")),
                1,
                1.0,
            ),
        ];
        let names = HashMap::from([("install".to_string(), "Install login".to_string())]);
        assert_eq!(
            by_payer(&rows, &names),
            [
                PayerRow {
                    payer: "install".into(),
                    plan: "Install login".into(),
                    credential_tier: Some(CredentialTier::SystemShared),
                    cost_usd: 1.0,
                    calls: 1,
                },
                PayerRow {
                    payer: "alice".into(),
                    plan: "alice's plan".into(),
                    credential_tier: Some(CredentialTier::GlobalUser),
                    cost_usd: 0.5,
                    calls: 3,
                },
            ]
        );
    }
}
