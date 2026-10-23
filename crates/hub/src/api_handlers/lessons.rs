//! Admin Lessons-learned API (admin-scope on every route, reads included —
//! the corpus contains customer/PII content).
//!
//! - `GET    /api/lessons`              — browse the lesson corpus.
//! - `GET    /api/lessons/metrics`      — summary cards + time-series + prevention.
//! - `GET    /api/lessons/recurrences`  — cross-day trigger-tag clusters.
//! - `DELETE /api/lessons/{id}`         — forget one lesson.
//! - `POST   /api/lessons/cleanup`      — kick a background cleanup job.
//! - `POST   /api/lessons/backfill`     — kick a background backfill job.
//! - `GET    /api/lessons/jobs/{id}`    — poll a kicked job.
//!
//! The memory store and an LLM are borrowed from the live agent services
//! (`state.default_service()`) so they carry the correct Weaviate credentials
//! and provider config — no per-request reconstruction.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::Json;
use chrono::{DateTime, Duration, Utc};
use once_cell::sync::Lazy;
use serde::Deserialize;
use serde_json::json;

use workforce_common::session::TurnPrincipal;
use workforce_core::types::AgentId;
use workforce_memory::store::MemoryStore;
use workforce_memory::types::{MemoryFilter, MemoryId, MemorySource};

use crate::gateway::GatewayState;

use super::errors::ApiError;

const MAX_LESSONS: usize = 5_000;

// ── Query / body types ──────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct LessonsQuery {
    pub token: Option<String>,
    pub agent: Option<String>,
    pub kind: Option<String>,
    /// "0" → never applied, "1" → applied at least once.
    pub applied: Option<String>,
    pub q: Option<String>,
    pub limit: Option<usize>,
    pub offset: Option<usize>,
    pub days: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct JobBody {
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub dry_run: Option<bool>,
    #[serde(default)]
    pub merge_similarity: Option<f32>,
    #[serde(default)]
    pub since_days: Option<i64>,
}

// ── Tag helpers ──────────────────────────────────────────────────────────────

fn tag_f32(tags: &[String], key: &str) -> Option<f32> {
    let prefix = format!("{key}:");
    tags.iter()
        .find_map(|t| t.strip_prefix(&prefix))
        .and_then(|s| s.parse().ok())
}

fn tag_u32(tags: &[String], key: &str) -> Option<u32> {
    let prefix = format!("{key}:");
    tags.iter()
        .find_map(|t| t.strip_prefix(&prefix))
        .and_then(|s| s.parse().ok())
}

fn trigger_handles(tags: &[String]) -> Vec<String> {
    tags.iter()
        .filter_map(|t| t.strip_prefix("trigger:").map(str::to_string))
        .collect()
}

fn learning_kind(tags: &[String]) -> Option<String> {
    tags.iter()
        .find_map(|t| t.strip_prefix("learning:").map(str::to_string))
}

fn lesson_date(tags: &[String], created_at: DateTime<Utc>) -> String {
    tags.iter()
        .find_map(|t| t.strip_prefix("date:").map(str::to_string))
        .unwrap_or_else(|| created_at.format("%Y-%m-%d").to_string())
}

/// Borrow the shared memory store from any live agent service.
fn memory_store(state: &GatewayState) -> Result<Arc<dyn MemoryStore>, ApiError> {
    state
        .default_service()
        .and_then(|svc| svc.memory_store.clone())
        .ok_or_else(ApiError::service_unavailable)
}

/// Build the lesson list filter, optionally scoped to one owner.
fn lesson_filter(agent: Option<&str>) -> MemoryFilter {
    MemoryFilter {
        source: Some(MemorySource::Lesson),
        owner: agent
            .filter(|a| !a.is_empty() && *a != "all")
            .map(AgentId::new),
        system_unscoped: true,
        limit: Some(MAX_LESSONS),
        ..Default::default()
    }
}

/// A flattened lesson row used by both list and metrics.
struct LessonRow {
    id: String,
    owner: String,
    content: String,
    importance: f32,
    confidence: f32,
    applied: u32,
    kind: Option<String>,
    date: String,
    triggers: Vec<String>,
}

