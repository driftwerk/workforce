use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post, put};
use axum::{Extension, Json, Router};
use chrono::Datelike;
use serde::{Deserialize, Serialize};
use workforce_common::persistence::WorkflowInstanceRow;
use workforce_common::session::TurnPrincipal;

use crate::gateway::CallerContext;
use crate::hub_core::jobs::JobReader;
use crate::hub_core::workflows::{DecisionRefused, ResumeAsk, ResumeRefused, Resumed};
use crate::run_steering::{Steer, SteerAction, SteerRefused, Steerer};
use crate::state::HubState;
use workforce_common::admin::{
    AgentsSummary, AuditEntry, CostSnapshot, DashboardView, NodesSummary, TasksSummary,
    TeamsSummary,
};
use workforce_common::cost::TokenHistoryPoint;
use workforce_orchestrator::task::TaskStatus;
use workforce_orchestrator::workflow::instance::StepRecord;

/// Build the admin API router. Routes are path-relative so callers can
/// `nest` this router under whatever prefix they need (e.g. `/api` for the
/// standalone admin port, `/api/admin` for the gateway-mounted version).
pub fn router(state: Arc<HubState>) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/ready", get(ready))
        .route("/metrics", get(metrics))
        .route("/dashboard", get(dashboard))
        .route("/cost", get(cost_summary))
        .route("/cost/history", get(cost_history))
        .route("/cost/monthly", get(cost_monthly))
        .route("/cost/detail", get(cost_detail))
        .route("/scheduler/jobs/history", get(scheduled_jobs_history))
        .route("/tasks", post(submit_task))
        .route("/workflows", get(list_workflows))
        .route("/workflows/{id}", get(get_workflow_definition))
        .route("/workflows/{id}/run", post(run_workflow))
        .route("/playbooks", get(list_playbooks))
        .route("/workflow-instances", get(list_workflow_instances))
        .route("/workflow-instances/{id}", get(get_workflow_instance))
        .route(
            "/workflow-instances/{id}/resume",
            post(resume_workflow_instance),
        )
        .route(
            "/workflow-instances/{id}/abort",
            post(abort_workflow_instance),
        )
        .route(
            "/workflow-instances/{id}/reject",
            post(reject_workflow_instance),
        )
        .route("/workflow-instances/{id}/steps", get(get_instance_steps))
        .route("/workflow-instances/{id}/cost", get(get_instance_cost))
        .route(
            "/workflow-instances/{id}/retry",
            post(retry_instance_step),
        )
        .route("/workflow-instances/{id}/skip", post(skip_instance_step))
        .route("/workflow-instances/{id}/goto", post(goto_instance_step))
        .route("/workflow-instances/{id}/pause", post(pause_instance))
        .route(
            "/workflow-instances/{id}/variables",
            get(get_instance_variables),
        )
        .route(
            "/workflow-instances/{id}/variables/{key}",
            put(set_instance_variable_handler),
        )
        // ─── Sub-agent management (DW-428) ───
        .route("/subagents", get(list_subagents))
        .route("/subagents/{id}", get(get_subagent))
        .route("/subagents/{id}/cancel", post(cancel_subagent))
        .merge(administrators_only())
        .with_state(state)
}

/// Routes over everyone's tasks, audit trail, scheduled jobs, sessions and
/// logs, which name no owner a caller rule could narrow them by, and the hub's
/// event sources and knowledge index, which are the operator's to run. The
/// router admits the `admin` scope, which a caller who is no administrator may
/// hold; only an administrator is let through these.
fn administrators_only() -> Router<Arc<HubState>> {
    Router::new()
        .route("/events/sources", get(list_event_sources))
        .route("/events/sources/{id}/start", post(start_event_source))
        .route("/events/sources/{id}/stop", post(stop_event_source))
        .route("/events/sources/{id}/restart", post(restart_event_source))
        .route("/artifacts/index", get(artifact_index))
        .route("/artifacts/index/retry", post(retry_artifact_index))
        .route("/knowledge/vectors", get(code_vectors))
        .route("/knowledge/status", get(knowledge_status))
        .route("/knowledge/index", post(trigger_knowledge_index))
        .route("/knowledge/index/cancel", post(cancel_knowledge_index))
        .route("/logs", get(system_logs))
        .route("/tasks", get(list_tasks))
        .route("/audit", get(audit_log))
        .route(
            "/scheduler",
            get(list_scheduled_jobs).post(create_scheduled_job),
        )
        .route("/scheduler/history", get(get_global_history))
        .route(
            "/scheduler/{id}",
            get(get_scheduled_job)
                .put(update_scheduled_job)
                .delete(delete_scheduled_job),
        )
        .route("/scheduler/{id}/run", post(trigger_scheduled_job))
        .route("/scheduler/{id}/history", get(get_job_history))
        .route("/sessions/history", get(session_history))
        .route("/sessions/redact", post(redact_sessions))
        // Shadow mode's operator surface (DW-676).
        .route("/shadow/status", get(crate::shadow_admin::status))
        .route("/shadow/report", get(crate::shadow_admin::report))
        .route_layer(axum::middleware::from_fn(admit_administrator))
}

