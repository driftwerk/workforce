//! HTTP handlers for per-agent self-learning: learning plans (file-backed),
//! curated knowledge items, and the deep-expertise knowledge tree (both
//! store-backed) with their human-review transitions.
//!
//! Routes (registered in [`crate::gateway::router`]):
//!
//! ```text
//! GET    /api/agents/{id}/self-learning/plans
//! POST   /api/agents/{id}/self-learning/plans                 json: id, name, schedule…, body
//! GET    /api/agents/{id}/self-learning/plans/{plan_id}
//! DELETE /api/agents/{id}/self-learning/plans/{plan_id}
//! POST   /api/agents/{id}/self-learning/plans/{plan_id}/run
//! GET    /api/agents/{id}/self-learning/items?status=&plan=&topic=
//! POST   /api/agents/{id}/self-learning/items/{item_id}/approve
//! POST   /api/agents/{id}/self-learning/items/{item_id}/reject    json: note?
//! POST   /api/agents/{id}/self-learning/items/{item_id}/reverify
//! DELETE /api/agents/{id}/self-learning/items/{item_id}
//! GET    /api/agents/{id}/deep-expertise/nodes?plan=&status=&parent=
//! GET    /api/agents/{id}/deep-expertise/nodes/{node_id}
//! POST   /api/agents/{id}/deep-expertise/nodes/{node_id}/approve
//! POST   /api/agents/{id}/deep-expertise/nodes/{node_id}/reject  json: note?
//! DELETE /api/agents/{id}/deep-expertise/nodes/{node_id}
//! ```
//!
//! Auth: every route requires the owner token via
//! [`crate::gateway::require_admin_action`] — self-learning writes are admin
//! actions over org-wide agent state. The URL agent scope is the auth scope:
//! item handlers go through [`crate::self_learning::review`] and node handlers
//! through [`crate::self_learning::review_node`], both of which refuse any
//! item/node whose `agent_id` differs from the path.

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;
use tracing::{info, warn};
use uuid::Uuid;

use workforce_common::learning_plan::{self, CurationMode, LearningPlan, LearningPlanMeta};
use workforce_common::settings::SelfLearningConfig;
use workforce_memory::store::MemoryStore;
use workforce_self_learning::{
    DeepExpertiseStore, DeepNode, ItemQuery, ItemStatus, KnowledgeItem, NodeParent, NodeQuery,
    NodeStatus, SelfLearningStore,
};

use crate::gateway::{require_admin_action, GatewayState, WsQuery};
use crate::self_learning::{review, review_node};
use crate::serve::AgentService;

type ApiError = (StatusCode, Json<serde_json::Value>);
type ApiResult<T> = Result<Json<T>, ApiError>;

// ── plans ────────────────────────────────────────────────────────────