async fn lesson_rows(
    store: &Arc<dyn MemoryStore>,
    agent: Option<&str>,
) -> Result<Vec<LessonRow>, ApiError> {
    let mems = store
        .list(lesson_filter(agent))
        .await
        .map_err(|e| ApiError::internal(format!("listing lessons: {e}")))?;
    Ok(mems
        .into_iter()
        .map(|m| LessonRow {
            confidence: tag_f32(&m.tags, "confidence").unwrap_or(m.importance),
            applied: tag_u32(&m.tags, "applied").unwrap_or(0),
            kind: learning_kind(&m.tags),
            date: lesson_date(&m.tags, m.created_at),
            triggers: trigger_handles(&m.tags),
            id: m.id.0,
            owner: m.owner.as_str().to_string(),
            content: m.content,
            importance: m.importance,
        })
        .collect())
}

fn row_json(r: &LessonRow) -> serde_json::Value {
    json!({
        "id": r.id,
        "owner": r.owner,
        "content": r.content,
        "importance": r.importance,
        "confidence": r.confidence,
        "applied": r.applied,
        "learning_kind": r.kind,
        "date": r.date,
        "trigger_tags": r.triggers,
    })
}

// ── GET /api/lessons ─────────────────────────────────────────────────────────

pub async fn list_lessons(
    Query(query): Query<LessonsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<serde_json::Value>, ApiError> {
    crate::gateway::require_scope(
        &state,
        &headers,
        query.token.as_deref(),
        crate::jwt::SCOPE_ADMIN,
    )
    .await?;
    let store = memory_store(&state)?;
    let mut rows = lesson_rows(&store, query.agent.as_deref()).await?;

    if let Some(kind) = query.kind.as_deref().filter(|k| !k.is_empty()) {
        rows.retain(|r| r.kind.as_deref() == Some(kind));
    }
    match query.applied.as_deref() {
        Some("0") => rows.retain(|r| r.applied == 0),
        Some("1") => rows.retain(|r| r.applied >= 1),
        _ => {}
    }
    if let Some(q) = query.q.as_deref().filter(|q| !q.is_empty()) {
        let needle = q.to_lowercase();
        rows.retain(|r| {
            r.content.to_lowercase().contains(&needle)
                || r.triggers
                    .iter()
                    .any(|t| t.to_lowercase().contains(&needle))
        });
    }
    rows.sort_by(|a, b| b.applied.cmp(&a.applied).then(b.date.cmp(&a.date)));

    let total = rows.len();
    let offset = query.offset.unwrap_or(0);
    let limit = query.limit.unwrap_or(100);
    let page: Vec<serde_json::Value> = rows.iter().skip(offset).take(limit).map(row_json).collect();

    Ok(Json(json!({ "lessons": page, "total": total })))
}

// ── GET /api/lessons/metrics ─────────────────────────────────────────────────

pub async fn lessons_metrics(
    Query(query): Query<LessonsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<serde_json::Value>, ApiError> {
    crate::gateway::require_scope(
        &state,
        &headers,
        query.token.as_deref(),
        crate::jwt::SCOPE_ADMIN,
    )
    .await?;
    let store = memory_store(&state)?;
    let agent = query.agent.as_deref();
    let rows = lesson_rows(&store, agent).await?;
    let days = query.days.unwrap_or(30).clamp(1, 365);
    let to = Utc::now();
    let from = to - Duration::days(days);

    // ── Summary cards (live corpus) ──
    let total = rows.len();
    let applied_at_least_once = rows.iter().filter(|r| r.applied >= 1).count();
    let never_applied = total - applied_at_least_once;
    let avg_confidence = if total > 0 {
        rows.iter().map(|r| r.confidence as f64).sum::<f64>() / total as f64
    } else {
        0.0
    };

    // ── By learning kind ──
    let mut by_kind: HashMap<String, usize> = HashMap::new();
    for r in &rows {
        *by_kind
            .entry(r.kind.clone().unwrap_or_else(|| "unknown".into()))
            .or_default() += 1;
    }

    // ── Corpus growth (cumulative, from lesson date tags) ──
    let mut per_day: HashMap<String, usize> = HashMap::new();
    for r in &rows {
        *per_day.entry(r.date.clone()).or_default() += 1;
    }
    let mut days_sorted: Vec<(String, usize)> = per_day.into_iter().collect();
    days_sorted.sort_by(|a, b| a.0.cmp(&b.0));
    let mut cumulative = 0usize;
    let corpus_growth: Vec<serde_json::Value> = days_sorted
        .iter()
        .map(|(day, n)| {
            cumulative += n;
            json!({ "day": day, "cumulative": cumulative })
        })
        .collect();

    // ── Cross-day recurrence clusters ──
    let clusters = recurrence_clusters(&rows);
    let open_recurrences = clusters.len();

    // ── Leaderboard ──
    let mut leaders = rows.iter().collect::<Vec<_>>();
    leaders.sort_by(|a, b| b.applied.cmp(&a.applied));
    let leaderboard: Vec<serde_json::Value> = leaders
        .iter()
        .take(10)
        .map(|r| json!({ "id": r.id, "content": r.content, "applied": r.applied, "confidence": r.confidence }))
        .collect();

    // ── Event-derived series (telemetry) ──
    let (timeseries, prevention, recall, extraction_cost) = event_metrics(&state, agent, from, to)
        .unwrap_or((Vec::new(), json!(null), json!(null), 0.0));

    Ok(Json(json!({
        "summary": {
            "total": total,
            "applied_at_least_once": applied_at_least_once,
            "never_applied": never_applied,
            "avg_confidence": avg_confidence,
            "open_recurrences": open_recurrences,
            "extraction_cost_usd": extraction_cost,
            "window_days": days,
        },
        "by_kind": by_kind,
        "corpus_growth": corpus_growth,
        "timeseries": timeseries,
        "prevention": prevention,
        "recall": recall,
        "leaderboard": leaderboard,
    })))
}

/// Compute the daily event series, prevention rate, and extraction cost from
/// the telemetry store. Returns `None` when there is no telemetry store.
#[allow(clippy::type_complexity)]
fn event_metrics(
    state: &GatewayState,
    agent: Option<&str>,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> Option<(
    Vec<serde_json::Value>,
    serde_json::Value,
    serde_json::Value,
    f64,
)> {
    let store = state.telemetry_store.as_ref()?;
    let agent_filter = agent.filter(|a| !a.is_empty() && *a != "all");
    let events = store
        .lesson_events_in_range(agent_filter, from, to)
        .unwrap_or_default();

    // Daily counts per event type.
    let mut by_day: HashMap<String, HashMap<String, usize>> = HashMap::new();
    for ev in &events {
        let day = ev.timestamp.format("%Y-%m-%d").to_string();
        *by_day
            .entry(day)
            .or_default()
            .entry(ev.event_type.clone())
            .or_default() += 1;
    }
    let mut day_keys: Vec<String> = by_day.keys().cloned().collect();
    day_keys.sort();
    let timeseries: Vec<serde_json::Value> = day_keys
        .iter()
        .map(|day| {
            let m = &by_day[day];
            json!({
                "day": day,
                "extracted": m.get("extracted").copied().unwrap_or(0),
                "injected": m.get("injected").copied().unwrap_or(0),
                "suppressed": m.get("suppressed").copied().unwrap_or(0),
                "reinforced_positive": m.get("reinforced_positive").copied().unwrap_or(0),
                "reinforced_negative": m.get("reinforced_negative").copied().unwrap_or(0),
                "merged_recurrence": m.get("merged_recurrence").copied().unwrap_or(0),
                "hit": m.get("hit").copied().unwrap_or(0),
                "recall_miss": m.get("recall_miss").copied().unwrap_or(0),
                "suppressed_repeat": m.get("suppressed_repeat").copied().unwrap_or(0),
                "superseded": m.get("superseded").copied().unwrap_or(0),
                "pruned": m.get("pruned").copied().unwrap_or(0),
            })
        })
        .collect();

    // Prevention: an `injected` event "failed" if a later same-trigger
    // extraction/recurrence happened for the same agent; else "prevented".
    let recurrences: Vec<_> = events
        .iter()
        .filter(|e| e.event_type == "extracted" || e.event_type == "merged_recurrence")
        .collect();
    let mut prevented = 0usize;
    let mut failed = 0usize;
    for inj in events.iter().filter(|e| e.event_type == "injected") {
        let inj_triggers: HashSet<&String> = inj.trigger_tags.iter().collect();
        if inj_triggers.is_empty() {
            continue;
        }
        let recurred = recurrences.iter().any(|rec| {
            rec.timestamp > inj.timestamp
                && rec.agent_id == inj.agent_id
                && rec.trigger_tags.iter().any(|t| inj_triggers.contains(t))
        });
        if recurred {
            failed += 1;
        } else {
            prevented += 1;
        }
    }
    let prevention = if prevented + failed > 0 {
        json!({
            "prevented": prevented,
            "failed": failed,
            "rate": prevented as f64 / (prevented + failed) as f64,
        })
    } else {
        json!({ "prevented": 0, "failed": 0, "rate": null })
    };

    // Recall hit-rate: of lessons re-taught (a recurrence), how often was the
    // lesson actually injected this turn (`hit`) vs missed by retrieval
    // (`recall_miss`). Separates earned continuity from lucky recall.
    let hits = events.iter().filter(|e| e.event_type == "hit").count();
    let recall_misses = events
        .iter()
        .filter(|e| e.event_type == "recall_miss")
        .count();
    let recall = if hits + recall_misses > 0 {
        json!({
            "hits": hits,
            "misses": recall_misses,
            "rate": hits as f64 / (hits + recall_misses) as f64,
        })
    } else {
        json!({ "hits": 0, "misses": 0, "rate": null })
    };

    // Lesson-extraction cost in the window.
    let extraction_cost = store
        .cost_by_task_type(from, to)
        .unwrap_or_default()
        .into_iter()
        .find(|(k, _)| k == "lesson_extraction")
        .map(|(_, v)| v)
        .unwrap_or(0.0);

    Some((timeseries, prevention, recall, extraction_cost))
}

// ── GET /api/lessons/recurrences ─────────────────────────────────────────────

/// Trigger tags that appear on lessons from ≥2 distinct days, with the lessons.
fn recurrence_clusters(rows: &[LessonRow]) -> Vec<serde_json::Value> {
    let mut by_trigger: HashMap<String, Vec<&LessonRow>> = HashMap::new();
    for r in rows {
        for t in &r.triggers {
            by_trigger.entry(t.clone()).or_default().push(r);
        }
    }
    let mut clusters: Vec<serde_json::Value> = by_trigger
        .into_iter()
        .filter_map(|(trigger, lessons)| {
            let dates: HashSet<&String> = lessons.iter().map(|l| &l.date).collect();
            if dates.len() < 2 {
                return None;
            }
            let mut dates: Vec<String> = dates.into_iter().cloned().collect();
            dates.sort();
            let items: Vec<serde_json::Value> = lessons
                .iter()
                .map(|l| json!({ "id": l.id, "content": l.content, "date": l.date, "applied": l.applied }))
                .collect();
            Some(json!({ "trigger": trigger, "dates": dates, "lessons": items }))
        })
        .collect();
    clusters.sort_by(|a, b| {
        b["dates"]
            .as_array()
            .map(|d| d.len())
            .unwrap_or(0)
            .cmp(&a["dates"].as_array().map(|d| d.len()).unwrap_or(0))
    });
    clusters
}

pub async fn lessons_recurrences(
    Query(query): Query<LessonsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<serde_json::Value>, ApiError> {
    crate::gateway::require_scope(
        &state,
        &headers,
        query.token.as_deref(),
        crate::jwt::SCOPE_ADMIN,
    )
    .await?;
    let store = memory_store(&state)?;
    let rows = lesson_rows(&store, query.agent.as_deref()).await?;
    Ok(Json(json!({ "clusters": recurrence_clusters(&rows) })))
}

// ── DELETE /api/lessons/{id} ─────────────────────────────────────────────────

pub async fn delete_lesson(
    Path(id): Path<String>,
    Query(query): Query<LessonsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<serde_json::Value>, ApiError> {
    crate::gateway::require_scope(
        &state,
        &headers,
        query.token.as_deref(),
        crate::jwt::SCOPE_ADMIN,
    )
    .await?;
    let store = memory_store(&state)?;
    store
        .forget(&MemoryId(id.clone()))
        .await
        .map_err(|e| ApiError::internal(format!("forget failed: {e}")))?;
    Ok(Json(json!({ "deleted": true, "id": id })))
}

// ── Background jobs (cleanup / backfill) ─────────────────────────────────────

static JOBS: Lazy<std::sync::Mutex<HashMap<String, serde_json::Value>>> =
    Lazy::new(|| std::sync::Mutex::new(HashMap::new()));

fn set_job(id: &str, value: serde_json::Value) {
    if let Ok(mut jobs) = JOBS.lock() {
        jobs.insert(id.to_string(), value);
    }
}

pub async fn lessons_cleanup(
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
    Json(body): Json<JobBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let caller =
        crate::gateway::require_scope(&state, &headers, None, crate::jwt::SCOPE_ADMIN).await?;
    let principal = TurnPrincipal::run_as(caller.identity_id);
    let svc = state
        .default_service()
        .ok_or_else(ApiError::service_unavailable)?;
    let store = svc
        .memory_store
        .clone()
        .ok_or_else(ApiError::service_unavailable)?;
    let llm = Arc::clone(&svc.llm);

    let job_id = uuid::Uuid::new_v4().to_string();
    let dry_run = body.dry_run.unwrap_or(true);
    let agent_ids: Vec<AgentId> = body
        .agent
        .filter(|a| !a.is_empty() && a != "all")
        .map(|a| vec![AgentId::new(a)])
        .unwrap_or_default();
    let merge_similarity = body
        .merge_similarity
        .unwrap_or(crate::turn_extraction::LESSON_NEAR_DUPLICATE_SIMILARITY);

    set_job(
        &job_id,
        json!({ "status": "running", "kind": "cleanup", "dry_run": dry_run }),
    );
    let job_id_task = job_id.clone();
    tokio::spawn(async move {
        let opts = crate::lessons_cleanup::CleanupOptions {
            agent_ids,
            dry_run,
            model: None,
            merge_similarity,
            print: true,
        };
        let run = crate::lessons_cleanup::run(llm, store, opts);
        match crate::user_config::as_principal(&principal, run).await {
            Ok(report) => set_job(
                &job_id_task,
                json!({
                    "status": "done",
                    "kind": "cleanup",
                    "dry_run": dry_run,
                    "report": {
                        "lessons_scanned": report.lessons_scanned,
                        "pruned": report.pruned,
                        "merged": report.merged,
                        "kept": report.kept,
                        "errors": report.errors,
                    },
                }),
            ),
            Err(e) => set_job(
                &job_id_task,
                json!({ "status": "failed", "kind": "cleanup", "error": e.to_string() }),
            ),
        }
    });

    Ok(Json(json!({ "job_id": job_id, "status": "running" })))
}

pub async fn lessons_backfill(
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
    Json(body): Json<JobBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let caller =
        crate::gateway::require_scope(&state, &headers, None, crate::jwt::SCOPE_ADMIN).await?;
    let principal = TurnPrincipal::run_as(caller.identity_id);
    let svc = state
        .default_service()
        .ok_or_else(ApiError::service_unavailable)?;
    let store = svc
        .memory_store
        .clone()
        .ok_or_else(ApiError::service_unavailable)?;
    let llm = Arc::clone(&svc.llm);
    let data_dir = std::path::PathBuf::from(&svc.config.hub.data_dir);

    let job_id = uuid::Uuid::new_v4().to_string();
    let dry_run = body.dry_run.unwrap_or(true);
    let agent_ids: Vec<AgentId> = body
        .agent
        .filter(|a| !a.is_empty() && a != "all")
        .map(|a| vec![AgentId::new(a)])
        .unwrap_or_default();
    let since = body
        .since_days
        .map(|d| Utc::now() - Duration::days(d.clamp(1, 3650)));

    set_job(
        &job_id,
        json!({ "status": "running", "kind": "backfill", "dry_run": dry_run }),
    );
    let job_id_task = job_id.clone();
    tokio::spawn(async move {
        let opts = crate::lessons_backfill::BackfillOptions {
            agent_ids,
            since,
            dry_run,
            model: None,
            print_lessons: false,
        };
        let run = crate::lessons_backfill::run(&data_dir, llm, store, opts);
        match crate::user_config::as_principal(&principal, run).await {
            Ok(report) => set_job(
                &job_id_task,
                json!({
                    "status": "done",
                    "kind": "backfill",
                    "dry_run": dry_run,
                    "report": {
                        "agents_scanned": report.agents_scanned,
                        "sessions_scanned": report.sessions_scanned,
                        "turns_scanned": report.turns_scanned,
                        "signals_detected": report.signals_detected,
                        "lessons_written": report.lessons_written,
                    },
                }),
            ),
            Err(e) => set_job(
                &job_id_task,
                json!({ "status": "failed", "kind": "backfill", "error": e.to_string() }),
            ),
        }
    });

    Ok(Json(json!({ "job_id": job_id, "status": "running" })))
}

pub async fn lessons_job_status(
    Path(id): Path<String>,
    Query(query): Query<LessonsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<serde_json::Value>, ApiError> {
    crate::gateway::require_scope(
        &state,
        &headers,
        query.token.as_deref(),
        crate::jwt::SCOPE_ADMIN,
    )
    .await?;
    let value = JOBS
        .lock()
        .ok()
        .and_then(|jobs| jobs.get(&id).cloned())
        .ok_or_else(|| ApiError::not_found("job not found"))?;
    Ok(Json(value))
}