async fn admit_administrator(
    caller: Option<Extension<CallerContext>>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Result<axum::response::Response, crate::gateway::ScopeRejection> {
    if caller_writer(caller.as_ref()).is_admin {
        Ok(next.run(request).await)
    } else {
        Err(crate::gateway::ScopeRejection::MissingScope)
    }
}

/// Process-wide shared Postgres `KnowledgeStore` backing the artifact tool
/// (Artifacts v2, DW-549/552). Built lazily from `[postgres]` and shared by
/// every agent so the knowledge base is genuinely cross-agent. `None` when
/// Postgres is disabled/unreachable — agents then fall back to a per-agent
/// file backend. Mirrors the existing global tool-output store in `state.rs`.
pub static SHARED_ARTIFACT_STORE: tokio::sync::OnceCell<
    Option<Arc<workforce_orchestrator::artifact_store::KnowledgeStore>>,
> = tokio::sync::OnceCell::const_new();

/// Where each store is kept, as the config this process serves says, for
/// health reports.
static STORE_BACKENDS: std::sync::OnceLock<serde_json::Value> = std::sync::OnceLock::new();

/// Remember where `config` keeps each store, for [`store_backends`], and
/// say loudly, once, if it leaves an index nowhere.
pub fn note_store_backends(config: &workforce_common::settings::WorkforceConfig) {
    let backends: serde_json::Map<String, serde_json::Value> = config
        .store_backends()
        .into_iter()
        .map(|(store, backend)| (store.to_string(), backend.into()))
        .collect();
    if STORE_BACKENDS.set(backends.into()).is_ok() {
        for left in config.indexes_left_nowhere() {
            tracing::error!("{left}");
        }
    }
}

/// Where memories, the artifact index and code vectors are kept; `null`
/// before a config was served.
fn store_backends() -> serde_json::Value {
    STORE_BACKENDS.get().cloned().unwrap_or_default()
}

/// How the artifact index of the process-wide store stands, for health
/// reports; `None` before the store is opened, or with none.
fn artifact_index_state() -> Option<workforce_orchestrator::artifact_store::IndexState> {
    let store = SHARED_ARTIFACT_STORE.get()?.as_ref()?;
    Some(store.index_state())
}

/// Have the hub's index keeper try at once what it is waiting to try
/// again; how many artifacts that is. `None` with no artifact store.
async fn retry_index() -> Option<eyre::Result<u64>> {
    let store = SHARED_ARTIFACT_STORE.get()?.as_ref()?;
    Some(store.retry_now().await)
}

/// Liveness, and how the artifact index and the code vectors stand, in
/// outline. It is read without a key, so it names no artifact or repo and
/// repeats no error; and it is what tells an orchestrator the hub is alive,
/// so it only reads what the hub last saw, asking nothing of Postgres or
/// the vector database. The whole is an administrator's, at
/// `/artifacts/index` and `/knowledge/vectors`.
async fn health(State(state): State<Arc<HubState>>) -> Json<serde_json::Value> {
    let index = artifact_index_state();
    let code = crate::knowledge::code_vector_status();
    Json(serde_json::json!({
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION"),
        "stores": store_backends(),
        "artifact_index": index.as_ref().map(artifact_index_outline),
        // Shadow mode's vellev mirror of it, in the same outline; null
        // with no mirror (DW-676).
        "artifact_index_mirror": crate::serve::artifact_mirror_state()
            .map(|(index, lease)| mirror_outline(&index, &lease)),
        "code_vectors": code_vectors_outline(&code),
        "code_vector_search_breaker": crate::knowledge::code_vector_breaker(),
        // Shadow mode's counts in this hub process (DW-676): null when it
        // shadows nothing. The mirror's backlog is not here: health asks
        // nothing of Postgres. It is in /metrics and the admin status.
        "shadow": crate::shadow_status::health_json(
            &crate::shadow_status::process_state(),
            state.config.vellev.shadow.paused,
        ),
    }))
}

/// What anyone may know of the code vectors: how many repos have them
/// complete, how far the fills under way have got, and how much is left
/// undone. It names no repo and repeats no error; `null` where no store
/// keeps track of them.
fn code_vectors_outline(repos: &[workforce_knowledge_service::RepoVectors]) -> serde_json::Value {
    use workforce_codebase_knowledge::graph::vectors::FillStage;
    if repos.is_empty() {
        return serde_json::Value::Null;
    }
    let filling: Vec<_> = repos
        .iter()
        .filter(|repo| !repo.complete)
        .filter_map(|repo| repo.fill.as_ref())
        .filter(|fill| fill.stage != FillStage::Done)
        .collect();
    serde_json::json!({
        "repos": repos.len(),
        "complete": repos.iter().filter(|repo| repo.complete).count(),
        "filling": filling.len(),
        "stored": filling.iter().map(|fill| fill.done).sum::<u64>(),
        "to_store": filling.iter().map(|fill| fill.total).sum::<u64>(),
        "pending_files": repos.iter().map(|repo| repo.pending_files).sum::<usize>(),
        "errors": repos.iter().filter(|repo| repo.last_error.is_some()).count(),
        // Repos not compared with the graph, entity by entity, since this
        // process began, and how long ago the one compared longest ago was.
        "uncompared": repos.iter().filter(|repo| repo.compared_secs_ago.is_none()).count(),
        "oldest_compare_secs": repos.iter().filter_map(|repo| repo.compared_secs_ago).max(),
    })
}

/// How every repo's code vectors stand, in full (see
/// [`workforce_knowledge_service::RepoVectors`]): each repo by name, how far its fill
/// has got, the files still to put right, and why the last pass left it
/// with something undone. It names repos and repeats errors that say where
/// the vector database is, so it is an administrator's alone.
async fn code_vectors() -> Json<serde_json::Value> {
    Json(serde_json::json!(crate::knowledge::code_vector_status()))
}

/// What anyone may know of the artifact index: whether search relies on
/// it, how far a backfill has got, and how much is left undone.
/// The mirror's outline: the artifact index's, and its lease. Lapses are
/// what to alert on: writes made while it had lapsed were not noted for
/// the mirror, and wait for its next compare.
fn mirror_outline(
    index: &workforce_orchestrator::artifact_store::IndexState,
    lease: &workforce_orchestrator::artifact_store::LeaseState,
) -> serde_json::Value {
    let mut outline = artifact_index_outline(index);
    outline["lease_until"] = serde_json::json!(lease.until);
    outline["lease_lapses"] = serde_json::json!(lease.lapses);
    // A keeper that is not running leaves the counts above frozen at its
    // last round, and the lease lapsing.
    outline["keeper_alive"] = serde_json::json!(lease.keeper_alive);
    // Running and stuck: its lease is not renewed while it is.
    outline["keeper_stalled"] = serde_json::json!(lease.keeper_stalled);
    outline["progress_age_secs"] = serde_json::json!(lease.progress_age.map(|age| age.as_secs()));
    outline
}

fn artifact_index_outline(
    index: &workforce_orchestrator::artifact_store::IndexState,
) -> serde_json::Value {
    serde_json::json!({
        "configured": index.configured,
        "complete": index.complete,
        "backfill": index.backfill,
        "pending": index.pending,
        // Pending artifacts the index has failed to take, each tried again
        // when it is due and none ever given up. Which, and what the index
        // said of each, is the administrator's (the full state names them).
        "failing": index.failing.len(),
        // Of those, how many wait the longest between tries, having failed
        // at every try for a while; and for how long the oldest has been
        // failing, which is what to alert on.
        "failing_at_cap": index.failing_at_cap,
        "oldest_failure_secs": index
            .oldest_failure_at
            .map(|at| (chrono::Utc::now() - at).num_seconds().max(0)),
        // How a search goes: "index", or "keyword" while the index is
        // being filled or more is waiting than a search may leave out.
        "search": index.search,
        // What the counts here are as of: the keeper's last round. Read
        // without asking anything of Postgres.
        "counted_at": index.counted_at,
        // Whether a write found the index not the one the hub made (a
        // Weaviate class made by its auto-schema): searched by keyword
        // until it is put right. Why, and what to do, is the
        // administrator's.
        "not_ours": index.not_ours.is_some(),
        "failed": index.last_backfill.as_ref().map_or(0, |last| last.failed),
        "error": index.last_error.is_some(),
        // Whether searches are asking the index: "closed" when they are,
        // "open" while it is left alone after not answering in time,
        // "half_open" when it is about to be tried again; null where
        // searches have no deadline.
        "search_breaker": index.search_breaker,
    })
}

/// How the artifact index stands, in full (see
/// [`workforce_orchestrator::artifact_store::IndexState`]): which
/// artifacts it would not take and why, and why the last round left
/// something undone. `null` with no artifact store. It names artifacts
/// that are not the caller's and repeats errors that name where the hub's
/// vector database is, so it is an administrator's alone.
async fn artifact_index() -> Json<serde_json::Value> {
    Json(serde_json::json!(artifact_index_state()))
}

/// Have the index keeper try at once every artifact it is waiting to try
/// again: for an operator who has put right what the index was failing
/// on. Answers how many artifacts that is.
async fn retry_artifact_index() -> (StatusCode, Json<serde_json::Value>) {
    match retry_index().await {
        Some(Ok(due)) => (StatusCode::OK, Json(serde_json::json!({ "due_now": due }))),
        Some(Err(e)) => {
            let said = serde_json::json!({ "error": format!("{e:#}") });
            (StatusCode::INTERNAL_SERVER_ERROR, Json(said))
        }
        None => {
            let said = serde_json::json!({ "error": "this hub has no artifact store" });
            (StatusCode::NOT_FOUND, Json(said))
        }
    }
}

/// Readiness probe — distinct from `/health` (which is liveness).
///
/// Returns 200 when the hub can serve traffic: orchestrator responsive and
/// NATS connected (when configured). Returns 503 with a JSON body listing
/// which components are degraded so load balancers / k8s probes can drain
/// traffic away.
async fn ready(State(state): State<Arc<HubState>>) -> (StatusCode, Json<serde_json::Value>) {
    let mut components = serde_json::Map::new();
    let mut all_ok = true;

    // Orchestrator: cheap call that only fails if its internal lock is poisoned.
    let orch_ok = tokio::time::timeout(
        std::time::Duration::from_millis(500),
        state.orchestrator.task_count(),
    )
    .await
    .is_ok();
    components.insert("orchestrator".into(), serde_json::json!(orch_ok));
    all_ok &= orch_ok;

    // NATS: only required if the deployment configured it. `Connected`
    // is the green state; everything else (Pending, Disconnected) is not.
    if let Some(client) = state.nats.as_ref() {
        let connected = matches!(
            client.connection_state(),
            async_nats::connection::State::Connected
        );
        components.insert("nats".into(), serde_json::json!(connected));
        all_ok &= connected;
    }

    let status = if all_ok {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (
        status,
        Json(serde_json::json!({
            "status": if all_ok { "ready" } else { "not_ready" },
            "components": components,
        })),
    )
}

/// Render the Prometheus exposition snapshot.
///
/// Returns plain text in the Prometheus 0.0.4 format. Unauthenticated by
/// design — scraping infra (Prometheus, GMP, Datadog Agent) doesn't carry
/// auth tokens; rely on network policy to keep this private.
///
/// The hub always enables `workforce-common`'s `prometheus` feature, so
/// the recorder is installed by `setup_logging`. Any empty body here means
/// the install step in `setup_logging` failed at startup — check stderr
/// for the warning.
async fn metrics(
    State(state): State<Arc<HubState>>,
) -> (StatusCode, [(&'static str, &'static str); 1], String) {
    let mut body = workforce_common::logging::PROMETHEUS_HANDLE
        .get()
        .map(|h| h.render())
        .unwrap_or_else(|| {
            "# prometheus recorder not installed; check stderr at startup\n".to_string()
        });
    // Fleet-health gauges are rendered directly from process state, not the
    // recorder — see health_gauges.rs for why.
    let nats_connected = state.nats.as_ref().map(|client| {
        matches!(
            client.connection_state(),
            async_nats::connection::State::Connected
        )
    });
    if !body.ends_with('\n') {
        body.push('\n');
    }
    // The mirror's backlog is asked of Postgres here, bounded, so that the
    // rendering stays synchronous; not read in time, it is told unknown.
    let backlog = crate::shadow_status::backlog(&state.config).await;
    body.push_str(&crate::health_gauges::render(
        nats_connected,
        backlog.as_ref(),
    ));
    (
        StatusCode::OK,
        [("content-type", "text/plain; version=0.0.4")],
        body,
    )
}

async fn dashboard(
    State(state): State<Arc<HubState>>,
    caller: Option<Extension<CallerContext>>,
    gateway: Option<Extension<Arc<crate::gateway::GatewayState>>>,
) -> Json<DashboardView> {
    let named = NamedAgents::of(caller.as_ref(), gateway.as_ref());
    let active_tasks = state.orchestrator.active_tasks().await;
    let all_tasks = state.orchestrator.list_tasks(None).await;
    let node_stats = state.node_registry.read().await.stats();

    let completed = all_tasks
        .iter()
        .filter(|t| t.status == TaskStatus::Complete)
        .count();
    let failed = all_tasks
        .iter()
        .filter(|t| t.status == TaskStatus::Failed)
        .count();

    let now = chrono::Utc::now();
    let today_start = now.date_naive().and_hms_opt(0, 0, 0).unwrap();
    let today_start =
        chrono::DateTime::<chrono::Utc>::from_naive_utc_and_offset(today_start, chrono::Utc);
    let month_start = now
        .date_naive()
        .with_day(1)
        .unwrap()
        .and_hms_opt(0, 0, 0)
        .unwrap();
    let month_start =
        chrono::DateTime::<chrono::Utc>::from_naive_utc_and_offset(month_start, chrono::Utc);
    let today_cost = state
        .telemetry_store
        .total_cost(today_start, now)
        .unwrap_or(0.0);
    let month_cost = state
        .telemetry_store
        .total_cost(month_start, now)
        .unwrap_or(0.0);
    let today_by_agent = state
        .telemetry_store
        .cost_by_agent(today_start, now)
        .unwrap_or_default();
    let today_by_model = state
        .telemetry_store
        .cost_by_model(today_start, now)
        .unwrap_or_default();

    Json(DashboardView {
        agents: AgentsSummary {
            total: 0,
            active: active_tasks.len(),
            idle: 0,
        },
        teams: TeamsSummary {
            total: 0,
            total_members: 0,
        },
        tasks: TasksSummary {
            total: all_tasks.len(),
            in_progress: active_tasks.len(),
            completed,
            failed,
        },
        nodes: NodesSummary {
            total: node_stats.total,
            online: node_stats.online,
            offline: node_stats.offline,
        },
        cost: CostSnapshot {
            today_usd: today_cost,
            month_usd: month_cost,
            top_agent: today_by_agent
                .iter()
                .find(|(agent, _)| named.shows(agent))
                .map(|(agent, _)| agent.clone()),
            top_model: today_by_model.first().map(|(k, _)| k.clone()),
        },
    })
}

#[derive(Deserialize)]
struct TaskQuery {
    status: Option<String>,
}

async fn list_tasks(
    State(state): State<Arc<HubState>>,
    Query(query): Query<TaskQuery>,
) -> Json<serde_json::Value> {
    let status_filter = query.status.and_then(|s| match s.as_str() {
        "created" => Some(TaskStatus::Created),
        "assigned" => Some(TaskStatus::Assigned),
        "in_progress" => Some(TaskStatus::InProgress),
        "in_review" => Some(TaskStatus::InReview),
        "complete" => Some(TaskStatus::Complete),
        "failed" => Some(TaskStatus::Failed),
        _ => None,
    });

    let tasks = state.orchestrator.list_tasks(status_filter).await;
    Json(serde_json::json!({
        "tasks": tasks.iter().map(|t| serde_json::json!({
            "id": t.id.0,
            "title": t.title,
            "status": t.status,
            "priority": t.priority,
            "assignee": t.assignee.as_ref().map(|a| a.as_str().to_string()),
            "created_at": t.created_at.to_rfc3339(),
        })).collect::<Vec<_>>(),
    }))
}

async fn cost_summary(
    State(state): State<Arc<HubState>>,
    caller: Option<Extension<CallerContext>>,
    gateway: Option<Extension<Arc<crate::gateway::GatewayState>>>,
) -> Json<serde_json::Value> {
    let named = NamedAgents::of(caller.as_ref(), gateway.as_ref());
    let now = chrono::Utc::now();
    let today_start = now.date_naive().and_hms_opt(0, 0, 0).unwrap();
    let today_start =
        chrono::DateTime::<chrono::Utc>::from_naive_utc_and_offset(today_start, chrono::Utc);

    // Query historic data from SQLite (persisted across restarts)
    let total_cost = state
        .telemetry_store
        .total_cost(today_start, now)
        .unwrap_or(0.0);
    let by_agent: std::collections::HashMap<String, f64> = state
        .telemetry_store
        .cost_by_agent(today_start, now)
        .unwrap_or_default()
        .into_iter()
        .filter(|(agent, _)| named.shows(agent))
        .collect();
    let by_model: std::collections::HashMap<String, f64> = state
        .telemetry_store
        .cost_by_model(today_start, now)
        .unwrap_or_default()
        .into_iter()
        .collect();
    let by_provider: std::collections::HashMap<String, f64> = state
        .telemetry_store
        .cost_by_provider(today_start, now)
        .unwrap_or_default()
        .into_iter()
        .collect();
    let (input_tokens, output_tokens, record_count) = state
        .telemetry_store
        .cost_token_totals(today_start, now)
        .unwrap_or((0, 0, 0));

    Json(serde_json::json!({
        "total_cost_usd": total_cost,
        "total_input_tokens": input_tokens,
        "total_output_tokens": output_tokens,
        "by_agent": by_agent,
        "by_model": by_model,
        "by_provider": by_provider,
        "record_count": record_count,
    }))
}

#[derive(Deserialize)]
struct CostHistoryQuery {
    #[serde(default)]
    days: Option<u32>,
}

#[derive(Serialize)]
struct CostHistoryResponse {
    days: i64,
    points: Vec<TokenHistoryPoint>,
}

async fn cost_history(
    State(state): State<Arc<HubState>>,
    Query(q): Query<CostHistoryQuery>,
) -> Json<CostHistoryResponse> {
    let days = q.days.unwrap_or(7).clamp(1, 90) as i64;
    let now = chrono::Utc::now();
    let today_start = now.date_naive().and_hms_opt(0, 0, 0).unwrap().and_utc();
    let from = today_start - chrono::Duration::days(days - 1);

    let by_day: std::collections::HashMap<chrono::NaiveDate, TokenHistoryPoint> = state
        .telemetry_store
        .cost_token_history_by_day(from, now)
        .unwrap_or_default()
        .into_iter()
        .map(|p| (p.bucket_start.date_naive(), p))
        .collect();

    let points: Vec<TokenHistoryPoint> = (0..days)
        .map(|i| {
            let day = (from + chrono::Duration::days(i)).date_naive();
            by_day
                .get(&day)
                .cloned()
                .unwrap_or_else(|| TokenHistoryPoint {
                    bucket_start: day.and_hms_opt(0, 0, 0).unwrap().and_utc(),
                    input_tokens: 0,
                    output_tokens: 0,
                    cache_creation_input_tokens: 0,
                    cache_read_input_tokens: 0,
                })
        })
        .collect();

    Json(CostHistoryResponse { days, points })
}

/// Monthly token + cost history for the past N months (default 12, max 36).
///
/// Returns one bucket per calendar month, oldest first. Months with no
/// activity are included with zero values so the client gets a contiguous
/// series it can render as a bar chart.
#[derive(Deserialize)]
struct CostMonthlyQuery {
    months: Option<u32>,
}

#[derive(Serialize)]
struct MonthlyPoint {
    month: String, // "YYYY-MM"
    input_tokens: u64,
    output_tokens: u64,
    cache_creation_input_tokens: u64,
    cache_read_input_tokens: u64,
    cost_usd: f64,
}

#[derive(Serialize)]
struct CostMonthlyResponse {
    months: i64,
    points: Vec<MonthlyPoint>,
}

async fn cost_monthly(
    State(state): State<Arc<HubState>>,
    Query(q): Query<CostMonthlyQuery>,
) -> Json<CostMonthlyResponse> {
    use chrono::{Datelike, TimeZone};
    let months = q.months.unwrap_or(12).clamp(1, 36) as i64;
    let now = chrono::Utc::now();

    // Build a list of (month_key, month_start, month_end) for the window
    let month_ranges: Vec<(
        String,
        chrono::DateTime<chrono::Utc>,
        chrono::DateTime<chrono::Utc>,
    )> = (0..months)
        .map(|i| {
            let mut y = now.year();
            let mut mo = now.month() as i32 - (months as i32 - 1 - i as i32);
            while mo <= 0 {
                mo += 12;
                y -= 1;
            }
            let start = chrono::Utc
                .with_ymd_and_hms(y, mo as u32, 1, 0, 0, 0)
                .unwrap();
            let mut next_y = y;
            let mut next_mo = mo + 1;
            if next_mo > 12 {
                next_mo = 1;
                next_y += 1;
            }
            let end = chrono::Utc
                .with_ymd_and_hms(next_y, next_mo as u32, 1, 0, 0, 0)
                .unwrap();
            (format!("{y:04}-{mo:02}"), start, end)
        })
        .collect();

    // Token history (one query across the full window)
    let window_start = month_ranges.first().map(|(_, s, _)| *s).unwrap_or(now);
    let token_by_month: std::collections::HashMap<String, TokenHistoryPoint> = state
        .telemetry_store
        .cost_token_history_by_month(window_start, now)
        .unwrap_or_default()
        .into_iter()
        .map(|p| {
            let key = format!("{:04}-{:02}", p.bucket_start.year(), p.bucket_start.month());
            (key, p)
        })
        .collect();

    // Cost per month from cost_records (cost_usd is the authoritative figure).
    let cost_by_month: std::collections::HashMap<String, f64> = month_ranges
        .iter()
        .map(|(key, start, end)| {
            let cost = state
                .telemetry_store
                .total_cost(*start, *end)
                .unwrap_or(0.0);
            (key.clone(), cost)
        })
        .collect();

    let points: Vec<MonthlyPoint> = month_ranges
        .iter()
        .map(|(key, _, _)| {
            let tokens = token_by_month.get(key);
            MonthlyPoint {
                month: key.clone(),
                input_tokens: tokens.map(|t| t.input_tokens).unwrap_or(0),
                output_tokens: tokens.map(|t| t.output_tokens).unwrap_or(0),
                cache_creation_input_tokens: tokens
                    .map(|t| t.cache_creation_input_tokens)
                    .unwrap_or(0),
                cache_read_input_tokens: tokens.map(|t| t.cache_read_input_tokens).unwrap_or(0),
                cost_usd: cost_by_month.get(key).copied().unwrap_or(0.0),
            }
        })
        .collect();

    Json(CostMonthlyResponse { months, points })
}

/// Maximum page size for the detail endpoints. Caps memory + payload.
const DETAIL_PAGE_LIMIT_MAX: u32 = 500;

#[derive(Deserialize)]
struct CostDetailQuery {
    start: Option<String>,
    end: Option<String>,
    agent_id: Option<String>,
    user_id: Option<String>,
    billing: Option<workforce_common::subscription::Billing>,
    #[serde(default = "default_detail_limit")]
    limit: u32,
    #[serde(default)]
    offset: u32,
}

fn default_detail_limit() -> u32 {
    100
}

/// Parse RFC3339 → UTC, falling back to `default` when missing or
/// malformed. Returning the default on parse error matches the leniency
/// the rest of the admin API expects from optional query params.
fn parse_rfc3339_or(
    opt: Option<&str>,
    default: chrono::DateTime<chrono::Utc>,
) -> chrono::DateTime<chrono::Utc> {
    opt.and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.with_timezone(&chrono::Utc))
        .unwrap_or(default)
}

/// The agents a caller may see named in cost figures. An administrator sees
/// every one; anyone else the agents the gateway's roster shows them
/// ([`crate::gateway::AgentViewer`]), and none when the gate handed no gateway
/// over. A hidden agent's spend stays in the totals, unnamed.
enum NamedAgents<'a> {
    Every,
    Seen(crate::gateway::AgentViewer<'a>),
    NoneAtAll,
}

impl<'a> NamedAgents<'a> {
    fn of(
        caller: Option<&Extension<CallerContext>>,
        gateway: Option<&'a Extension<Arc<crate::gateway::GatewayState>>>,
    ) -> Self {
        match (caller, gateway) {
            (Some(Extension(caller)), _) if caller.is_admin => Self::Every,
            (Some(Extension(caller)), Some(Extension(gateway))) => {
                Self::Seen(crate::gateway::AgentViewer::new(gateway, caller.clone()))
            }
            _ => Self::NoneAtAll,
        }
    }

    fn shows(&self, agent_id: &str) -> bool {
        match self {
            Self::Every => true,
            Self::Seen(viewer) => viewer.can_see(agent_id),
            Self::NoneAtAll => false,
        }
    }
}

/// The person whose cost and run records a read covers: whoever an
/// administrator asks for (everyone when they name nobody), and otherwise the
/// caller alone, whatever they ask for. A caller who is neither an
/// administrator nor anybody reads no one's.
fn records_of(
    reader: &crate::def_write_auth::DefWriter,
    asked: Option<&str>,
) -> Result<Option<String>, (StatusCode, String)> {
    match reader {
        crate::def_write_auth::DefWriter { is_admin: true, .. } => Ok(asked.map(str::to_string)),
        crate::def_write_auth::DefWriter {
            identity_id: Some(own),
            ..
        } => Ok(Some(own.clone())),
        _ => Err((StatusCode::FORBIDDEN, "forbidden".to_string())),
    }
}

async fn cost_detail(
    State(state): State<Arc<HubState>>,
    caller: Option<Extension<CallerContext>>,
    gateway: Option<Extension<Arc<crate::gateway::GatewayState>>>,
    Query(q): Query<CostDetailQuery>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    use crate::api_handlers::identity_display::{
        enrich_row_json, resolve_agents, resolve_payers, resolve_principals, AgentRef,
        PrincipalKind, PrincipalRef,
    };
    use workforce_common::persistence::{BillingSplit, CostDetailFilter, CostScope};
    use workforce_common::subscription::{plan_label, Billing};

    let now = chrono::Utc::now();
    let start = parse_rfc3339_or(q.start.as_deref(), now - chrono::Duration::hours(1));
    let end = parse_rfc3339_or(q.end.as_deref(), now);
    let limit = q.limit.min(DETAIL_PAGE_LIMIT_MAX);
    let reader = caller_writer(caller.as_ref());
    let user_id = records_of(&reader, q.user_id.as_deref())?;

    let filter = CostDetailFilter {
        scope: CostScope {
            agent_id: q.agent_id.as_deref(),
            user_id: user_id.as_deref(),
        },
        billing: q.billing,
    };
    let internal = |e: eyre::Report| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
    let mut page = state
        .telemetry_store
        .cost_detail_records(start, end, filter, limit, q.offset)
        .map_err(internal)?;
    if !reader.is_admin {
        page.distinct_users
            .retain(|id| Some(id) == user_id.as_ref());
        let named = NamedAgents::of(caller.as_ref(), gateway.as_ref());
        page.distinct_agents.retain(|agent| named.shows(agent));
    }
    let billing_totals = BillingSplit::of(
        &state
            .telemetry_store
            .cost_split(start, end, filter.scope)
            .map_err(internal)?,
    );

    let principals = resolve_principals(
        state.identity_store(),
        page.records
            .iter()
            .filter_map(|r| r.user_id.as_deref())
            .chain(page.distinct_users.iter().map(|s| s.as_str())),
    );
    let agents = state
        .agent_registry()
        .map(|reg| {
            resolve_agents(
                reg,
                page.records
                    .iter()
                    .map(|r| r.agent_id.as_str())
                    .chain(page.distinct_agents.iter().map(|s| s.as_str())),
            )
        })
        .unwrap_or_default();

    let payers = state
        .agent_registry()
        .map(|reg| {
            resolve_payers(
                state.identity_store(),
                reg,
                page.records.iter().filter_map(|r| r.payer.as_deref()),
            )
        })
        .unwrap_or_default();

    let records: Vec<serde_json::Value> = page
        .records
        .iter()
        .map(|r| {
            let mut row =
                enrich_row_json(r, r.user_id.as_deref(), &r.agent_id, &principals, &agents);
            row["plan"] = serde_json::json!((r.billing == Billing::Subscription).then(|| {
                let payer = r.payer.as_deref();
                plan_label(
                    payer,
                    payer
                        .and_then(|payer| payers.get(payer))
                        .map(String::as_str),
                    r.credential_tier,
                )
            }));
            row
        })
        .collect();

    let distinct_users: Vec<PrincipalRef> = page
        .distinct_users
        .iter()
        .map(|id| {
            principals.get(id).cloned().unwrap_or_else(|| PrincipalRef {
                id: id.clone(),
                display_name: id.clone(),
                kind: PrincipalKind::User,
            })
        })
        .collect();
    let distinct_agents: Vec<AgentRef> = page
        .distinct_agents
        .iter()
        .map(|id| {
            agents.get(id).cloned().unwrap_or_else(|| AgentRef {
                id: id.clone(),
                display_name: id.clone(),
            })
        })
        .collect();

    Ok(Json(serde_json::json!({
        "records": records,
        "total": page.total,
        "distinct_users": distinct_users,
        "distinct_agents": distinct_agents,
        "billing_totals": billing_totals,
        "limit": limit,
        "offset": q.offset,
    })))
}

#[derive(Deserialize)]
struct ScheduledJobHistoryQuery {
    start: Option<String>,
    end: Option<String>,
    agent_id: Option<String>,
    #[serde(default = "default_detail_limit")]
    limit: u32,
    #[serde(default)]
    offset: u32,
}

/// A scheduled run's record carries what it cost, which is its owner's and an
/// administrator's to see, so a caller who is no administrator reads their own.
async fn scheduled_jobs_history(
    State(state): State<Arc<HubState>>,
    caller: Option<Extension<CallerContext>>,
    Query(q): Query<ScheduledJobHistoryQuery>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    use crate::api_handlers::identity_display::{
        enrich_row_json, resolve_agents, resolve_principals,
    };
    use workforce_common::persistence::CostScope;

    let now = chrono::Utc::now();
    let start = parse_rfc3339_or(q.start.as_deref(), now - chrono::Duration::days(7));
    let end = parse_rfc3339_or(q.end.as_deref(), now);
    let limit = q.limit.min(DETAIL_PAGE_LIMIT_MAX);
    let user_id = records_of(&caller_writer(caller.as_ref()), None)?;

    let page = state
        .telemetry_store
        .scheduled_job_history(
            start,
            end,
            CostScope {
                agent_id: q.agent_id.as_deref(),
                user_id: user_id.as_deref(),
            },
            limit,
            q.offset,
        )
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let principals = resolve_principals(
        state.identity_store(),
        page.records.iter().filter_map(|r| r.user_id.as_deref()),
    );
    let agents = state
        .agent_registry()
        .map(|reg| resolve_agents(reg, page.records.iter().map(|r| r.agent_id.as_str())))
        .unwrap_or_default();

    let records: Vec<serde_json::Value> = page
        .records
        .iter()
        .map(|r| enrich_row_json(r, r.user_id.as_deref(), &r.agent_id, &principals, &agents))
        .collect();

    Ok(Json(serde_json::json!({
        "records": records,
        "total": page.total,
        "limit": limit,
        "offset": q.offset,
    })))
}

#[derive(Deserialize)]
struct AuditQuery {
    query: Option<String>,
    limit: Option<usize>,
    #[serde(default)]
    offset: Option<usize>,
    #[serde(default)]
    from: Option<String>,
    #[serde(default)]
    to: Option<String>,
    #[serde(default)]
    agent: Option<String>,
    #[serde(default)]
    action: Option<String>,
    #[serde(default)]
    outcome: Option<String>,
    #[serde(default)]
    format: Option<String>,
}

#[derive(Deserialize)]
struct SubmitTaskRequest {
    title: String,
    description: String,
    team_id: Option<String>,
}

/// Who moves a run through the API: the identity behind the caller, or the
/// hub for a client with none (the static token).
fn caller_actor(
    caller: Option<Extension<CallerContext>>,
) -> workforce_orchestrator::workflow::instance::Actor {
    workforce_orchestrator::workflow::instance::Actor::of(
        caller.and_then(|Extension(caller)| caller.identity_id),
        None,
    )
}

/// Leave for the admin caller to `action` the run `id`, or the refusal as the
/// route answers it.
async fn steer(
    runtime: &dyn crate::hosts::WorkflowHost,
    id: &str,
    caller: Option<Extension<CallerContext>>,
    action: SteerAction,
) -> Result<Steer, (StatusCode, Json<serde_json::Value>)> {
    let steerer = Steerer {
        writer: caller_writer(caller.as_ref()),
        actor: caller_actor(caller),
    };
    runtime
        .authorize_steer(id, &steerer, action)
        .await
        .map_err(|refused| refusal(steer_status(&refused), &refused))
}

/// How a route answers a steer refused: a run waiting on a decision
/// conflicts with the steer, anything else is forbidden or malformed.
fn steer_status(refused: &SteerRefused) -> StatusCode {
    match refused {
        SteerRefused::Unknown(_) => StatusCode::BAD_REQUEST,
        SteerRefused::AwaitingDecision { .. } | SteerRefused::StopAtDecision => {
            StatusCode::CONFLICT
        }
        _ => StatusCode::FORBIDDEN,
    }
}

/// How a route answers a resume refused, a press of a decision included.
fn resume_status(refused: &ResumeRefused) -> StatusCode {
    match refused {
        ResumeRefused::Steer(refused)
        | ResumeRefused::Decision(DecisionRefused::Steer(refused)) => steer_status(refused),
        ResumeRefused::Decision(
            DecisionRefused::NotWaiting
            | DecisionRefused::Ended { .. }
            | DecisionRefused::Decided { .. }
            | DecisionRefused::Outdated
            | DecisionRefused::StepGone { .. }
            | DecisionRefused::WorkflowGone,
        ) => StatusCode::CONFLICT,
        ResumeRefused::Decision(
            DecisionRefused::Choose { .. } | DecisionRefused::Unavailable(_),
        )
        | ResumeRefused::Failed(_) => StatusCode::BAD_REQUEST,
    }
}

fn refusal(
    status: StatusCode,
    why: &impl std::fmt::Display,
) -> (StatusCode, Json<serde_json::Value>) {
    (
        status,
        Json(serde_json::json!({ "error": why.to_string() })),
    )
}

/// The caller as the run rules see it. The router admits the `admin` scope,
/// which a caller who is no administrator may hold; a request the gate did not
/// name is nobody.
fn caller_writer(caller: Option<&Extension<CallerContext>>) -> crate::def_write_auth::DefWriter {
    crate::def_write_auth::DefWriter {
        identity_id: caller.and_then(|Extension(caller)| caller.identity_id.clone()),
        is_admin: caller.is_some_and(|Extension(caller)| caller.is_admin),
    }
}

/// The run `id` when `caller` may read it
/// ([`crate::workflow_runtime::WorkflowRuntime::readable_runs`]), or the
/// not-found answer every route gives a run that is missing or someone else's.
async fn readable_instance(
    runtime: &dyn crate::hosts::WorkflowHost,
    id: &str,
    caller: Option<&Extension<CallerContext>>,
) -> Result<WorkflowInstanceRow, (StatusCode, Json<serde_json::Value>)> {
    runtime
        .readable_run(id, &caller_writer(caller))
        .await
        .ok()
        .flatten()
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": "Instance not found" })),
            )
        })
}