/// `GET /api/agents/{id}/self-learning/plans`
pub async fn list_plans(
    Path(agent_id): Path<String>,
    Query(query): Query<WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> ApiResult<serde_json::Value> {
    let _ = require_admin_action(&state, &headers, query.token.as_deref()).await?;
    let service = resolve_agent(&state, &agent_id)?;

    let plans = learning_plan::list_learning_plans(&service.workspace)
        .map_err(|e| internal_err(&format!("list plans failed: {e}")))?;
    let last_runs = crate::self_learning::runner::plan_last_runs(&service.workspace);
    let counts = service
        .self_learning_store()
        .map(|store| item_counts(store.as_ref(), &agent_id))
        .unwrap_or_default();
    let nodes = service
        .deep_expertise_store()
        .map(|store| node_counts(store.as_ref(), &agent_id))
        .unwrap_or_default();

    let mut out = Vec::with_capacity(plans.len());
    for p in &plans {
        out.push(plan_to_json(
            p,
            last_runs.get(&p.id),
            counts.get(&p.id),
            nodes.get(&p.id).copied().unwrap_or_default(),
            plan_estimate(&service, p).await,
        ));
    }
    Ok(Json(serde_json::json!({ "plans": out })))
}

#[derive(Deserialize)]
pub struct PlanUpsert {
    id: String,
    #[serde(flatten)]
    meta: LearningPlanMeta,
    #[serde(default)]
    body: String,
}

/// `POST /api/agents/{id}/self-learning/plans` — create or replace a plan.
pub async fn upsert_plan(
    Path(agent_id): Path<String>,
    Query(query): Query<WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
    Json(req): Json<PlanUpsert>,
) -> ApiResult<serde_json::Value> {
    let _ = require_admin_action(&state, &headers, query.token.as_deref()).await?;
    let service = resolve_agent(&state, &agent_id)?;

    // Preserve a legacy `mode = "items"` plan's mode on edit: the FE omits
    // `mode`, and `resolved_mode()` defaults absent → `DeepExpertise`, so a
    // round-trip would silently flip an existing items plan. An explicit mode in
    // the request always wins; a brand-new plan with no mode keeps the default.
    let existing = learning_plan::load_learning_plan(&service.workspace, &req.id)
        .ok()
        .flatten();
    let mut meta = req.meta;
    meta.mode = resolve_upsert_mode(meta.mode, existing.as_ref());
    let plan = LearningPlan {
        id: req.id,
        meta,
        body: req.body,
    };
    learning_plan::write_learning_plan(&service.workspace, &plan)
        .map_err(|e| bad_request_err(&format!("write plan failed: {e}")))?;
    info!(agent = %agent_id, plan = %plan.id, "self-learning: plan saved");
    let estimate = plan_estimate(&service, &plan).await;
    Ok(Json(plan_to_json(&plan, None, None, 0, estimate)))
}

/// `GET /api/agents/{id}/self-learning/plans/{plan_id}`
pub async fn get_plan(
    Path((agent_id, plan_id)): Path<(String, String)>,
    Query(query): Query<WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> ApiResult<serde_json::Value> {
    let _ = require_admin_action(&state, &headers, query.token.as_deref()).await?;
    let service = resolve_agent(&state, &agent_id)?;

    let plan = learning_plan::load_learning_plan(&service.workspace, &plan_id)
        .map_err(|e| bad_request_err(&format!("load plan failed: {e}")))?
        .ok_or_else(|| not_found_err(&format!("no plan `{plan_id}` for agent `{agent_id}`")))?;
    let last_runs = crate::self_learning::runner::plan_last_runs(&service.workspace);
    let counts = service
        .self_learning_store()
        .map(|store| item_counts(store.as_ref(), &agent_id))
        .unwrap_or_default();
    let nodes = service
        .deep_expertise_store()
        .map(|store| node_counts(store.as_ref(), &agent_id))
        .unwrap_or_default();
    let estimate = plan_estimate(&service, &plan).await;
    Ok(Json(plan_to_json(
        &plan,
        last_runs.get(&plan.id),
        counts.get(&plan.id),
        nodes.get(&plan.id).copied().unwrap_or_default(),
        estimate,
    )))
}

/// `DELETE /api/agents/{id}/self-learning/plans/{plan_id}`
pub async fn delete_plan(
    Path((agent_id, plan_id)): Path<(String, String)>,
    Query(query): Query<WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> ApiResult<serde_json::Value> {
    let _ = require_admin_action(&state, &headers, query.token.as_deref()).await?;
    let service = resolve_agent(&state, &agent_id)?;

    let removed = learning_plan::delete_learning_plan(&service.workspace, &plan_id)
        .map_err(|e| bad_request_err(&format!("delete plan failed: {e}")))?;
    if !removed {
        return Err(not_found_err(&format!("no plan `{plan_id}`")));
    }
    info!(agent = %agent_id, plan = %plan_id, "self-learning: plan deleted");
    Ok(Json(
        serde_json::json!({ "agent_id": agent_id, "plan": plan_id }),
    ))
}

/// `POST /api/agents/{id}/self-learning/plans/{plan_id}/run` — curate now.
pub async fn run_plan(
    Path((agent_id, plan_id)): Path<(String, String)>,
    Query(query): Query<WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> ApiResult<serde_json::Value> {
    let _ = require_admin_action(&state, &headers, query.token.as_deref()).await?;
    let service = resolve_agent(&state, &agent_id)?;
    if !service.self_learning_enabled() {
        return Err(service_unavailable_err(
            "self-learning is disabled for this agent",
        ));
    }
    learning_plan::load_learning_plan(&service.workspace, &plan_id)
        .map_err(|e| bad_request_err(&format!("load plan failed: {e}")))?
        .ok_or_else(|| not_found_err(&format!("no plan `{plan_id}`")))?;

    let handle = service
        .self_learning_handle
        .read()
        .clone()
        .ok_or_else(|| service_unavailable_err("self-learning runner not started"))?;
    let queued = handle.wake_plan(plan_id.clone()).await;
    Ok(Json(
        serde_json::json!({ "plan": plan_id, "queued": queued }),
    ))
}

// ── items ────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct ItemFilter {
    token: Option<String>,
    status: Option<String>,
    plan: Option<String>,
    topic: Option<String>,
}

/// `GET /api/agents/{id}/self-learning/items`
pub async fn list_items(
    Path(agent_id): Path<String>,
    Query(filter): Query<ItemFilter>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> ApiResult<serde_json::Value> {
    let _ = require_admin_action(&state, &headers, filter.token.as_deref()).await?;
    let service = resolve_agent(&state, &agent_id)?;
    let Some(store) = service.self_learning_store() else {
        return Ok(Json(serde_json::json!({ "items": [] })));
    };

    let query = ItemQuery {
        plan_id: filter.plan,
        topic: filter.topic,
        status: filter.status.as_deref().map(ItemStatus::parse),
        limit: None,
    };
    let items = store
        .list_items(&agent_id, &query)
        .map_err(|e| internal_err(&format!("list items failed: {e}")))?;
    let out: Vec<_> = items.iter().map(item_to_json).collect();
    Ok(Json(serde_json::json!({ "items": out })))
}

#[derive(Deserialize, Default)]
pub struct RejectBody {
    #[serde(default)]
    note: Option<String>,
}

/// `POST /api/agents/{id}/self-learning/items/{item_id}/approve`
pub async fn approve_item(
    Path((agent_id, item_id)): Path<(String, Uuid)>,
    Query(query): Query<WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> ApiResult<serde_json::Value> {
    let caller = require_admin_action(&state, &headers, query.token.as_deref()).await?;
    let service = resolve_agent(&state, &agent_id)?;
    let (store, memory, cfg) = require_self_learning(&service).await?;

    let item = review::approve(
        store.as_ref(),
        memory.as_ref(),
        &cfg,
        &agent_id,
        item_id,
        &reviewer(&caller),
    )
    .await
    .map_err(review_err)?;
    Ok(Json(item_to_json(&item)))
}

/// `POST /api/agents/{id}/self-learning/items/{item_id}/reject`
pub async fn reject_item(
    Path((agent_id, item_id)): Path<(String, Uuid)>,
    Query(query): Query<WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
    body: Option<Json<RejectBody>>,
) -> ApiResult<serde_json::Value> {
    let caller = require_admin_action(&state, &headers, query.token.as_deref()).await?;
    let service = resolve_agent(&state, &agent_id)?;
    let (store, memory, _) = require_self_learning(&service).await?;

    let note = body.and_then(|Json(b)| b.note);
    let item = review::reject(
        store.as_ref(),
        memory.as_ref(),
        &agent_id,
        item_id,
        &reviewer(&caller),
        note,
    )
    .await
    .map_err(review_err)?;
    Ok(Json(item_to_json(&item)))
}

/// `POST /api/agents/{id}/self-learning/items/{item_id}/reverify`
pub async fn reverify_item(
    Path((agent_id, item_id)): Path<(String, Uuid)>,
    Query(query): Query<WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> ApiResult<serde_json::Value> {
    let caller = require_admin_action(&state, &headers, query.token.as_deref()).await?;
    let service = resolve_agent(&state, &agent_id)?;
    let (store, memory, _) = require_self_learning(&service).await?;

    let item = review::mark_for_reverify(
        store.as_ref(),
        memory.as_ref(),
        &agent_id,
        item_id,
        &reviewer(&caller),
    )
    .await
    .map_err(review_err)?;
    // Nudge the owning plan so it re-checks the claim against source soon. Bind
    // the handle first so the parking_lot guard is dropped before the await (it
    // is not Send and must not cross an await point).
    let handle = service.self_learning_handle.read().clone();
    if let Some(handle) = handle {
        handle.wake_plan(item.plan_id.clone()).await;
    }
    Ok(Json(item_to_json(&item)))
}

/// `DELETE /api/agents/{id}/self-learning/items/{item_id}`
pub async fn delete_item(
    Path((agent_id, item_id)): Path<(String, Uuid)>,
    Query(query): Query<WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> ApiResult<serde_json::Value> {
    let _ = require_admin_action(&state, &headers, query.token.as_deref()).await?;
    let service = resolve_agent(&state, &agent_id)?;
    let (store, memory, _) = require_self_learning(&service).await?;

    review::delete(store.as_ref(), memory.as_ref(), &agent_id, item_id)
        .await
        .map_err(review_err)?;
    info!(agent = %agent_id, item = %item_id, "self-learning: item deleted");
    Ok(Json(
        serde_json::json!({ "agent_id": agent_id, "item": item_id }),
    ))
}

// ── deep-expertise nodes ─────────────────────────────────────────────

#[derive(Deserialize)]
pub struct NodeFilter {
    token: Option<String>,
    status: Option<String>,
    plan: Option<String>,
    parent: Option<String>,
}

/// `GET /api/agents/{id}/deep-expertise/nodes` — list nodes one tree level at a
/// time: `parent` absent ⇒ the roots, `parent=<uuid>` ⇒ that node's children.
pub async fn list_nodes(
    Path(agent_id): Path<String>,
    Query(filter): Query<NodeFilter>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> ApiResult<serde_json::Value> {
    let _ = require_admin_action(&state, &headers, filter.token.as_deref()).await?;
    let service = resolve_agent(&state, &agent_id)?;
    let Some(store) = service.deep_expertise_store() else {
        return Ok(Json(serde_json::json!({ "nodes": [] })));
    };

    let parent = match filter.parent.as_deref() {
        None => NodeParent::Roots,
        Some(raw) => NodeParent::Of(
            Uuid::parse_str(raw)
                .map_err(|_| bad_request_err(&format!("invalid parent `{raw}`")))?,
        ),
    };
    let query = NodeQuery {
        plan_id: filter.plan,
        parent,
        status: filter.status.as_deref().map(NodeStatus::parse),
        limit: None,
    };
    let nodes = store
        .list_nodes(&agent_id, &query)
        .map_err(|e| internal_err(&format!("list nodes failed: {e}")))?;
    let out: Vec<_> = nodes.iter().map(node_to_json).collect();
    Ok(Json(serde_json::json!({ "nodes": out })))
}

/// `GET /api/agents/{id}/deep-expertise/nodes/{node_id}` — the node and its
/// direct children.
pub async fn get_node(
    Path((agent_id, node_id)): Path<(String, Uuid)>,
    Query(query): Query<WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> ApiResult<serde_json::Value> {
    let _ = require_admin_action(&state, &headers, query.token.as_deref()).await?;
    let service = resolve_agent(&state, &agent_id)?;
    let store = require_deep_expertise(&service).await?;

    let (node, children) =
        review_node::node_with_children(store.as_ref(), &agent_id, node_id).map_err(review_err)?;
    Ok(Json(serde_json::json!({
        "node": node_to_json(&node),
        "children": children.iter().map(node_to_json).collect::<Vec<_>>(),
    })))
}

/// `POST /api/agents/{id}/deep-expertise/nodes/{node_id}/approve`
pub async fn approve_node(
    Path((agent_id, node_id)): Path<(String, Uuid)>,
    Query(query): Query<WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> ApiResult<serde_json::Value> {
    let caller = require_admin_action(&state, &headers, query.token.as_deref()).await?;
    let service = resolve_agent(&state, &agent_id)?;
    let store = require_deep_expertise(&service).await?;

    let node = review_node::set_status(
        store.as_ref(),
        &agent_id,
        node_id,
        NodeStatus::Approved,
        &reviewer(&caller),
        None,
    )
    .map_err(review_err)?;
    Ok(Json(node_to_json(&node)))
}

/// `POST /api/agents/{id}/deep-expertise/nodes/{node_id}/reject`
pub async fn reject_node(
    Path((agent_id, node_id)): Path<(String, Uuid)>,
    Query(query): Query<WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
    body: Option<Json<RejectBody>>,
) -> ApiResult<serde_json::Value> {
    let caller = require_admin_action(&state, &headers, query.token.as_deref()).await?;
    let service = resolve_agent(&state, &agent_id)?;
    let store = require_deep_expertise(&service).await?;

    let note = body.and_then(|Json(b)| b.note);
    let node = review_node::set_status(
        store.as_ref(),
        &agent_id,
        node_id,
        NodeStatus::Rejected,
        &reviewer(&caller),
        note,
    )
    .map_err(review_err)?;
    Ok(Json(node_to_json(&node)))
}

/// `DELETE /api/agents/{id}/deep-expertise/nodes/{node_id}` — delete the node
/// and its whole subtree.
pub async fn delete_node(
    Path((agent_id, node_id)): Path<(String, Uuid)>,
    Query(query): Query<WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> ApiResult<serde_json::Value> {
    let _ = require_admin_action(&state, &headers, query.token.as_deref()).await?;
    let service = resolve_agent(&state, &agent_id)?;
    let store = require_deep_expertise(&service).await?;

    let deleted =
        review_node::delete_subtree(store.as_ref(), &agent_id, node_id).map_err(review_err)?;
    info!(agent = %agent_id, node = %node_id, deleted, "deep-expertise: subtree deleted");
    Ok(Json(serde_json::json!({
        "agent_id": agent_id,
        "node": node_id,
        "deleted": deleted,
    })))
}

// ── helpers ──────────────────────────────────────────────────────────

async fn require_self_learning(
    service: &Arc<AgentService>,
) -> Result<
    (
        Arc<dyn SelfLearningStore>,
        Arc<dyn MemoryStore>,
        SelfLearningConfig,
    ),
    ApiError,
> {
    if !service.self_learning_enabled() {
        return Err(service_unavailable_err(
            "self-learning is disabled for this agent",
        ));
    }
    crate::self_learning::ensure_store(service).await;
    let store = service
        .self_learning_store()
        .ok_or_else(|| service_unavailable_err("self-learning store not configured"))?;
    let memory = service
        .memory_store
        .clone()
        .ok_or_else(|| service_unavailable_err("memory store not configured"))?;
    Ok((store, memory, service.effective_self_learning()))
}

/// Resolve the deep-expertise tree store, opening it on demand. Unlike
/// [`require_self_learning`] this needs no Memory handle: node review is a pure
/// store transition (the recall surface retires hits whose node is gone).
async fn require_deep_expertise(
    service: &Arc<AgentService>,
) -> Result<Arc<dyn DeepExpertiseStore>, ApiError> {
    if !service.self_learning_enabled() {
        return Err(service_unavailable_err(
            "self-learning is disabled for this agent",
        ));
    }
    crate::self_learning::ensure_store(service).await;
    service
        .deep_expertise_store()
        .ok_or_else(|| service_unavailable_err("deep-expertise store not configured"))
}

fn reviewer(caller: &crate::gateway::CallerContext) -> String {
    caller
        .identity_id
        .clone()
        .unwrap_or_else(|| "owner-token".to_string())
}

/// Per-plan item counts (total / provisional / approved) for the plan listing.
fn item_counts(
    store: &dyn SelfLearningStore,
    agent_id: &str,
) -> std::collections::HashMap<String, (usize, usize, usize)> {
    let mut counts: std::collections::HashMap<String, (usize, usize, usize)> =
        std::collections::HashMap::new();
    let Ok(items) = store.list_items(agent_id, &ItemQuery::default()) else {
        return counts;
    };
    for item in items {
        let entry = counts.entry(item.plan_id).or_default();
        entry.0 += 1;
        match item.status {
            ItemStatus::Provisional => entry.1 += 1,
            ItemStatus::Approved => entry.2 += 1,
            _ => {}
        }
    }
    counts
}

/// Per-plan deep-expertise node counts for the plan listing — the tree-mode
/// counterpart to [`item_counts`].
fn node_counts(
    store: &dyn DeepExpertiseStore,
    agent_id: &str,
) -> std::collections::HashMap<String, usize> {
    let mut counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let Ok(nodes) = store.list_nodes(agent_id, &NodeQuery::default()) else {
        return counts;
    };
    for node in nodes {
        *counts.entry(node.plan_id).or_default() += 1;
    }
    counts
}

/// Projected plan cost from its cadence and this plan's own run history
/// (`task_type = "self_learning:{id}"`), for the plan editor. `None` when
/// budgets are disabled or the plan is run-once.
async fn plan_estimate(
    service: &Arc<AgentService>,
    plan: &LearningPlan,
) -> Option<serde_json::Value> {
    let budget = &service.config.budget;
    if !budget.enabled {
        return None;
    }
    let schedule = match (&plan.meta.schedule, plan.meta.interval_secs) {
        (Some(expr), _) => crate::hub_core::schedules::Schedule::Cron {
            expr: expr.clone(),
            tz: None,
        },
        (None, Some(secs)) => crate::hub_core::schedules::Schedule::Every {
            every_secs: secs,
            anchor: None,
        },
        (None, None) => return None,
    };
    let runs_per_day = crate::hub_core::schedules::runs_per_day(
        &schedule,
        &chrono::Utc::now(),
        service.config.user_timezone(),
    );
    let forecaster = crate::cost_forecast::CostForecaster::new(
        Arc::clone(&service.telemetry_store),
        budget.warn_turn_usd,
    );
    let per_run = forecaster
        .forecast(
            service.agent_id.as_str(),
            Some(&format!(
                "{}{}",
                crate::self_learning::TASK_LABEL_PREFIX,
                plan.id
            )),
            plan.meta
                .model
                .as_deref()
                .unwrap_or(&service.config.llm.default_model),
        )
        .await
        .ok()?;
    let projected = per_run.recurring(runs_per_day);
    let round2 = crate::cost_forecast::round2;
    Some(serde_json::json!({
        "per_run_usd": round2(projected.per_run_usd),
        "runs_per_day": (projected.runs_per_day * 10.0).round() / 10.0,
        "per_day_usd": round2(projected.per_day_usd),
        "per_week_usd": round2(projected.per_day_usd * 7.0),
        "basis": per_run.basis,
        "warn": projected.per_day_usd > budget.confirm_recurring_daily_usd,
    }))
}

fn plan_to_json(
    plan: &LearningPlan,
    last_run: Option<&chrono::DateTime<chrono::Utc>>,
    counts: Option<&(usize, usize, usize)>,
    node_count: usize,
    estimate: Option<serde_json::Value>,
) -> serde_json::Value {
    let (total, provisional, approved) = counts.copied().unwrap_or_default();
    serde_json::json!({
        "estimate": estimate,
        "id": plan.id,
        "name": plan.meta.name,
        "enabled": plan.meta.enabled,
        "schedule": plan.meta.schedule,
        "interval_secs": plan.meta.interval_secs,
        "run_as_identity": plan.meta.run_as_identity,
        "priority": plan.meta.priority,
        "max_items_per_run": plan.meta.max_items_per_run,
        "freshness_days": plan.meta.freshness_days,
        "model": plan.meta.model,
        "mode": plan.meta.resolved_mode(),
        "transcript_lookback_secs": plan.meta.transcript_lookback_secs,
        "source": plan.meta.source,
        "sources": plan.meta.sources.iter().map(|s| serde_json::json!({
            "skill": s.skill,
            "instance": s.instance,
            "scope": s.scope,
        })).collect::<Vec<_>>(),
        "body": plan.body,
        "last_run": last_run.map(|t| t.to_rfc3339()),
        "item_count": total,
        "provisional_count": provisional,
        "approved_count": approved,
        "node_count": node_count,
    })
}

fn item_to_json(item: &KnowledgeItem) -> serde_json::Value {
    serde_json::json!({
        "id": item.id.to_string(),
        "plan_id": item.plan_id,
        "topic": item.topic,
        "claim": item.claim,
        "status": item.status.as_str(),
        "confidence": item.confidence,
        "high_stakes": item.high_stakes,
        "source": {
            "kind": item.source.kind.as_str(),
            "skill": item.source.skill,
            "reference": item.source.reference,
            "excerpt": item.source.excerpt,
        },
        "created_at": item.created_at.to_rfc3339(),
        "created_by": item.created_by,
        "verified_at": item.verified_at.to_rfc3339(),
        "expires_at": item.expires_at.map(|t| t.to_rfc3339()),
        "reviewed_at": item.reviewed_at.map(|t| t.to_rfc3339()),
        "reviewed_by": item.reviewed_by,
        "review_note": item.review_note,
        "superseded_by": item.superseded_by.map(|u| u.to_string()),
    })
}

fn node_to_json(node: &DeepNode) -> serde_json::Value {
    serde_json::json!({
        "id": node.id.to_string(),
        "agent_id": node.agent_id,
        "plan_id": node.plan_id,
        "parent_id": node.parent_id.map(|u| u.to_string()),
        "depth": node.depth,
        "title": node.title,
        "body": node.body,
        "status": node.status.as_str(),
        "confidence": node.confidence,
        "sources": node.sources.iter().map(|s| serde_json::json!({
            "kind": s.kind.as_str(),
            "skill": s.skill,
            "reference": s.reference,
            "excerpt": s.excerpt,
        })).collect::<Vec<_>>(),
        "coverage": node.coverage.iter().map(|c| serde_json::json!({
            "reference": c.reference,
            "covered": c.covered,
        })).collect::<Vec<_>>(),
        "created_at": node.created_at.to_rfc3339(),
        "created_by": node.created_by,
        "verified_at": node.verified_at.to_rfc3339(),
        "expires_at": node.expires_at.map(|t| t.to_rfc3339()),
        "reviewed_at": node.reviewed_at.map(|t| t.to_rfc3339()),
        "reviewed_by": node.reviewed_by,
        "review_note": node.review_note,
        "superseded_by": node.superseded_by.map(|u| u.to_string()),
    })
}

/// The mode an upsert should persist: an explicit request `mode` always wins;
/// when absent, an existing plan's mode is preserved (so editing a legacy
/// `mode = "items"` plan from the FE — which omits `mode` — does not flip it to
/// the default `DeepExpertise`); a brand-new plan with no mode stays `None`,
/// resolving to `DeepExpertise`.
fn resolve_upsert_mode(
    requested: Option<CurationMode>,
    existing: Option<&LearningPlan>,
) -> Option<CurationMode> {
    requested.or_else(|| existing.and_then(|p| p.meta.mode))
}

fn resolve_agent(state: &GatewayState, id: &str) -> Result<Arc<AgentService>, ApiError> {
    state
        .registry
        .get(id)
        .ok_or_else(|| not_found_err(&format!("agent '{id}' not found")))
}

/// Map a typed review error: a missing / cross-agent item is a 404 (the two are
/// deliberately indistinguishable), a store/memory failure is a generic 500 —
/// never the raw backend error echoed to the client.
fn review_err(e: review::ReviewError) -> ApiError {
    match e {
        review::ReviewError::NotFound => not_found_err("item not found"),
        review::ReviewError::Store(err) => {
            warn!(error = %err, "self-learning review store error");
            internal_err("review failed")
        }
    }
}

fn bad_request_err(msg: &str) -> ApiError {
    (
        StatusCode::BAD_REQUEST,
        Json(serde_json::json!({ "error": msg })),
    )
}

fn not_found_err(msg: &str) -> ApiError {
    (
        StatusCode::NOT_FOUND,
        Json(serde_json::json!({ "error": msg })),
    )
}

fn internal_err(msg: &str) -> ApiError {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({ "error": msg })),
    )
}

fn service_unavailable_err(msg: &str) -> ApiError {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(serde_json::json!({ "error": msg })),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan_with_mode(mode: Option<CurationMode>) -> LearningPlan {
        LearningPlan {
            id: "p".into(),
            meta: LearningPlanMeta {
                name: "P".into(),
                enabled: true,
                schedule: None,
                interval_secs: None,
                run_as_identity: None,
                priority: 0,
                max_items_per_run: None,
                freshness_days: None,
                model: None,
                source: None,
                mode,
                transcript_lookback_secs: None,
                sources: Vec::new(),
            },
            body: String::new(),
        }
    }

    // The `#[serde(flatten)]` on `PlanUpsert.meta` already routes the JSON
    // `mode` key into `meta.mode`; the preserve logic reads it from there.
    #[test]
    fn upsert_flatten_routes_mode_into_meta() {
        let with: PlanUpsert =
            serde_json::from_str(r#"{"id":"x","name":"n","mode":"items"}"#).unwrap();
        assert_eq!(with.meta.mode, Some(CurationMode::Items));
        let without: PlanUpsert = serde_json::from_str(r#"{"id":"x","name":"n"}"#).unwrap();
        assert_eq!(without.meta.mode, None);
    }

    #[test]
    fn resolve_mode_explicit_request_wins() {
        let existing = plan_with_mode(Some(CurationMode::Items));
        assert_eq!(
            resolve_upsert_mode(Some(CurationMode::DeepExpertise), Some(&existing)),
            Some(CurationMode::DeepExpertise)
        );
    }

    #[test]
    fn resolve_mode_preserves_existing_when_absent() {
        let existing = plan_with_mode(Some(CurationMode::Items));
        assert_eq!(
            resolve_upsert_mode(None, Some(&existing)),
            Some(CurationMode::Items)
        );
    }

    #[test]
    fn resolve_mode_new_plan_stays_none() {
        assert_eq!(resolve_upsert_mode(None, None), None);
    }
}