/// Who an admin request acts as: the identity behind the caller the admin
/// gate admitted, or nobody for the static admin token.
fn caller_principal(caller: Option<Extension<CallerContext>>) -> TurnPrincipal {
    TurnPrincipal::run_as(caller.and_then(|Extension(caller)| caller.identity_id))
}

/// A task runs as its submitter. Naming a team puts the run under that team's
/// policy and spend, which is an administrator's to choose.
async fn submit_task(
    State(state): State<Arc<HubState>>,
    caller: Option<Extension<CallerContext>>,
    Json(req): Json<SubmitTaskRequest>,
) -> Result<Json<serde_json::Value>, crate::gateway::ScopeRejection> {
    if req.team_id.is_some() && !caller_writer(caller.as_ref()).is_admin {
        return Err(crate::gateway::ScopeRejection::MissingScope);
    }
    Ok(
        match crate::worker::submit_task(
            &state,
            &req.title,
            &req.description,
            req.team_id.as_deref(),
            caller_principal(caller),
        )
        .await
        {
            Ok(result) => Json(serde_json::json!({
                "task_id": result.task_id,
                "success": result.success,
                "response": result.response,
                "input_tokens": result.input_tokens,
                "output_tokens": result.output_tokens,
            })),
            Err(e) => Json(serde_json::json!({
                "error": e.to_string(),
            })),
        },
    )
}

async fn audit_log(
    State(state): State<Arc<HubState>>,
    Query(query): Query<AuditQuery>,
    headers: HeaderMap,
) -> (StatusCode, HeaderMap, Vec<u8>) {
    let limit = query.limit.unwrap_or(50).min(1000);
    let offset = query.offset.unwrap_or(0);

    // Get a generous window of entries then filter/paginate in memory.
    // Deliberate tradeoff: audit logs are bounded in size and stored in
    // SQLite, so loading up to 10 000 rows and filtering in Rust is fast
    // and avoids a complex query builder. Revisit if audit volume grows.
    let all_entries: Vec<AuditEntry> = if let Some(ref q) = query.query {
        state.audit_log.search(q, 10_000)
    } else {
        state.audit_log.recent(10_000)
    };

    // Parse optional time filters
    let from_dt = query.from.as_deref().and_then(|s| {
        chrono::DateTime::parse_from_rfc3339(s)
            .ok()
            .map(|dt| dt.with_timezone(&chrono::Utc))
    });
    let to_dt = query.to.as_deref().and_then(|s| {
        chrono::DateTime::parse_from_rfc3339(s)
            .ok()
            .map(|dt| dt.with_timezone(&chrono::Utc))
    });

    // Apply filters
    let filtered: Vec<&AuditEntry> = all_entries
        .iter()
        .filter(|e| {
            if let Some(from) = from_dt {
                if let Ok(ts) = chrono::DateTime::parse_from_rfc3339(&e.timestamp) {
                    if ts.with_timezone(&chrono::Utc) < from {
                        return false;
                    }
                }
            }
            if let Some(to) = to_dt {
                if let Ok(ts) = chrono::DateTime::parse_from_rfc3339(&e.timestamp) {
                    if ts.with_timezone(&chrono::Utc) > to {
                        return false;
                    }
                }
            }
            if let Some(ref agent) = query.agent {
                if !e.actor.to_lowercase().contains(&agent.to_lowercase()) {
                    return false;
                }
            }
            if let Some(ref action_filter) = query.action {
                if !e
                    .action
                    .to_lowercase()
                    .contains(&action_filter.to_lowercase())
                {
                    return false;
                }
            }
            if let Some(ref outcome_filter) = query.outcome {
                let entry_outcome = format!("{:?}", e.outcome).to_lowercase();
                if !entry_outcome.contains(&outcome_filter.to_lowercase()) {
                    return false;
                }
            }
            true
        })
        .collect();

    let total = filtered.len();
    let page: Vec<&AuditEntry> = filtered.iter().skip(offset).take(limit).copied().collect();

    // Check if CSV export requested
    let want_csv = query.format.as_deref() == Some("csv")
        || headers
            .get("accept")
            .and_then(|v| v.to_str().ok())
            .map(|v| v.contains("text/csv"))
            .unwrap_or(false);

    if want_csv {
        let mut csv = String::from("timestamp,actor,action,resource,detail,outcome\n");
        for e in &page {
            fn csv_field(s: &str) -> String {
                format!("\"{}\"", s.replace('"', "\"\""))
            }
            let detail = e.detail.as_deref().unwrap_or("");
            let outcome = format!("{:?}", e.outcome).to_lowercase();
            csv.push_str(&format!(
                "{},{},{},{},{},{}\n",
                csv_field(&e.timestamp),
                csv_field(&e.actor),
                csv_field(&e.action),
                csv_field(&e.resource),
                csv_field(detail),
                csv_field(&outcome),
            ));
        }
        let mut resp_headers = HeaderMap::new();
        resp_headers.insert(
            axum::http::header::CONTENT_TYPE,
            axum::http::HeaderValue::from_static("text/csv"),
        );
        resp_headers.insert(
            axum::http::header::CONTENT_DISPOSITION,
            axum::http::HeaderValue::from_static("attachment; filename=\"audit.csv\""),
        );
        return (StatusCode::OK, resp_headers, csv.into_bytes());
    }

    // JSON response
    let json_val = serde_json::json!({
        "entries": page,
        "total": total,
        "limit": limit,
        "offset": offset,
    });
    let body = match serde_json::to_vec(&json_val) {
        Ok(b) => b,
        Err(e) => {
            tracing::error!(err = %e, "audit log serialization failed");
            let mut err_headers = HeaderMap::new();
            err_headers.insert(
                axum::http::header::CONTENT_TYPE,
                axum::http::HeaderValue::from_static("application/json"),
            );
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                err_headers,
                br#"{"error":"serialization failed"}"#.to_vec(),
            );
        }
    };
    let mut resp_headers = HeaderMap::new();
    resp_headers.insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("application/json"),
    );
    (StatusCode::OK, resp_headers, body)
}

async fn list_scheduled_jobs(State(state): State<Arc<HubState>>) -> Json<serde_json::Value> {
    let jobs = if let Some(ref scheduler) = state.scheduler {
        let jobs = scheduler.list_jobs().await;
        jobs.iter()
            .map(|job| serialize_listed_job(scheduler, job))
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };

    Json(serde_json::json!({ "jobs": jobs }))
}

/// [`serialize_job`] as a listing shows it: with what the job's runs have
/// cost so far, so an expensive job is visible where jobs are compared.
pub(crate) fn serialize_listed_job(
    scheduler: &crate::scheduler::Scheduler,
    job: &crate::hub_core::schedules::ScheduledJob,
) -> serde_json::Value {
    let mut value = serialize_job(job);
    value["spent_usd"] = serde_json::json!(scheduler.job_spend(&job.id));
    value
}

/// Serialize a ScheduledJob to a JSON value.
pub(crate) fn serialize_job(j: &crate::hub_core::schedules::ScheduledJob) -> serde_json::Value {
    use crate::hub_core::schedules::JobPayload;
    let message: Option<String> = match &j.payload {
        JobPayload::AgentTurn { message, .. } => Some(message.clone()),
        JobPayload::SystemEvent { text, .. } => Some(text.clone()),
        JobPayload::Reminder { text, .. } => Some(text.clone()),
        JobPayload::WorkflowStart { workflow_id, .. } => Some(format!("workflow: {workflow_id}")),
        JobPayload::MonitorSweep => Some("monitor sweep".to_string()),
        JobPayload::CrewSummary { evening } => Some(if *evening {
            "the overseer's evening summary".to_string()
        } else {
            "the overseer's morning summary".to_string()
        }),
        JobPayload::Consolidation { identity_id } => Some(match identity_id {
            Some(id) => format!("memory consolidation for {id}"),
            None => "memory consolidation".to_string(),
        }),
    };
    serde_json::json!({
        "id": j.id,
        "name": j.name,
        "description": j.description,
        "enabled": j.enabled,
        "schedule": j.schedule,
        "message": message,
        "next_run": j.next_run.map(|t| t.to_rfc3339()),
        "last_run": j.last_run.map(|t| t.to_rfc3339()),
        "run_count": j.run_count,
        "created_at": j.created_at.to_rfc3339(),
        "is_reminder": j.is_reminder,
        "owner": j.owner(),
        "visibility": j.effective_visibility(),
        "session": j.session_mode(),
        "max_runs": j.max_runs,
        "budget_usd": j.budget_usd,
        "expires_at": j.expires_at.map(|t| t.to_rfc3339()),
        "deliver_to": j.deliver_to,
    })
}

/// Serialize a JobRunRecord to a JSON value.
pub(crate) fn serialize_run_record(
    r: &crate::hub_core::schedules::JobRunRecord,
) -> serde_json::Value {
    serde_json::json!({
        "job_id": r.job_id,
        "job_name": r.job_name,
        "started_at": r.started_at.to_rfc3339(),
        "finished_at": r.finished_at.to_rfc3339(),
        "duration_ms": r.duration_ms,
        "success": r.success,
        "output": r.output,
        "delivered": r.delivered,
        "note": r.note,
    })
}

/// `GET /api/scheduler/{id}` — Get a single scheduled job.
async fn get_scheduled_job(
    State(state): State<Arc<HubState>>,
    Path(id): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(ref scheduler) = state.scheduler else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": "Scheduler not available" })),
        );
    };

    match scheduler.get_job(&id).await {
        Some(job) => (StatusCode::OK, Json(serialize_job(&job))),
        None => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": format!("Job '{}' not found", id) })),
        ),
    }
}

/// Request body for creating a scheduled job.
#[derive(Deserialize)]
struct CreateJobRequest {
    name: String,
    #[serde(default)]
    description: Option<String>,
    message: String,
    schedule: crate::hub_core::schedules::Schedule,
    #[serde(default)]
    channel_id: Option<String>,
    #[serde(default)]
    thread_id: Option<String>,
    #[serde(default)]
    model: Option<String>,
    /// When the job's instruction tells the agent to post its result itself,
    /// set this so the scheduler doesn't also deliver the agent's reply
    /// (avoids a duplicate post + a chatty "Done" confirmation).
    #[serde(default)]
    agent_owns_delivery: bool,
}

/// `POST /api/scheduler` — Create a new scheduled job.
async fn create_scheduled_job(
    State(state): State<Arc<HubState>>,
    Json(body): Json<CreateJobRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(ref scheduler) = state.scheduler else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": "Scheduler not available" })),
        );
    };

    let payload = crate::hub_core::schedules::JobPayload::AgentTurn {
        message: body.message,
        channel: None,
        channel_id: body.channel_id,
        thread_id: body.thread_id,
        model: body.model,
        user_id: None,
        max_rounds: None,
        agent_owns_delivery: body.agent_owns_delivery,
        tools: None,
    };

    let id = match scheduler
        .add_job_with_desc(
            &body.name,
            body.description.as_deref(),
            body.schedule,
            payload,
        )
        .await
    {
        Ok(id) => id,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": e })),
            );
        }
    };

    let job = scheduler.get_job(&id).await;
    (
        StatusCode::CREATED,
        Json(serde_json::json!({
            "created": true,
            "id": id,
            "job": job.as_ref().map(serialize_job),
        })),
    )
}

/// `PUT /api/scheduler/{id}` — Update a scheduled job.
async fn update_scheduled_job(
    State(state): State<Arc<HubState>>,
    caller: Option<Extension<CallerContext>>,
    Path(id): Path<String>,
    Json(edit): Json<crate::hub_core::schedules::JobEdit>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(ref scheduler) = state.scheduler else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": "Scheduler not available" })),
        );
    };

    let editor = caller
        .as_ref()
        .and_then(|Extension(caller)| caller.identity_id.as_deref())
        .unwrap_or(crate::hub_core::schedules::OWNER_TOKEN_EDITOR);
    match scheduler.update_job(&id, edit, editor).await {
        Ok(Some(job)) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "updated": true,
                "job": serialize_job(&job),
            })),
        ),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": format!("Job '{}' not found", id) })),
        ),
        Err(refused) => edit_refused(refused),
    }
}

/// How an update route answers an edit that was not applied: 400 with the
/// reason, or 409 with the forecast the caller re-sends `confirmed: true` to
/// approve.
pub(crate) fn edit_refused(
    refused: crate::hub_core::schedules::EditRefused,
) -> (StatusCode, Json<serde_json::Value>) {
    use crate::hub_core::schedules::EditRefused;
    match refused {
        EditRefused::Invalid(why) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": why })),
        ),
        EditRefused::NeedsConfirmation(cost) => (StatusCode::CONFLICT, Json(cost.to_json())),
    }
}

/// `DELETE /api/scheduler/{id}` — Delete a scheduled job.
async fn delete_scheduled_job(
    State(state): State<Arc<HubState>>,
    Path(id): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(ref scheduler) = state.scheduler else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": "Scheduler not available" })),
        );
    };

    if scheduler.remove_job(&id).await {
        (
            StatusCode::OK,
            Json(serde_json::json!({ "deleted": true, "id": id })),
        )
    } else {
        (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": format!("Job '{}' not found", id) })),
        )
    }
}

/// `POST /api/scheduler/{id}/run` — Trigger a scheduled job immediately.
async fn trigger_scheduled_job(
    State(state): State<Arc<HubState>>,
    Path(id): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(ref scheduler) = state.scheduler else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": "Scheduler not available" })),
        );
    };

    match scheduler.run_job_now(&id).await {
        Some(record) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "triggered": true,
                "run": serialize_run_record(&record),
            })),
        ),
        None => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": format!("Job '{}' not found", id) })),
        ),
    }
}

/// `GET /api/scheduler/{id}/history` — Get run history for a specific job.
async fn get_job_history(
    State(state): State<Arc<HubState>>,
    Path(id): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(ref scheduler) = state.scheduler else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": "Scheduler not available" })),
        );
    };

    // Verify the job exists first.
    if scheduler.get_job(&id).await.is_none() {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": format!("Job '{}' not found", id) })),
        );
    }

    let history = scheduler.job_run_history(&id).await;
    let records: Vec<_> = history.iter().map(serialize_run_record).collect();
    (
        StatusCode::OK,
        Json(serde_json::json!({ "history": records })),
    )
}

/// `GET /api/scheduler/history` — Get global run history across all jobs.
async fn get_global_history(State(state): State<Arc<HubState>>) -> Json<serde_json::Value> {
    let history = if let Some(ref scheduler) = state.scheduler {
        let records = scheduler.run_history().await;
        records.iter().map(serialize_run_record).collect::<Vec<_>>()
    } else {
        Vec::new()
    };

    Json(serde_json::json!({ "history": history }))
}

// ─── Workflow endpoints ───

/// The definitions of `kind` in `def_store` the caller sees, by the rule the
/// gateway's listings apply: an administrator sees every one, anyone else the
/// global ones, their own and those shared with them or their teams.
async fn definitions_seen_by(
    state: &HubState,
    def_store: Option<&Arc<workforce_orchestrator::workflow_store::WorkflowStore>>,
    caller: Option<&Extension<CallerContext>>,
    kind: &str,
) -> crate::resource_access::DefListing {
    let reader = caller_writer(caller);
    let teams = crate::team_reach::TeamReach::new(
        state
            .agent_registry()
            .and_then(|registry| registry.default_agent())
            .and_then(|service| service.team_store.read().clone()),
        reader.identity_id.clone(),
    );
    crate::resource_access::DefListing::resolve(def_store, reader, teams, kind).await
}

async fn list_workflows(
    State(state): State<Arc<HubState>>,
    caller: Option<Extension<CallerContext>>,
) -> Json<serde_json::Value> {
    let workflows = if let Some(ref runtime) = state.workflow_runtime {
        let listing = definitions_seen_by(
            &state,
            runtime.def_store(),
            caller.as_ref(),
            workforce_orchestrator::workflow_store::KIND_WORKFLOW,
        )
        .await;
        let defs = runtime.list_definitions_visible(&listing).await;
        defs.iter()
            .map(|d| {
                serde_json::json!({
                    "id": d.id,
                    "name": d.name,
                    "description": d.description,
                    "steps": d.step_count,
                })
            })
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    Json(serde_json::json!({ "workflows": workflows }))
}

/// GET /api/playbooks — playbook definition summaries (mirror of list_workflows).
async fn list_playbooks(
    State(state): State<Arc<HubState>>,
    caller: Option<Extension<CallerContext>>,
) -> Json<serde_json::Value> {
    let playbooks = if let Some(ref runtime) = state.playbook_runtime {
        let listing = definitions_seen_by(
            &state,
            runtime.def_store(),
            caller.as_ref(),
            workforce_orchestrator::workflow_store::KIND_PLAYBOOK,
        )
        .await;
        runtime
            .list_definitions_visible(&listing)
            .await
            .into_iter()
            .map(|p| {
                serde_json::json!({
                    "id": p.id,
                    "name": p.name,
                    "description": p.description,
                    "steps": p.step_count,
                })
            })
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    Json(serde_json::json!({ "playbooks": playbooks }))
}

/// GET /api/workflows/{id} — full workflow definition detail.
async fn get_workflow_definition(
    State(state): State<Arc<HubState>>,
    caller: Option<Extension<CallerContext>>,
    Path(id): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(ref runtime) = state.workflow_runtime else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": "Workflow runtime not available" })),
        );
    };

    match runtime
        .readable_definition(&id, &caller_writer(caller.as_ref()))
        .await
    {
        Some(def) => match serde_json::to_value(&def) {
            Ok(val) => (StatusCode::OK, Json(val)),
            Err(e) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": format!("Serialization error: {}", e) })),
            ),
        },
        None => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": format!("Workflow '{}' not found", id) })),
        ),
    }
}

#[derive(Deserialize)]
struct RunWorkflowPayload {
    #[serde(default)]
    payload: serde_json::Value,
    #[serde(default)]
    channel_id: Option<String>,
    #[serde(default)]
    thread_id: Option<String>,
}

async fn run_workflow(
    State(state): State<Arc<HubState>>,
    caller: Option<Extension<CallerContext>>,
    Path(id): Path<String>,
    Json(body): Json<RunWorkflowPayload>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(ref runtime) = state.workflow_runtime else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": "Workflow runtime not available" })),
        );
    };
    if runtime
        .readable_definition(&id, &caller_writer(caller.as_ref()))
        .await
        .is_none()
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": format!("Unknown workflow: '{id}'") })),
        );
    }

    // `start_workflow` persists the identity in scope as the run's `_run_as`.
    let start = Arc::clone(runtime).start_workflow(
        &id,
        body.payload,
        body.channel_id,
        body.thread_id,
        None,
        None,
    );
    match crate::user_config::as_principal(&caller_principal(caller), start).await {
        Ok(instance_id) => (
            StatusCode::CREATED,
            Json(serde_json::json!({
                "instance_id": instance_id,
                "workflow_id": id,
                "status": "running"
            })),
        ),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": e.to_string() })),
        ),
    }
}

#[derive(Deserialize)]
struct InstancesQuery {
    #[serde(default)]
    status: Option<String>,
    #[serde(default, alias = "workflow_id")]
    workflow: Option<String>,
}

async fn list_workflow_instances(
    State(state): State<Arc<HubState>>,
    caller: Option<Extension<CallerContext>>,
    Query(query): Query<InstancesQuery>,
) -> Json<serde_json::Value> {
    let instances = match state.workflow_runtime.as_ref() {
        Some(rt) => {
            let rows = rt
                .list_instances(query.status.as_deref(), query.workflow.as_deref())
                .unwrap_or_default();
            rt.readable_runs(rows, &caller_writer(caller.as_ref()))
                .await
        }
        None => Vec::new(),
    };

    let list: Vec<serde_json::Value> = instances
        .iter()
        .map(|row| {
            serde_json::json!({
                "instance_id": row.id,
                "workflow_id": row.workflow_id,
                "status": row.status,
                "current_step": row.current_step_id,
                "awaiting_decision": crate::workflow_runtime::awaits_decision(row),
                "created_at": row.created_at,
                "updated_at": row.updated_at,
            })
        })
        .collect();

    Json(serde_json::json!({ "instances": list, "count": list.len() }))
}

/// GET /api/workflow-instances/{id} — enhanced with step_history, variables, trigger, parent_instance_id.
async fn get_workflow_instance(
    State(state): State<Arc<HubState>>,
    caller: Option<Extension<CallerContext>>,
    Path(id): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(ref runtime) = state.workflow_runtime else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "Instance not found" })),
        );
    };

    match readable_instance(runtime.as_ref(), &id, caller.as_ref()).await {
        Ok(row) => {
            let step_history: serde_json::Value =
                serde_json::from_str(&row.step_history).unwrap_or(serde_json::json!([]));
            let variables: serde_json::Value =
                serde_json::from_str(&row.variables).unwrap_or(serde_json::json!({}));
            let trigger: serde_json::Value =
                serde_json::from_str(&row.trigger_json).unwrap_or(serde_json::json!(null));

            (
                StatusCode::OK,
                Json(serde_json::json!({
                    "instance_id": row.id,
                    "workflow_id": row.workflow_id,
                    "status": row.status,
                    "current_step": row.current_step_id,
                    "agent_id": row.agent_id,
                    "parent_instance_id": row.parent_instance_id,
                    "step_history": step_history,
                    "variables": variables,
                    "trigger": trigger,
                    "failure_reason": variables.get("_failure_reason"),
                    "decision": runtime.decision_view(
                        &row,
                        caller
                            .as_ref()
                            .and_then(|Extension(caller)| caller.identity_id.as_deref()),
                    ),
                    "created_at": row.created_at,
                    "updated_at": row.updated_at,
                })),
            )
        }
        Err(missing) => missing,
    }
}

/// GET /api/workflow-instances/{id}/steps — full step history with all StepRecord fields.
async fn get_instance_steps(
    State(state): State<Arc<HubState>>,
    caller: Option<Extension<CallerContext>>,
    Path(id): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(ref runtime) = state.workflow_runtime else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": "Workflow runtime not available" })),
        );
    };

    match readable_instance(runtime.as_ref(), &id, caller.as_ref()).await {
        Ok(row) => {
            let steps: Vec<StepRecord> =
                serde_json::from_str(&row.step_history).unwrap_or_default();
            let variables: serde_json::Value =
                serde_json::from_str(&row.variables).unwrap_or(serde_json::json!({}));
            let failure = variables.get("_failure_reason").and_then(|v| v.as_str());
            let views = step_views(&steps, &row.status, row.current_step_id.as_deref(), failure);
            (
                StatusCode::OK,
                Json(serde_json::json!({
                    "instance_id": id,
                    "steps": views,
                    "count": steps.len(),
                })),
            )
        }
        Err(missing) => missing,
    }
}

/// Longest agent response the steps view carries; the record keeps the rest.
const STEP_VIEW_RESPONSE_MAX: usize = 8_000;
/// Longest single output value the steps view carries.
const STEP_VIEW_OUTPUT_MAX: usize = 4_000;

/// A recorded step as the instance page shows it: its outcome, timing, the
/// engine and model that ran it, its cost and its outputs. A recorded failed
/// attempt is `failed`, and so is the step the instance failed on, which
/// also carries the failure; every other recorded step ran to completion.
fn step_views(
    steps: &[StepRecord],
    instance_status: &str,
    current_step: Option<&str>,
    failure: Option<&str>,
) -> Vec<serde_json::Value> {
    let last = steps.len().checked_sub(1);
    steps
        .iter()
        .enumerate()
        .map(|(i, step)| {
            let stopped_on = instance_status == "failed"
                && Some(i) == last
                && current_step == Some(step.step_id.as_str());
            let failed = step.failed || stopped_on;
            let duration_ms = step
                .completed_at
                .signed_duration_since(step.started_at)
                .num_milliseconds();
            let mut view = serde_json::json!({
                "step_id": step.step_id,
                "status": if failed { "failed" } else { "completed" },
                "outcome": step.outcome,
                "correction": step.correction,
                "started_at": step.started_at,
                "completed_at": step.completed_at,
                "duration_ms": duration_ms,
                "rounds": step.rounds,
                "cost_usd": step.cost_usd,
                "engine": step.provenance.engine,
                "model": step.provenance.model,
                "input_tokens": step.provenance.input_tokens,
                "output_tokens": step.provenance.output_tokens,
                "budget_capped": step.budget_capped,
                "outputs": step.outputs.as_ref().map(clip_values),
                "report": step.report,
                "raw_response": clip(&step.agent_response, STEP_VIEW_RESPONSE_MAX),
                "error": stopped_on
                    .then_some(failure)
                    .flatten()
                    .or(step.correction.as_ref().map(|c| c.detail.as_str())),
            });
            if let Some(first) = &step.provenance.decided_first {
                view["decided_first"] = serde_json::json!(first);
            }
            view
        })
        .collect()
}

fn clip(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((cut, _)) => format!("{}…", &text[..cut]),
        None => text.to_string(),
    }
}

fn clip_values(outputs: &serde_json::Value) -> serde_json::Value {
    match outputs {
        serde_json::Value::Object(map) => serde_json::Value::Object(
            map.iter()
                .map(|(k, v)| {
                    let clipped = match v {
                        serde_json::Value::String(s) => {
                            serde_json::Value::String(clip(s, STEP_VIEW_OUTPUT_MAX))
                        }
                        other => other.clone(),
                    };
                    (k.clone(), clipped)
                })
                .collect(),
        ),
        other => other.clone(),
    }
}

/// GET /api/workflow-instances/{id}/cost — total + per-step cost/rounds/duration.
async fn get_instance_cost(
    State(state): State<Arc<HubState>>,
    caller: Option<Extension<CallerContext>>,
    Path(id): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(ref runtime) = state.workflow_runtime else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": "Workflow runtime not available" })),
        );
    };

    match readable_instance(runtime.as_ref(), &id, caller.as_ref()).await {
        Ok(row) => {
            let steps: Vec<StepRecord> =
                serde_json::from_str(&row.step_history).unwrap_or_default();

            let total_cost_usd: f64 = steps.iter().map(|s| s.cost_usd).sum();
            let total_rounds: u32 = steps.iter().map(|s| s.rounds).sum();
            let total_duration_ms: i64 = steps
                .iter()
                .map(|s| {
                    s.completed_at
                        .signed_duration_since(s.started_at)
                        .num_milliseconds()
                })
                .sum();

            let per_step: Vec<serde_json::Value> = steps
                .iter()
                .map(|s| {
                    let duration_ms = s
                        .completed_at
                        .signed_duration_since(s.started_at)
                        .num_milliseconds();
                    serde_json::json!({
                        "step_id": s.step_id,
                        "cost_usd": s.cost_usd,
                        "rounds": s.rounds,
                        "duration_ms": duration_ms,
                        "budget_capped": s.budget_capped,
                        "outcome": s.outcome,
                        "failed": s.failed,
                    })
                })
                .collect();

            (
                StatusCode::OK,
                Json(serde_json::json!({
                    "instance_id": id,
                    "total_cost_usd": total_cost_usd,
                    "total_rounds": total_rounds,
                    "total_duration_ms": total_duration_ms,
                    "step_count": steps.len(),
                    "per_step": per_step,
                })),
            )
        }
        Err(missing) => missing,
    }
}

/// POST /api/workflow-instances/{id}/retry — retry the current failed step.
async fn retry_instance_step(
    State(state): State<Arc<HubState>>,
    Path(id): Path<String>,
    caller: Option<Extension<CallerContext>>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(ref runtime) = state.workflow_runtime else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": "Workflow runtime not available" })),
        );
    };

    let by = match steer(runtime.as_ref(), &id, caller, SteerAction::Retry).await {
        Ok(by) => by,
        Err(refused) => return refused,
    };
    match runtime.retry_step(&id, &by) {
        Ok(step_id) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "instance_id": id,
                "step_id": step_id,
                "status": "paused",
                "message": "Step queued for retry — call resume to re-execute",
            })),
        ),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": e.to_string() })),
        ),
    }
}

/// POST /api/workflow-instances/{id}/skip — skip the current step.
async fn skip_instance_step(
    State(state): State<Arc<HubState>>,
    Path(id): Path<String>,
    caller: Option<Extension<CallerContext>>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(ref runtime) = state.workflow_runtime else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": "Workflow runtime not available" })),
        );
    };

    let by = match steer(runtime.as_ref(), &id, caller, SteerAction::Skip).await {
        Ok(by) => by,
        Err(refused) => return refused,
    };
    match Arc::clone(runtime).skip_step(&id, &by).await {
        Ok(next_step_id) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "instance_id": id,
                "next_step_id": next_step_id,
                "status": "paused",
                "message": "Step skipped — call resume to continue execution",
            })),
        ),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": e.to_string() })),
        ),
    }
}

#[derive(Deserialize)]
struct GotoPayload {
    step_id: String,
}

/// POST /api/workflow-instances/{id}/goto — jump to a specific step.
async fn goto_instance_step(
    State(state): State<Arc<HubState>>,
    Path(id): Path<String>,
    caller: Option<Extension<CallerContext>>,
    Json(body): Json<GotoPayload>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(ref runtime) = state.workflow_runtime else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": "Workflow runtime not available" })),
        );
    };

    let by = match steer(runtime.as_ref(), &id, caller, SteerAction::Goto).await {
        Ok(by) => by,
        Err(refused) => return refused,
    };
    match runtime.goto_step(&id, &body.step_id, &by).await {
        Ok(()) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "instance_id": id,
                "step_id": body.step_id,
                "status": "paused",
                "message": "Jumped to step — call resume to execute from here",
            })),
        ),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": e.to_string() })),
        ),
    }
}

/// POST /api/workflow-instances/{id}/pause — pause a running workflow.
async fn pause_instance(
    State(state): State<Arc<HubState>>,
    Path(id): Path<String>,
    caller: Option<Extension<CallerContext>>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(ref runtime) = state.workflow_runtime else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": "Workflow runtime not available" })),
        );
    };

    let by = match steer(runtime.as_ref(), &id, caller, SteerAction::Pause).await {
        Ok(by) => by,
        Err(refused) => return refused,
    };
    match runtime.pause_workflow(&id, &by).await {
        Ok(()) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "instance_id": id,
                "status": "paused",
                "message": "Workflow paused",
            })),
        ),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": e.to_string() })),
        ),
    }
}

/// GET /api/workflow-instances/{id}/variables — get current variable values.
async fn get_instance_variables(
    State(state): State<Arc<HubState>>,
    caller: Option<Extension<CallerContext>>,
    Path(id): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(ref runtime) = state.workflow_runtime else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": "Workflow runtime not available" })),
        );
    };

    match readable_instance(runtime.as_ref(), &id, caller.as_ref()).await {
        Ok(row) => {
            let variables: serde_json::Value =
                serde_json::from_str(&row.variables).unwrap_or(serde_json::json!({}));
            (
                StatusCode::OK,
                Json(serde_json::json!({
                    "instance_id": id,
                    "variables": variables,
                })),
            )
        }
        Err(missing) => missing,
    }
}

#[derive(Deserialize)]
struct SetVariableBody {
    value: String,
}

/// PUT /api/workflow-instances/{id}/variables/{key} — set a variable value.
async fn set_instance_variable_handler(
    State(state): State<Arc<HubState>>,
    Path((id, key)): Path<(String, String)>,
    caller: Option<Extension<CallerContext>>,
    Json(body): Json<SetVariableBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(ref runtime) = state.workflow_runtime else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": "Workflow runtime not available" })),
        );
    };

    match runtime.set_operator_variable(&caller_principal(caller), &id, &key, &body.value) {
        Ok(()) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "instance_id": id,
                "key": key,
                "value": body.value,
                "message": "Variable updated",
            })),
        ),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": e.to_string() })),
        ),
    }
}

#[derive(Deserialize)]
struct ResumePayload {
    #[serde(default)]
    outcome: Option<String>,
    /// The open decision's gate, from the instance's `decision`.
    #[serde(default)]
    gate: Option<String>,
    #[serde(default)]
    channel_id: Option<String>,
    #[serde(default)]
    thread_id: Option<String>,
}

async fn resume_workflow_instance(
    State(state): State<Arc<HubState>>,
    Path(id): Path<String>,
    caller: Option<Extension<CallerContext>>,
    Json(body): Json<ResumePayload>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(ref runtime) = state.workflow_runtime else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": "Workflow runtime not available" })),
        );
    };

    let gate = match body.gate.as_deref().map(uuid::Uuid::parse_str).transpose() {
        Ok(gate) => gate,
        Err(_) => return refusal(StatusCode::CONFLICT, &DecisionRefused::Outdated),
    };
    let steerer = Steerer {
        writer: caller_writer(caller.as_ref()),
        actor: caller_actor(caller),
    };
    let ask = ResumeAsk {
        outcome: body.outcome.clone(),
        gate,
        feedback: None,
        channel_id: body.channel_id,
        thread_id: body.thread_id,
        originating_agent_id: None,
    };
    match Arc::clone(runtime).resume_as(&id, ask, &steerer).await {
        Ok(Resumed::Moved { applied }) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "instance_id": id,
                "status": "running",
                "outcome_applied": applied,
                "message": crate::workflow_runtime::resumed_message(body.outcome.as_deref(), applied, "'")
            })),
        ),
        Ok(Resumed::Decided(made)) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "instance_id": id,
                "status": "running",
                "decided": {
                    "outcome": made.outcome,
                    "label": made.label,
                    "summary": made.summary,
                },
            })),
        ),
        Err(refused) => refusal(resume_status(&refused), &refused),
    }
}

async fn abort_workflow_instance(
    State(state): State<Arc<HubState>>,
    Path(id): Path<String>,
    caller: Option<Extension<CallerContext>>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(ref runtime) = state.workflow_runtime else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": "Workflow runtime not available" })),
        );
    };

    let by = match steer(runtime.as_ref(), &id, caller, SteerAction::Abort).await {
        Ok(by) => by,
        Err(refused) => return refused,
    };
    match Arc::clone(runtime).abort_workflow(&id, &by).await {
        Ok(()) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "instance_id": id,
                "status": "cancelled",
                "message": "Workflow cancelled"
            })),
        ),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": e.to_string() })),
        ),
    }
}

#[derive(Deserialize)]
struct RejectPayload {
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    feedback: Option<String>,
    #[serde(default)]
    channel_id: Option<String>,
    #[serde(default)]
    thread_id: Option<String>,
}

async fn reject_workflow_instance(
    State(state): State<Arc<HubState>>,
    Path(id): Path<String>,
    caller: Option<Extension<CallerContext>>,
    Json(body): Json<RejectPayload>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(ref runtime) = state.workflow_runtime else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": "Workflow runtime not available" })),
        );
    };

    let by = match steer(runtime.as_ref(), &id, caller, SteerAction::Reject).await {
        Ok(by) => by,
        Err(refused) => return refused,
    };
    match Arc::clone(runtime)
        .reject_workflow_step(
            &id,
            body.reason.as_deref(),
            body.feedback.as_deref(),
            body.channel_id,
            body.thread_id,
            None,
            &by,
        )
        .await
    {
        Ok(applied) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "instance_id": id,
                "status": "running",
                "outcome_applied": applied,
                "message": crate::workflow_runtime::resumed_message(
                    Some(crate::workflow_runtime::REJECTED),
                    applied,
                    "'"
                )
            })),
        ),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": e.to_string() })),
        ),
    }
}

#[derive(Deserialize)]
struct SessionHistoryQuery {
    agent: Option<String>,
    days: Option<u32>,
    limit: Option<usize>,
}

async fn session_history(
    State(state): State<Arc<HubState>>,
    Query(query): Query<SessionHistoryQuery>,
) -> Json<serde_json::Value> {
    let workspace_dir = std::path::Path::new(&state.config.hub.data_dir).join("agents");
    let limit = query.limit.unwrap_or(20).min(100);
    let mut all_sessions = Vec::new();

    // Read all agent directories
    if let Ok(entries) = std::fs::read_dir(&workspace_dir) {
        for entry in entries.flatten() {
            let agent_dir = entry.path();
            if !agent_dir.is_dir() {
                continue;
            }

            let sessions_dir = agent_dir.join("sessions");
            if !sessions_dir.exists() {
                continue;
            }

            // Read session metadata files
            if let Ok(session_entries) = std::fs::read_dir(&sessions_dir) {
                for file in session_entries.flatten() {
                    if file.path().extension() == Some(std::ffi::OsStr::new("json"))
                        && file.file_name().to_string_lossy().ends_with(".meta.json")
                    {
                        if let Ok(content) = std::fs::read_to_string(file.path()) {
                            if let Ok(meta) = serde_json::from_str::<serde_json::Value>(&content) {
                                all_sessions.push(meta);
                            }
                        }
                    }
                }
            }
        }
    }

    // Filter by agent if specified
    if let Some(agent_filter) = &query.agent {
        all_sessions.retain(|s| s.get("owner").and_then(|v| v.as_str()) == Some(agent_filter));
    }

    // Filter by days if specified
    if let Some(days) = query.days {
        let cutoff = chrono::Utc::now() - chrono::Duration::days(days as i64);
        all_sessions.retain(|s| {
            s.get("created_at")
                .and_then(|v| v.as_str())
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                .map(|dt| dt.with_timezone(&chrono::Utc) >= cutoff)
                .unwrap_or(false)
        });
    }

    // Sort by created_at descending
    all_sessions.sort_by(|a, b| {
        let a_time = a.get("created_at").and_then(|v| v.as_str()).unwrap_or("");
        let b_time = b.get("created_at").and_then(|v| v.as_str()).unwrap_or("");
        b_time.cmp(a_time)
    });

    // Limit results
    all_sessions.truncate(limit);

    // Format results
    let results: Vec<_> = all_sessions
        .iter()
        .map(|s| {
            let created = s.get("created_at").and_then(|v| v.as_str()).unwrap_or("");
            let updated = s.get("updated_at").and_then(|v| v.as_str()).unwrap_or("");

            let duration_mins = if let (Ok(c), Ok(u)) = (
                chrono::DateTime::parse_from_rfc3339(created),
                chrono::DateTime::parse_from_rfc3339(updated),
            ) {
                u.signed_duration_since(c).num_minutes()
            } else {
                0
            };

            serde_json::json!({
                "id": s.get("id").and_then(|v| v.as_str()).unwrap_or(""),
                "agent": s.get("owner").and_then(|v| v.as_str()).unwrap_or(""),
                "label": s.get("label").and_then(|v| v.as_str()).unwrap_or(""),
                "created_at": created,
                "duration_mins": duration_mins,
                "messages": s.get("message_count").and_then(|v| v.as_u64()).unwrap_or(0),
                "tokens": s.get("total_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
            })
        })
        .collect();

    Json(serde_json::json!({ "sessions": results }))
}

/// Query for `POST /api/sessions/redact`.
#[derive(Debug, Deserialize)]
struct RedactQuery {
    /// Restrict to one agent; omit to sweep every agent the hub serves.
    agent: Option<String>,
    /// Report what would change without writing.
    #[serde(default)]
    dry_run: bool,
}

/// `POST /api/sessions/redact` — operator-triggered secret redaction over
/// stored sessions. Runs in-process through each agent's session store so
/// the cache stays coherent with disk.
async fn redact_sessions(
    State(state): State<Arc<HubState>>,
    Query(query): Query<RedactQuery>,
) -> Json<serde_json::Value> {
    let stores: Vec<Arc<dyn workforce_common::session::SessionStore>> = {
        let Some(registry) = state.agent_registry() else {
            return Json(serde_json::json!({ "error": "agent registry unavailable" }));
        };
        match query.agent.as_deref() {
            Some(agent) => registry
                .get(agent)
                .into_iter()
                .map(|s| Arc::clone(&s.sessions))
                .collect(),
            None => registry
                .all_services()
                .into_iter()
                .map(|s| Arc::clone(&s.sessions))
                .collect(),
        }
    };
    if stores.is_empty() {
        return Json(serde_json::json!({ "error": "no matching agent" }));
    }

    let dry_run = query.dry_run;
    let report = tokio::task::spawn_blocking(move || {
        stores.iter().fold(
            crate::session_redaction::RedactionReport::default(),
            |mut acc, store| {
                let r = crate::session_redaction::run_manual_sweep(store, dry_run);
                acc.sessions_scanned += r.sessions_scanned;
                acc.sessions_redacted += r.sessions_redacted;
                acc.messages_redacted += r.messages_redacted;
                acc.archive_messages_redacted += r.archive_messages_redacted;
                acc
            },
        )
    })
    .await
    .unwrap_or_default();

    Json(serde_json::json!({
        "dry_run": dry_run,
        "sessions_scanned": report.sessions_scanned,
        "sessions_redacted": report.sessions_redacted,
        "messages_redacted": report.messages_redacted,
        "archive_messages_redacted": report.archive_messages_redacted,
    }))
}

// ─── Sub-agent management (DW-428) ───

/// Who an admin request reads sub-agent tasks as. The router admits the
/// `admin` scope, which a caller who is no administrator may hold: such a
/// caller sees the tasks run for them, as the `subagent` tool shows a turn.
fn caller_reader(caller: Option<Extension<CallerContext>>) -> JobReader {
    let caller = caller.map(|Extension(caller)| caller).unwrap_or_default();
    JobReader::new(caller.is_admin, TurnPrincipal::run_as(caller.identity_id))
}

/// `GET /api/subagents` — List the sub-agent tasks the caller may see.
async fn list_subagents(
    State(state): State<Arc<HubState>>,
    caller: Option<Extension<CallerContext>>,
) -> Json<serde_json::Value> {
    let Some(ref runner) = state.agent_jobs else {
        return Json(serde_json::json!({ "tasks": [], "count": 0 }));
    };

    let reader = caller_reader(caller);
    let list: Vec<serde_json::Value> = runner
        .list_by_type(crate::hub_core::jobs::JobType::Agent)
        .await
        .iter()
        .filter(|m| m.readable_by(&reader))
        .map(|m| {
            serde_json::json!({
                "task_id": m.job_id,
                "task": m.agent_task_text().chars().take(200).collect::<String>(),
                "status": m.agent_status_label(),
                "spawning_agent": m.agent_id,
                "model": m.agent_model(),
                "started_at": m.started_at.unwrap_or(m.submitted_at).to_rfc3339(),
                "finished_at": m.completed_at.map(|f| f.to_rfc3339()),
                "elapsed_secs": m.completed_at.is_none().then(|| m.elapsed_secs() as i64),
            })
        })
        .collect();

    Json(serde_json::json!({ "tasks": list, "count": list.len() }))
}

/// `GET /api/subagents/{id}` — Get a single sub-agent task with full detail.
async fn get_subagent(
    State(state): State<Arc<HubState>>,
    caller: Option<Extension<CallerContext>>,
    Path(id): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(ref runner) = state.agent_jobs else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": "Background execution not available" })),
        );
    };

    match runner.job_detail_for(&id, &caller_reader(caller)).await {
        Some((manifest, output)) => {
            let started = manifest.started_at.unwrap_or(manifest.submitted_at);
            let result_preview = output
                .as_ref()
                .filter(|o| !o.full_output.is_empty())
                .map(|o| workforce_common::text::truncate_str(&o.full_output, 500).to_string());

            (
                StatusCode::OK,
                Json(serde_json::json!({
                    "task_id": manifest.job_id,
                    "task": manifest.agent_task_text(),
                    "status": manifest.agent_status_label(),
                    "spawning_agent": manifest.agent_id,
                    "model": manifest.agent_model(),
                    "started_at": started.to_rfc3339(),
                    "finished_at": manifest.completed_at.map(|f| f.to_rfc3339()),
                    "elapsed_secs": manifest.completed_at.is_none().then(|| manifest.elapsed_secs() as i64),
                    "duration_secs": manifest.completed_at.map(|_| manifest.elapsed_secs() as i64),
                    "result_preview": result_preview,
                    "error": manifest.error,
                })),
            )
        }
        None => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": format!("Task '{}' not found", id) })),
        ),
    }
}

/// `POST /api/subagents/{id}/cancel` — Cancel a running sub-agent task.
async fn cancel_subagent(
    State(state): State<Arc<HubState>>,
    caller: Option<Extension<CallerContext>>,
    Path(id): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(ref runner) = state.agent_jobs else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": "Background execution not available" })),
        );
    };

    let cancelled = match runner.job_detail_for(&id, &caller_reader(caller)).await {
        Some(_) => runner.cancel(&id).await,
        None => Ok(false),
    };
    match cancelled {
        Ok(true) => (
            StatusCode::OK,
            Json(serde_json::json!({ "task_id": id, "status": "cancelling" })),
        ),
        _ => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({
                "error": format!("Task '{}' not found or already finished", id)
            })),
        ),
    }
}

// ─── Event source management (DW-428) ───

/// `GET /api/events/sources` — List all registered event sources with current state.
async fn list_event_sources(State(state): State<Arc<HubState>>) -> Json<serde_json::Value> {
    let Some(ref manager) = state.event_source_manager else {
        return Json(serde_json::json!({ "sources": [], "count": 0 }));
    };

    let sources = manager.status().await;
    let list: Vec<serde_json::Value> = sources
        .iter()
        .map(|s| {
            serde_json::json!({
                "id": s.id,
                "name": s.name,
                "source_type": s.source_type,
                "status": s.status,
                "events_received": s.events_received,
                "last_event": s.last_event,
                "started_at": s.started_at,
            })
        })
        .collect();

    Json(serde_json::json!({ "sources": list, "count": list.len() }))
}

/// `POST /api/events/sources/{id}/stop` — Stop a running event source.
async fn stop_event_source(
    State(state): State<Arc<HubState>>,
    Path(id): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(ref manager) = state.event_source_manager else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": "Event source manager not available" })),
        );
    };

    // Verify source exists
    if manager.get_status(&id).await.is_none() {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": format!("Event source '{}' not found", id) })),
        );
    }

    manager.stop(&id).await;
    (
        StatusCode::OK,
        Json(serde_json::json!({ "id": id, "status": "stopped" })),
    )
}

/// `POST /api/events/sources/{id}/start` — Start a stopped event source.
///
/// NOTE: Event sources are managed by the gateway process and require a live
/// future to run (e.g. a WebSocket connection task). Starting an event source
/// via the REST API is not supported — the source will reconnect automatically
/// on next gateway startup. Use the `system.reload_slack` tool for Slack sources.
async fn start_event_source(
    State(state): State<Arc<HubState>>,
    Path(id): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(ref manager) = state.event_source_manager else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": "Event source manager not available" })),
        );
    };

    match manager.get_status(&id).await {
        None => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": format!("Event source '{}' not found", id) })),
        ),
        Some(_) => (
            StatusCode::NOT_IMPLEMENTED,
            Json(serde_json::json!({
                "error": "Starting event sources via the REST API is not supported. Sources are managed by the gateway and reconnect automatically on restart. Use the system.reload_slack tool for Slack sources.",
                "id": id,
            })),
        ),
    }
}

/// `POST /api/events/sources/{id}/restart` — Restart an event source (stop then start).
async fn restart_event_source(
    State(state): State<Arc<HubState>>,
    Path(id): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(ref manager) = state.event_source_manager else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": "Event source manager not available" })),
        );
    };

    if manager.get_status(&id).await.is_none() {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": format!("Event source '{}' not found", id) })),
        );
    }

    // Stop the current task handle if running. The source will reconnect
    // automatically on the next gateway startup cycle.
    manager.stop(&id).await;
    (
        StatusCode::OK,
        Json(serde_json::json!({
            "id": id,
            "status": "stopped",
            "message": "Event source stopped. It will reconnect automatically on next gateway startup.",
        })),
    )
}

// ─── Knowledge management ───

fn check_api_auth(
    state: &HubState,
    headers: &HeaderMap,
) -> Result<(), (StatusCode, Json<serde_json::Value>)> {
    let Some(ref expected) = state.auth_token else {
        return Ok(());
    };
    let token = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "));
    match token {
        Some(t) if crate::auth::constant_time_token_eq(t, expected) => Ok(()),
        _ => Err((
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({"error": "Unauthorized"})),
        )),
    }
}

#[derive(Deserialize)]
struct KnowledgeStatusQuery {
    #[serde(default)]
    path: Option<String>,
}

async fn knowledge_status(
    State(state): State<Arc<HubState>>,
    Query(query): Query<KnowledgeStatusQuery>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(ref ks) = state.knowledge_service else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": "Knowledge service not available" })),
        );
    };

    let root = if let Some(ref rel_path) = query.path {
        let workspace = std::path::PathBuf::from(&state.config.hub.data_dir);
        let resolved = workspace.join(rel_path);
        match std::fs::canonicalize(&resolved) {
            Ok(p) => p,
            Err(_) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({
                        "error": format!("Path '{}' does not exist", rel_path)
                    })),
                );
            }
        }
    } else {
        ks.workspace().to_path_buf()
    };

    let repo_id = workforce_knowledge_service::KnowledgeService::repo_id_for(&root);
    let is_indexing = ks.is_indexing_repo(&repo_id);
    let is_indexing_any = ks.is_indexing_any();

    // Get graph stats
    let stats_result = ks.graph().get_stats(&repo_id).await;
    let index_state_result = ks.graph().get_index_state(&repo_id).await;

    match (stats_result, index_state_result) {
        (Ok(stats), Ok(index_state)) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "repo_id": repo_id,
                "repo_name": workforce_knowledge_service::KnowledgeService::repo_name_for(&root),
                "workspace": root.to_string_lossy(),
                "indexed": index_state.is_some(),
                "indexing_in_progress": is_indexing,
                "any_indexing_in_progress": is_indexing_any,
                "last_commit": index_state.as_ref().and_then(|s| s.last_commit.clone()),
                "last_full_index_at": index_state.as_ref().and_then(|s| s.last_full_index_at.clone()),
                "last_incremental_at": index_state.as_ref().and_then(|s| s.last_incremental_at.clone()),
                "total_entities": stats.entity_count,
                "total_relationships": stats.relationship_count,
                "total_files": stats.file_count,
            })),
        ),
        (Err(e), _) | (_, Err(e)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        ),
    }
}

#[derive(Deserialize)]
struct TriggerIndexRequest {
    #[serde(default)]
    mode: Option<String>,
    #[serde(default)]
    path: Option<String>,
}

/// `POST /api/knowledge/index` — Trigger an index rebuild (incremental or full).
async fn trigger_knowledge_index(
    State(state): State<Arc<HubState>>,
    headers: HeaderMap,
    Json(body): Json<TriggerIndexRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    if let Err(e) = check_api_auth(&state, &headers) {
        return e;
    }

    let Some(ref ks) = state.knowledge_service else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": "Knowledge service not available" })),
        );
    };

    let root = if let Some(ref rel_path) = body.path {
        let workspace = std::path::PathBuf::from(&state.config.hub.data_dir);
        let resolved = workspace.join(rel_path);
        match std::fs::canonicalize(&resolved) {
            Ok(p) => p,
            Err(_) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({
                        "error": format!("Path '{}' does not exist", rel_path)
                    })),
                );
            }
        }
    } else {
        ks.workspace().to_path_buf()
    };

    let repo_id = workforce_knowledge_service::KnowledgeService::repo_id_for(&root);

    if ks.is_indexing_repo(&repo_id) {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "error": "Indexing already in progress for this repo",
                "repo_id": repo_id,
            })),
        );
    }

    let full = body.mode.as_deref() == Some("full");
    let ks_clone = Arc::clone(ks);
    let root_clone = root.clone();

    // Spawn indexing in background
    tokio::spawn(async move {
        if full {
            let _ = ks_clone.trigger_full_index_for(&root_clone).await;
        } else {
            let _ = ks_clone.trigger_incremental_index_for(&root_clone).await;
        }
    });

    (
        StatusCode::ACCEPTED,
        Json(serde_json::json!({
            "repo_id": repo_id,
            "mode": if full { "full" } else { "incremental" },
            "status": "indexing_started",
            "message": "Indexing started in background. Poll GET /api/knowledge/status to check progress.",
        })),
    )
}

/// `POST /api/knowledge/index/cancel` — Cancel a running index operation.
async fn cancel_knowledge_index(
    State(state): State<Arc<HubState>>,
    headers: HeaderMap,
    Query(query): Query<KnowledgeStatusQuery>,
) -> (StatusCode, Json<serde_json::Value>) {
    if let Err(e) = check_api_auth(&state, &headers) {
        return e;
    }

    let Some(ref ks) = state.knowledge_service else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": "Knowledge service not available" })),
        );
    };

    let root = if let Some(ref rel_path) = query.path {
        let workspace = std::path::PathBuf::from(&state.config.hub.data_dir);
        workspace.join(rel_path)
    } else {
        ks.workspace().to_path_buf()
    };

    let repo_id = workforce_knowledge_service::KnowledgeService::repo_id_for(&root);

    if ks.cancel_indexing(&repo_id).await {
        (
            StatusCode::OK,
            Json(serde_json::json!({ "repo_id": repo_id, "cancelled": true })),
        )
    } else {
        (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({
                "error": format!("No active indexing operation for repo '{}'", repo_id),
                "repo_id": repo_id,
            })),
        )
    }
}

// ─── System logs (DW-428) ───

#[derive(Deserialize)]
struct LogsQuery {
    #[serde(default)]
    action: Option<String>,
    #[serde(default)]
    lines: Option<usize>,
    #[serde(default)]
    level: Option<String>,
    #[serde(default)]
    target: Option<String>,
    #[serde(default)]
    keyword: Option<String>,
    #[serde(default)]
    since: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
}

/// `GET /api/logs` — Access system log buffer (read or search).
async fn system_logs(Query(query): Query<LogsQuery>) -> (StatusCode, Json<serde_json::Value>) {
    use workforce_common::logging::LOG_BUFFER;

    let buffer = match LOG_BUFFER.get() {
        Some(b) => b,
        None => {
            // Log buffer not yet initialised (common in tests / early startup).
            // Return an empty result rather than 503 so the endpoint is always usable.
            return (
                StatusCode::OK,
                Json(
                    serde_json::json!({ "lines": [], "count": 0, "note": "Log buffer not yet initialised" }),
                ),
            );
        }
    };

    let action = query.action.as_deref().unwrap_or("read");

    match action {
        "search" => {
            let limit = query.limit.unwrap_or(100).min(500);
            let since: Option<chrono::DateTime<chrono::Utc>> = query
                .since
                .as_deref()
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                .map(|dt| dt.with_timezone(&chrono::Utc));

            let entries = buffer.search(
                query.level.as_deref(),
                query.target.as_deref(),
                query.keyword.as_deref(),
                since,
                limit,
            );

            let lines: Vec<serde_json::Value> = entries
                .iter()
                .map(|e| {
                    serde_json::json!({
                        "timestamp": e.timestamp.to_rfc3339(),
                        "level": e.level,
                        "target": e.target,
                        "message": e.message,
                    })
                })
                .collect();

            (
                StatusCode::OK,
                Json(serde_json::json!({ "entries": lines, "count": lines.len() })),
            )
        }
        _ => {
            // Default: read last N lines
            let n = query.lines.unwrap_or(50).min(500);
            let entries = buffer.read_last(n);
            let lines: Vec<serde_json::Value> = entries
                .iter()
                .map(|e| {
                    serde_json::json!({
                        "timestamp": e.timestamp.to_rfc3339(),
                        "level": e.level,
                        "target": e.target,
                        "message": e.message,
                    })
                })
                .collect();

            (
                StatusCode::OK,
                Json(serde_json::json!({ "entries": lines, "count": lines.len() })),
            )
        }
    }
}

#[cfg(test)]
mod code_vectors_tests {
    use workforce_codebase_knowledge::graph::vectors::{FillStage, VectorFill};

    use workforce_knowledge_service::RepoVectors;

    /// Health says which backend each of the three stores is on, as the
    /// config served has it.
    #[tokio::test]
    async fn health_says_where_each_store_is() {
        let mut config = workforce_common::settings::WorkforceConfig::default();
        config.memory.backend = "vellev".into();
        super::note_store_backends(&config);
        let axum::Json(health) = super::health(axum::extract::State(std::sync::Arc::new(
            crate::state::HubState::for_tests(config.clone()),
        )))
        .await;
        assert!(health.get("shadow").is_some(), "{health}");
        let stores = health["stores"].as_object().expect("the stores");
        let mut named: Vec<&str> = stores.keys().map(String::as_str).collect();
        named.sort();
        assert_eq!(named, ["artifact_index", "code_vectors", "memories"]);
        // The mirror's outline says its lease and how often it lapsed.
        let lease = workforce_orchestrator::artifact_store::LeaseState {
            until: Some(chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap()),
            lapses: 2,
            keeper_alive: false,
            keeper_stalled: true,
            progress_age: Some(std::time::Duration::from_secs(150)),
        };
        let outline = super::mirror_outline(
            &workforce_orchestrator::artifact_store::IndexState::default(),
            &lease,
        );
        assert_eq!(outline["lease_lapses"], 2);
        assert_eq!(outline["keeper_alive"], false);
        assert_eq!(outline["keeper_stalled"], true);
        assert_eq!(outline["progress_age_secs"], 150);
        assert_eq!(outline["lease_until"], "2023-11-14T22:13:20Z");
        assert!(outline.get("pending").is_some(), "{outline}");
        // No mirror in this process: said, as null (DW-676).
        assert_eq!(
            health.get("artifact_index_mirror"),
            Some(&serde_json::Value::Null),
            "{health}"
        );
        // Whichever config this process noted first: a backend each.
        for backend in stores.values() {
            let backend = backend.as_str().expect("a backend's name");
            assert!(!backend.is_empty(), "{stores:?}");
        }
    }

    /// What is read without a key counts repos and names none, and
    /// repeats no error.
    #[test]
    fn the_outline_of_the_code_vectors_names_no_repo_and_repeats_no_error() {
        let fill = |stage, done| VectorFill {
            generation: "g".into(),
            collection: "c".into(),
            stage,
            cursor: "secret-entity".into(),
            done,
            total: 4_200_000,
        };
        let repos = [
            RepoVectors {
                repo_id: "payroll-1a2b".into(),
                name: "payroll".into(),
                complete: false,
                fill: Some(fill(FillStage::Entities, 1_000_000)),
                pending_files: 2,
                last_error: Some("vellev at http://internal:8080 refused tenant acme".into()),
                compared_secs_ago: None,
            },
            RepoVectors {
                repo_id: "site-3c4d".into(),
                name: "site".into(),
                complete: true,
                fill: Some(fill(FillStage::Done, 4_200_000)),
                pending_files: 1,
                last_error: None,
                compared_secs_ago: Some(120),
            },
        ];
        let outline = super::code_vectors_outline(&repos);
        assert_eq!(
            outline,
            serde_json::json!({
                "repos": 2,
                "complete": 1,
                "filling": 1,
                "stored": 1_000_000,
                "to_store": 4_200_000,
                "pending_files": 3,
                "errors": 1,
                "uncompared": 1,
                "oldest_compare_secs": 120,
            })
        );
        let said = outline.to_string();
        assert!(
            !said.contains("payroll") && !said.contains("internal") && !said.contains("secret")
        );
        assert_eq!(super::code_vectors_outline(&[]), serde_json::Value::Null);
        let whole = serde_json::json!(repos).to_string();
        assert!(whole.contains("payroll") && whole.contains("refused tenant acme"));
    }
}

#[cfg(test)]
mod artifact_index_tests {
    use workforce_orchestrator::artifact_store::{
        BackfillProgress, Backfilled, Failing, IndexFailure, IndexState,
    };

    /// What is read without a key says how the index stands and names
    /// neither an artifact nor an error's words.
    #[test]
    fn the_outline_of_the_artifact_index_names_no_artifact_and_repeats_no_error() {
        let state = IndexState {
            configured: true,
            complete: Some(false),
            backfill: Some(BackfillProgress {
                indexed: 40,
                total: 90,
            }),
            last_backfill: Some(Backfilled {
                indexed: 88,
                total: 90,
                removed: 1,
                failed: 2,
                failures: vec![IndexFailure {
                    id: "secret-plan".into(),
                    error: "vellev at http://internal:8080 refused tenant acme".into(),
                }],
            }),
            reconciled_at: None,
            failing: vec![Failing {
                id: "secret-memo".into(),
                tries: 2,
                first_failed_at: Some(chrono::Utc::now() - chrono::Duration::seconds(7200)),
                next_try_at: None,
                error: "vellev at http://internal:8080 answered 500".into(),
            }],
            failing_at_cap: 1,
            oldest_failure_at: Some(chrono::Utc::now() - chrono::Duration::seconds(7200)),
            search: Some("keyword"),
            counted_at: None,
            not_ours: Some("made by Weaviate at http://internal:8080 itself".into()),
            in_step: Some(false),
            pending: 3,
            last_error: Some("2 of 90 artifacts could not be indexed: 'secret-plan'".into()),
            search_breaker: Some("open"),
        };
        let outline = super::artifact_index_outline(&state);
        let oldest = outline["oldest_failure_secs"].as_i64().unwrap();
        assert!((7200..7260).contains(&oldest), "{oldest}");
        assert_eq!(
            outline,
            serde_json::json!({
                "configured": true,
                "complete": false,
                "backfill": { "indexed": 40, "total": 90 },
                "pending": 3,
                "failing": 1,
                "failing_at_cap": 1,
                "oldest_failure_secs": outline["oldest_failure_secs"].clone(),
                "search": "keyword",
                "counted_at": null,
                "not_ours": true,
                "failed": 2,
                "error": true,
                "search_breaker": "open",
            })
        );
        let said = outline.to_string();
        assert!(
            !said.contains("secret-plan") && !said.contains("internal"),
            "{said}"
        );
        let whole = serde_json::json!(state).to_string();
        assert!(whole.contains("secret-plan") && whole.contains("refused tenant acme"));
        // The administrator is told which are failing, how often each was
        // tried, and what the index said.
        let whole = serde_json::json!(state);
        assert_eq!(whole["failing"][0]["id"], "secret-memo");
        assert_eq!(whole["failing"][0]["tries"], 2);
        assert!(whole["failing"][0]["error"]
            .as_str()
            .unwrap()
            .contains("answered 500"));
        assert!(!said.contains("secret-memo"));
    }
}

#[cfg(test)]
mod step_view_tests {
    use super::*;
    use chrono::{Duration, Utc};
    use workforce_orchestrator::workflow::instance::StepProvenance;

    fn record(step_id: &str, response: &str, outputs: Option<serde_json::Value>) -> StepRecord {
        let started_at = Utc::now();
        StepRecord {
            step_id: step_id.to_string(),
            outcome: None,
            agent_response: response.to_string(),
            outputs,
            report: None,
            started_at,
            completed_at: started_at + Duration::seconds(41),
            rounds: 1,
            cost_usd: 0.11,
            budget_capped: false,
            failed: false,
            correction: None,
            provenance: StepProvenance {
                engine: Some("codex".into()),
                model: Some("gpt-5.6-sol".into()),
                ..StepProvenance::default()
            },
        }
    }

    #[test]
    fn recorded_steps_carry_status_timing_provenance_and_clipped_outputs() {
        let long = "x".repeat(STEP_VIEW_OUTPUT_MAX + 5);
        let steps = vec![
            record(
                "setup",
                "ok",
                Some(serde_json::json!({ "target_repo": "o/r" })),
            ),
            record(
                "probe",
                &"y".repeat(STEP_VIEW_RESPONSE_MAX + 1),
                Some(serde_json::json!({ "cli_version": long, "n": 3 })),
            ),
        ];
        let views = step_views(&steps, "completed", None, None);
        assert_eq!(views.len(), 2);
        assert_eq!(views[0]["status"], "completed");
        assert_eq!(views[0]["duration_ms"], 41_000);
        assert_eq!(views[1]["engine"], "codex");
        assert_eq!(views[1]["model"], "gpt-5.6-sol");
        let clipped = views[1]["outputs"]["cli_version"].as_str().unwrap();
        assert_eq!(clipped.chars().count(), STEP_VIEW_OUTPUT_MAX + 1);
        assert!(clipped.ends_with('…'));
        assert_eq!(views[1]["outputs"]["n"], 3);
        assert_eq!(
            views[1]["raw_response"].as_str().unwrap().chars().count(),
            STEP_VIEW_RESPONSE_MAX + 1
        );
        assert!(views[1]["error"].is_null());
    }

    #[test]
    fn a_step_the_decision_model_settled_first_says_so_and_no_other_step_changes() {
        use workforce_orchestrator::workflow::instance::{
            DecidedFirst, OutcomeBy, DECISION_ENGINE,
        };
        let settled = |first: Option<DecidedFirst>| StepRecord {
            outcome: Some("ignore".into()),
            rounds: 0,
            cost_usd: 0.0,
            provenance: StepProvenance {
                engine: Some(DECISION_ENGINE.into()),
                model: Some("kev".into()),
                outcome_by: Some(OutcomeBy::DecisionModel),
                decided_first: first,
                ..StepProvenance::default()
            },
            ..record("classify", "settled", None)
        };
        let first = DecidedFirst {
            settled: true,
            answer: "kev ignore p=0.93".into(),
        };
        let views = step_views(
            &[settled(Some(first)), settled(None)],
            "completed",
            None,
            None,
        );
        assert_eq!(
            views[0]["decided_first"],
            serde_json::json!({"settled": true, "answer": "kev ignore p=0.93"})
        );
        assert_eq!(
            (views[0]["rounds"].clone(), views[0]["cost_usd"].clone()),
            (0.into(), 0.0.into())
        );
        let plain = views[1].as_object().unwrap();
        assert!(
            !plain.contains_key("decided_first") && !plain.contains_key("outcome_by"),
            "any other step's view is as it was: {plain:?}"
        );
    }

    #[test]
    fn the_step_a_failed_instance_stopped_on_is_failed_with_its_reason() {
        let steps = vec![record("setup", "ok", None), record("gate", "", None)];
        let views = step_views(&steps, "failed", Some("gate"), Some("template unresolved"));
        assert_eq!(views[0]["status"], "completed");
        assert_eq!(views[1]["status"], "failed");
        assert_eq!(views[1]["error"], "template unresolved");
        let views = step_views(&steps, "failed", Some("approve"), Some("later"));
        assert!(
            views.iter().all(|v| v["status"] == "completed"),
            "a failure on an unrecorded step leaves the recorded ones completed"
        );
    }

    #[test]
    fn a_failed_attempt_is_failed_wherever_it_sits() {
        let failed = |step_id: &str, response: &str| StepRecord {
            failed: true,
            correction: None,
            ..record(step_id, response, None)
        };
        let resumed = vec![
            record("setup", "ok", None),
            failed("implement", "claude-code failed (exit 1)"),
            record("implement", "done", None),
        ];
        let views = step_views(&resumed, "completed", None, None);
        let statuses: Vec<&str> = views
            .iter()
            .map(|v| v["status"].as_str().unwrap())
            .collect();
        assert_eq!(statuses, ["completed", "failed", "completed"]);
        assert!(views[1]["error"].is_null());

        let stopped = vec![record("setup", "ok", None), failed("implement", "exit 1")];
        let views = step_views(&stopped, "failed", Some("implement"), Some("exit 1"));
        assert_eq!(views[1]["status"], "failed");
        assert_eq!(views[1]["error"], "exit 1");

        let again = vec![failed("implement", "exit 1"), failed("implement", "exit 2")];
        let views = step_views(&again, "failed", Some("implement"), Some("exit 2"));
        assert_eq!(views[0]["status"], "failed");
        assert!(
            views[0]["error"].is_null(),
            "an earlier attempt does not carry the instance's failure"
        );
        assert_eq!(views[1]["error"], "exit 2");
    }

    #[test]
    fn a_sent_back_attempt_is_a_failed_one_that_says_why() {
        use workforce_orchestrator::workflow::instance::{Correction, CorrectionCause};
        let sent_back = StepRecord {
            failed: true,
            correction: Some(Correction {
                cause: CorrectionCause::Rejected,
                detail: "A verifier did not accept it: count is 5".into(),
            }),
            ..record("count", "five", Some(serde_json::json!({ "count": 5 })))
        };
        let views = step_views(
            &[sent_back, record("count", "three", None)],
            "completed",
            None,
            None,
        );
        assert_eq!(views[0]["status"], "failed");
        assert_eq!(views[0]["correction"]["cause"], "rejected");
        assert_eq!(
            views[0]["error"],
            "A verifier did not accept it: count is 5"
        );
        assert_eq!(views[1]["status"], "completed");
        assert!(views[1]["correction"].is_null() && views[1]["error"].is_null());
    }

    #[test]
    fn a_caller_who_is_no_administrator_reads_sub_agent_tasks_as_themselves() {
        let caller = |identity: Option<&str>, is_admin| {
            Some(Extension(CallerContext {
                identity_id: identity.map(str::to_string),
                is_admin,
                ..CallerContext::default()
            }))
        };
        assert_eq!(caller_reader(caller(Some("root"), true)), JobReader::Admin);
        assert_eq!(caller_reader(caller(None, true)), JobReader::Admin);
        assert_eq!(
            caller_reader(caller(Some("ada"), false)),
            JobReader::Person(TurnPrincipal::run_as(Some("ada".into())))
        );
        assert_eq!(
            caller_reader(None),
            JobReader::Person(TurnPrincipal::default()),
            "a request the gate did not name reads as nobody"
        );
    }
}

#[cfg(test)]
mod steering_tests {
    use super::*;
    use crate::run_steering::SteerRefused;
    use crate::workflow_runtime::testing::{hub, Hub, WORKFLOW};
    use workforce_core::access::Visibility;
    use workforce_orchestrator::workflow::instance::{Actor, InstanceStatus, InterventionKind};

    fn test_config() -> workforce_common::settings::WorkforceConfig {
        let mut config = workforce_common::settings::WorkforceConfig::default();
        config.hub.data_dir = std::env::temp_dir().to_string_lossy().to_string();
        config
    }

    fn state(hub: &Hub) -> State<Arc<HubState>> {
        State(Arc::new(HubState {
            workflow_runtime: Some(hub.host()),
            ..HubState::for_tests(test_config())
        }))
    }

    fn caller(identity_id: Option<&str>, is_admin: bool) -> Option<Extension<CallerContext>> {
        Some(Extension(CallerContext {
            identity_id: identity_id.map(str::to_string),
            is_admin,
            ..Default::default()
        }))
    }

    async fn abort(hub: &Hub, id: &str, by: Option<Extension<CallerContext>>) -> StatusCode {
        abort_workflow_instance(state(hub), Path(id.to_string()), by)
            .await
            .0
    }

    #[tokio::test]
    async fn an_admin_may_steer_anyones_run_over_rest_and_is_named_for_it() {
        let hub = hub().await;
        let kept = crate::decision_records::testing::record_in_memory();
        let key = hub.define(Visibility::Private, false).await;
        let run = hub.run_of(&key, Some(&hub.alice)).await;
        assert_eq!(
            abort(&hub, &run, caller(Some(&hub.carol), true)).await,
            StatusCode::OK
        );
        assert_eq!(hub.status(&run), InstanceStatus::Cancelled);
        let kept = kept.interventions_of(&run, 1).await;
        assert_eq!(
            kept.iter().map(|i| (i.kind, &i.actor)).collect::<Vec<_>>(),
            [(
                InterventionKind::Abort,
                &Actor::Person {
                    id: hub.carol.clone()
                }
            )]
        );

        let other = hub.run_of(&key, Some(&hub.alice)).await;
        assert_eq!(
            abort(&hub, &other, caller(None, true)).await,
            StatusCode::OK,
            "the static admin token"
        );
    }

    #[tokio::test]
    async fn the_starter_may_steer_their_run_over_rest() {
        let hub = hub().await;
        let key = hub.define(Visibility::Private, false).await;
        let run = hub.run_of(&key, Some(&hub.alice)).await;
        let (status, _) = pause_instance(
            state(&hub),
            Path(run.clone()),
            caller(Some(&hub.alice), false),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "allowed, but a paused run cannot pause"
        );
        assert_eq!(
            abort(&hub, &run, caller(Some(&hub.alice), false)).await,
            StatusCode::OK
        );
    }

    /// What `who` sees of `run` on every surface that shows runs: the REST
    /// list and reads, and the `workflow` tool's `instances` and `status`;
    /// and whether they may steer it. All five must agree.
    async fn seen_by(hub: &Hub, who: &str, run: &str) -> ([bool; 4], bool) {
        use workforce_agent::tools::Tool;
        let by = caller(Some(who), false);
        let Json(listed) = list_workflow_instances(
            state(hub),
            by.clone(),
            Query(InstancesQuery {
                status: None,
                workflow: None,
            }),
        )
        .await;
        let listed = listed["instances"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["instance_id"] == run);
        let read = get_workflow_instance(state(hub), by, Path(run.to_string()))
            .await
            .0
            == StatusCode::OK;
        let tool = crate::workflow_tool::WorkflowTool::new(hub.host(), "agent".into(), None)
            .with_write_auth(
                crate::def_write_auth::TurnDefWriteAuth::default()
                    .with_admin_policy(crate::authz::never_admin()),
            );
        let ask = |input: serde_json::Value| {
            workforce_agent::runner::CURRENT_USER_ID.scope(who.to_string(), tool.execute(input))
        };
        let instances = ask(serde_json::json!({ "action": "instances" }))
            .await
            .unwrap()
            .output
            .to_string()
            .contains(run);
        let status = ask(serde_json::json!({ "action": "status", "instance_id": run }))
            .await
            .unwrap()
            .success;
        let steerer = crate::run_steering::Steerer {
            writer: crate::def_write_auth::DefWriter {
                identity_id: Some(who.to_string()),
                is_admin: false,
            },
            actor: Actor::Person {
                id: who.to_string(),
            },
        };
        let steers = hub
            .runtime
            .authorize_steer(run, &steerer, crate::run_steering::SteerAction::Pause)
            .await
            .is_ok();
        ([listed, read, instances, status], steers)
    }

    /// A run of a workflow that reaches a member and lets others steer its
    /// runs is theirs to see as it is theirs to steer, whoever started it; a
    /// private workflow, one closed to others' steering, or one gone keeps
    /// its runs to their runner and administrators.
    #[tokio::test]
    async fn whoever_may_steer_a_run_sees_it_and_nobody_else_does() {
        let open = [true; 4];
        let shut = [false; 4];
        for (visibility, by_others, seen) in [
            (Visibility::Global, true, true),
            (Visibility::Global, false, false),
            (Visibility::Private, true, false),
        ] {
            let hub = hub().await;
            let key = hub.define(visibility, by_others).await;
            let carols = hub.run_of(&key, Some(&hub.carol)).await;
            let expected = (if seen { open } else { shut }, seen);
            assert_eq!(
                seen_by(&hub, &hub.bob, &carols).await,
                expected,
                "{visibility:?}, steerable by others: {by_others}"
            );
            assert_eq!(seen_by(&hub, &hub.carol, &carols).await.0, open);
        }

        let hub = hub().await;
        let key = hub.define(Visibility::Global, true).await;
        let carols = hub.run_of(&key, Some(&hub.carol)).await;
        hub.forget(&key).await;
        assert_eq!(
            seen_by(&hub, &hub.bob, &carols).await,
            (shut, false),
            "its workflow is gone"
        );
    }

    /// The admin gate admits the `admin` scope, which a member may hold: of a
    /// workflow closed to others' steering, such a caller reads only runs done
    /// for them or for nobody, and any other run is not there for them.
    #[tokio::test]
    async fn rest_shows_a_run_only_to_whom_it_is_done_for_and_admins() {
        let hub = hub().await;
        let key = hub.define(Visibility::Global, false).await;
        let alices = hub.run_of(&key, Some(&hub.alice)).await;
        let nobodys = hub.run_of(&key, None).await;
        let listed = |by: Option<Extension<CallerContext>>| {
            let state = state(&hub);
            async move {
                let Json(body) = list_workflow_instances(
                    state,
                    by,
                    Query(InstancesQuery {
                        status: None,
                        workflow: None,
                    }),
                )
                .await;
                let mut ids: Vec<String> = body["instances"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|row| row["instance_id"].as_str().unwrap().to_string())
                    .collect();
                ids.sort();
                ids
            }
        };
        let both = {
            let mut both = vec![alices.clone(), nobodys.clone()];
            both.sort();
            both
        };
        assert_eq!(listed(caller(Some(&hub.alice), false)).await, both);
        assert_eq!(listed(caller(Some(&hub.carol), true)).await, both);
        assert_eq!(
            listed(caller(None, true)).await,
            both,
            "the static admin token"
        );
        assert_eq!(
            listed(caller(Some(&hub.bob), false)).await,
            std::slice::from_ref(&nobodys)
        );
        assert_eq!(
            listed(None).await,
            std::slice::from_ref(&nobodys),
            "a request the gate did not name"
        );

        let reads = |run: String, by: Option<Extension<CallerContext>>| {
            let state = state(&hub);
            async move {
                [
                    get_workflow_instance(state.clone(), by.clone(), Path(run.clone()))
                        .await
                        .0,
                    get_instance_steps(state.clone(), by.clone(), Path(run.clone()))
                        .await
                        .0,
                    get_instance_cost(state.clone(), by.clone(), Path(run.clone()))
                        .await
                        .0,
                    get_instance_variables(state, by, Path(run)).await.0,
                ]
            }
        };
        assert_eq!(
            reads(alices.clone(), caller(Some(&hub.bob), false)).await,
            [StatusCode::NOT_FOUND; 4]
        );
        assert_eq!(
            reads(alices.clone(), None).await,
            [StatusCode::NOT_FOUND; 4]
        );
        assert_eq!(
            reads(alices.clone(), caller(Some(&hub.alice), false)).await,
            [StatusCode::OK; 4]
        );
        assert_eq!(
            reads(alices, caller(Some(&hub.carol), true)).await,
            [StatusCode::OK; 4]
        );
        assert_eq!(
            reads(nobodys, caller(Some(&hub.bob), false)).await,
            [StatusCode::OK; 4]
        );
    }

    /// A member holding the `admin` scope lists, reads and runs only the
    /// workflows the gateway would show them; anyone else's private one is not
    /// there for them.
    #[tokio::test]
    async fn rest_shows_a_workflow_only_to_whom_the_gateway_would() {
        let hub = hub().await;
        let key = hub.define(Visibility::Private, false).await;
        let listed = |by: Option<Extension<CallerContext>>| {
            let state = state(&hub);
            async move {
                let Json(body) = list_workflows(state, by).await;
                body["workflows"].as_array().unwrap().len()
            }
        };
        let read = |by: Option<Extension<CallerContext>>| {
            get_workflow_definition(state(&hub), by, Path(key.clone()))
        };
        let run = |id: &str, by: Option<Extension<CallerContext>>| {
            run_workflow(
                state(&hub),
                by,
                Path(id.to_string()),
                Json(RunWorkflowPayload {
                    payload: serde_json::json!({}),
                    channel_id: None,
                    thread_id: None,
                }),
            )
        };

        for (by, sees) in [
            (caller(Some(&hub.alice), false), true),
            (caller(Some(&hub.carol), true), true),
            (caller(None, true), true),
            (caller(Some(&hub.bob), false), false),
            (None, false),
        ] {
            assert_eq!(listed(by.clone()).await, usize::from(sees));
            assert_eq!(
                read(by).await.0,
                if sees {
                    StatusCode::OK
                } else {
                    StatusCode::NOT_FOUND
                }
            );
        }
        let (status, Json(refused)) = run(&key, caller(Some(&hub.bob), false)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(
            refused["error"],
            format!("Unknown workflow: '{key}'"),
            "answered as a workflow that is not there"
        );
        let (status, _) = run("missing", caller(Some(&hub.bob), false)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        hub.define(Visibility::Global, false).await;
        assert_eq!(listed(caller(Some(&hub.bob), false)).await, 1);
        assert_eq!(read(caller(Some(&hub.bob), false)).await.0, StatusCode::OK);
    }

    #[tokio::test]
    async fn rest_lists_a_playbook_only_to_whom_the_gateway_would() {
        use crate::playbook_runtime::testing::{owned, runtime_with};
        let runtime = Arc::new(
            runtime_with(vec![
                owned("shared", None, Visibility::Global),
                owned("alices", Some("alice"), Visibility::Private),
            ])
            .await,
        );
        let listed = |by: Option<Extension<CallerContext>>| {
            let state = State(Arc::new(HubState {
                playbook_runtime: Some(runtime.clone()),
                ..HubState::for_tests(test_config())
            }));
            async move {
                let Json(body) = list_playbooks(state, by).await;
                let mut ids: Vec<String> = body["playbooks"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|p| p["id"].as_str().unwrap().to_string())
                    .collect();
                ids.sort();
                ids
            }
        };
        assert_eq!(
            listed(caller(Some("alice"), false)).await,
            ["alices", "shared"]
        );
        assert_eq!(listed(caller(None, true)).await, ["alices", "shared"]);
        assert_eq!(listed(caller(Some("bob"), false)).await, ["shared"]);
        assert_eq!(listed(None).await, ["shared"]);
    }

    /// Someone a run was not done for steers it where it already reports;
    /// its starter may still send it somewhere new.
    #[tokio::test]
    async fn only_the_starter_may_move_where_a_steered_run_reports() {
        let hub = hub().await;
        let key = hub.define(Visibility::Global, true).await;
        let resume_to = |run: String, who: &str, channel: &str| {
            resume_workflow_instance(
                state(&hub),
                Path(run),
                caller(Some(who), false),
                Json(ResumePayload {
                    outcome: None,
                    gate: None,
                    channel_id: Some(channel.into()),
                    thread_id: Some("9.9".into()),
                }),
            )
        };
        let reports_to = |run: &str| {
            let row = hub.runtime.get_instance_status(run).unwrap().unwrap();
            let vars: serde_json::Value = serde_json::from_str(&row.variables).unwrap();
            (vars["_channel_id"].clone(), vars.get("_thread_id").cloned())
        };

        let run = hub.run_of(&key, Some(&hub.alice)).await;
        hub.runtime
            .set_instance_variable(&run, "_channel_id", "C-ALICE")
            .unwrap();
        let (status, _) = resume_to(run.clone(), &hub.bob, "D-BOB").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            reports_to(&run),
            (serde_json::json!("C-ALICE"), None),
            "Bob's resume leaves the run reporting where Alice had it"
        );

        let own = hub.run_of(&key, Some(&hub.alice)).await;
        let (status, _) = resume_to(own.clone(), &hub.alice, "C-NEW").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            reports_to(&own),
            (serde_json::json!("C-NEW"), Some(serde_json::json!("9.9")))
        );
    }

    /// A run Alice started, waiting on a decision Bob makes.
    async fn waiting_on_bob(hub: &Hub) -> String {
        hub.with_slack();
        let key = hub
            .define_steps(
                Visibility::Private,
                false,
                &crate::workflow_runtime::testing::decision_steps(&hub.bob, ""),
            )
            .await;
        hub.run_to_decision(&key, Some(&hub.alice), Some("CRUN000001"))
            .await
    }

    async fn press(
        hub: &Hub,
        run: &str,
        who: Option<Extension<CallerContext>>,
        outcome: &str,
        gate: Option<String>,
    ) -> (StatusCode, serde_json::Value) {
        let (status, Json(body)) = resume_workflow_instance(
            state(hub),
            Path(run.to_string()),
            who,
            Json(ResumePayload {
                outcome: Some(outcome.into()),
                gate,
                channel_id: None,
                thread_id: None,
            }),
        )
        .await;
        (status, body)
    }

    fn gate_of(hub: &Hub, run: &str) -> String {
        workforce_orchestrator::workflow::decision::RunDecision::read(&hub.instance(run).variables)
            .unwrap()
            .gate
            .to_string()
    }

    #[tokio::test]
    async fn resuming_an_open_decision_decides_for_a_named_decider() {
        let hub = hub().await;
        let run = waiting_on_bob(&hub).await;

        let (status, body) = press(
            &hub,
            &run,
            caller(Some(&hub.bob), false),
            "send",
            Some(gate_of(&hub, &run)),
        )
        .await;

        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(
            body["decided"],
            serde_json::json!({ "outcome": "send", "label": "Send", "summary": "Sent by Bob" })
        );
        assert_eq!(hub.ran(&run).await, ["draft", "review", "send"]);
    }

    #[tokio::test]
    async fn an_admin_who_is_not_a_decider_is_refused() {
        let hub = hub().await;
        let run = waiting_on_bob(&hub).await;

        let (status, body) = press(&hub, &run, caller(Some(&hub.carol), true), "send", None).await;

        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body["error"], "Only Bob can decide this.");
    }

    #[tokio::test]
    async fn a_stale_gate_is_a_conflict() {
        let hub = hub().await;
        let run = waiting_on_bob(&hub).await;

        for gate in [uuid::Uuid::new_v4().to_string(), "not-a-gate".to_string()] {
            let (status, body) = press(
                &hub,
                &run,
                caller(Some(&hub.bob), false),
                "send",
                Some(gate),
            )
            .await;
            assert_eq!(status, StatusCode::CONFLICT);
            assert_eq!(
                body["error"],
                "That decision is closed; the run has moved on."
            );
        }
    }

    #[tokio::test]
    async fn reject_goto_and_skip_at_a_decision_are_conflicts() {
        let hub = hub().await;
        let run = waiting_on_bob(&hub).await;
        let alice = || caller(Some(&hub.alice), false);

        let (rejected, _) = reject_workflow_instance(
            state(&hub),
            Path(run.clone()),
            alice(),
            Json(RejectPayload {
                reason: None,
                feedback: None,
                channel_id: None,
                thread_id: None,
            }),
        )
        .await;
        let (went, _) = goto_instance_step(
            state(&hub),
            Path(run.clone()),
            alice(),
            Json(GotoPayload {
                step_id: "send".into(),
            }),
        )
        .await;
        let (skipped, _) = skip_instance_step(state(&hub), Path(run.clone()), alice()).await;

        assert_eq!(
            [rejected, went, skipped],
            [
                StatusCode::CONFLICT,
                StatusCode::CONFLICT,
                StatusCode::CONFLICT
            ]
        );
        assert_eq!(hub.status(&run), InstanceStatus::Paused);
    }

    #[tokio::test]
    async fn an_instance_shows_its_open_decision_and_whether_the_caller_may_decide() {
        let hub = hub().await;
        let run = waiting_on_bob(&hub).await;
        let shown = |who| get_workflow_instance(state(&hub), who, Path(run.clone()));

        let (status, Json(as_admin)) = shown(caller(Some(&hub.carol), true)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(as_admin["decision"]["title"], "Review the digest");
        assert_eq!(
            as_admin["decision"]["waiting_on"],
            serde_json::json!(["Bob"])
        );
        assert_eq!(as_admin["decision"]["gate"], gate_of(&hub, &run));
        assert_eq!(as_admin["decision"]["may_decide"], false);
        let (_, Json(as_bob)) = shown(caller(Some(&hub.bob), false)).await;
        assert_eq!(as_bob["decision"]["may_decide"], true);
    }

    #[tokio::test]
    async fn rest_says_whether_a_resumes_outcome_was_applied() {
        let hub = hub().await;
        let key = hub.define_gate(Visibility::Private, false).await;
        let waiting = hub.run_to_wait(&key, Some(&hub.alice)).await;
        let (status, Json(body)) = reject_workflow_instance(
            state(&hub),
            Path(waiting.clone()),
            caller(Some(&hub.alice), false),
            Json(RejectPayload {
                reason: None,
                feedback: None,
                channel_id: None,
                thread_id: None,
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            (&body["outcome_applied"], &body["message"]),
            (
                &serde_json::json!(true),
                &serde_json::json!("Workflow resumed with outcome 'rejected'.")
            )
        );
        assert_eq!(hub.ran(&waiting).await, ["gate", "fix"]);

        let resume = |id: &str, outcome: Option<&str>| {
            resume_workflow_instance(
                state(&hub),
                Path(id.to_string()),
                caller(Some(&hub.alice), false),
                Json(ResumePayload {
                    outcome: outcome.map(str::to_string),
                    gate: None,
                    channel_id: None,
                    thread_id: None,
                }),
            )
        };
        let held = hub.run_of(&key, Some(&hub.alice)).await;
        let (status, Json(body)) = resume(&held, Some("approved")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            (&body["outcome_applied"], &body["message"]),
            (
                &serde_json::json!(false),
                &serde_json::json!(
                    "Workflow resumed, but its step did not go on with outcome 'approved'."
                )
            )
        );
        hub.settled(&held).await;
        let (_, Json(body)) = resume(&held, None).await;
        assert_eq!(
            (&body["outcome_applied"], &body["message"]),
            (
                &serde_json::Value::Null,
                &serde_json::json!("Workflow resumed.")
            )
        );
    }

    /// The admin gate keeps members off these routes; the routes still apply
    /// the rule themselves, so a caller the gate let through as a member is
    /// held to it.
    #[tokio::test]
    async fn rest_holds_anyone_but_the_starter_and_admins_to_the_workflows_terms() {
        let hub = hub().await;
        let shared = hub.define(Visibility::Global, true).await;
        let open = hub.run_of(&shared, Some(&hub.alice)).await;
        let under_way = hub.run_of(&shared, Some(&hub.alice)).await;
        assert_eq!(
            abort(&hub, &open, caller(Some(&hub.bob), false)).await,
            StatusCode::OK
        );

        hub.define(Visibility::Global, false).await;
        assert_eq!(
            abort(&hub, &under_way, caller(Some(&hub.bob), false)).await,
            StatusCode::FORBIDDEN,
            "switched off mid-run"
        );
        let closed = hub.run_of(&shared, Some(&hub.alice)).await;
        let (status, Json(body)) = reject_workflow_instance(
            state(&hub),
            Path(closed.clone()),
            caller(Some(&hub.bob), false),
            Json(RejectPayload {
                reason: None,
                feedback: Some("ship it anyway".into()),
                channel_id: None,
                thread_id: None,
            }),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(
            body["error"],
            SteerRefused::Closed {
                workflow: WORKFLOW.into()
            }
            .to_string()
        );
        let untouched = hub.runtime.get_instance_status(&closed).unwrap().unwrap();
        assert!(
            !untouched.variables.contains("ship it anyway"),
            "a refused reject leaves no feedback behind"
        );

        hub.define(Visibility::Private, true).await;
        let private = hub.run_of(&shared, Some(&hub.alice)).await;
        assert_eq!(
            abort(&hub, &private, caller(Some(&hub.bob), false)).await,
            StatusCode::FORBIDDEN
        );
        assert_eq!(hub.status(&private), InstanceStatus::Paused);
    }
}
