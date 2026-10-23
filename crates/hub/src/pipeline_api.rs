//! `/api/pipelines` — pipelines over the gateway, for the UI and scripts.
//!
//! - `GET /api/pipelines` — every pipeline the caller may see (global, their
//!   own, and each agent's they can see in `/api/agents`), with item counts
//!   per column.
//! - `GET /api/pipelines/{scope}/{id}` — one definition with its version, the
//!   notices raised on it and neither cleared nor lapsed, and
//!   `can_view_costs`: whether this caller may read its items' costs.
//! - `GET /api/pipelines/{scope}/{id}/items` — the item records, newest
//!   movement first, each with its current column, a page at a time.
//!   `?view=board` reads every record and returns what the board shows: the
//!   finished items that entered a windowed column within its window, and
//!   every other item, with each windowed column's window and hidden count
//!   and the span of every item's history. `&windows=<window>` applies one
//!   window to every windowed column, `&windows=<column>:<window>,…` one per
//!   column.
//! - `GET /api/pipelines/{scope}/{id}/events` — every recorded event across
//!   the items, newest first, each with the column the definition puts it
//!   in, or whether it annotates the item instead: the feed and the replay
//!   read this.
//! - `GET /api/pipelines/{scope}/{id}/items/{item}/costs` — what one item has
//!   cost so far, from the step history of the runs its record names: billed
//!   and estimated totals, each stage with its models, each run with its
//!   steps, and what the figures do not cover.
//! - `GET /api/pipelines/{scope}/{id}/items/{item}/timings` — how long one
//!   item has taken so far: each stretch it spent in a column, the work its
//!   runs did and the waiting in between, and when it reached each milestone.
//!
//! Reads only: definitions are written through the `pipeline.define` tool
//! (validated, versioned) so a person, a workflow and the AI all go through
//! one door.
//!
//! An agent's pipelines are readable by everyone the `/api/agents` roster
//! shows the agent to; a hidden agent's pipeline answers as a missing one.
//! Timings are readable by everyone who can read the pipeline. Costs are
//! narrower: active admins on any pipeline, active managers on an agent's.
//! `/api/store` keeps the store's own policy, which leaves agent scopes to
//! admins and the agent's own runs.

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};
use workforce_common::telemetry::TelemetryStore;

use crate::authz::{is_effective_admin, is_effective_manager, Capability, Principal};
use crate::gateway::{require_capability_read, AgentViewer, GatewayState};
use crate::hub_core::pipelines::{Access, Caller, Run, Scope, StoreError};
use crate::pipeline::runs::{agent_of, runs_by_id, runs_named};
use crate::pipeline::{self, board, costs, notices, timings, PipelineDefinition, Rule};
use crate::store::{Document, DocumentStore, Query as StoreQuery};
use crate::store_api::{caller_from_request, store_error};

type ApiError = (StatusCode, Json<Value>);
type ApiResult = Result<Json<Value>, ApiError>;

#[derive(Debug, Deserialize)]
pub(crate) struct Params {
    token: Option<String>,
    limit: Option<usize>,
    cursor: Option<String>,
    view: Option<String>,
    windows: Option<String>,
}

fn err(status: StatusCode, code: &str, message: impl Into<String>) -> ApiError {
    (status, Json(json!({"error": message.into(), "code": code})))
}

fn store() -> Result<Arc<dyn DocumentStore>, ApiError> {
    crate::store::global_document_store().ok_or_else(|| {
        err(
            StatusCode::SERVICE_UNAVAILABLE,
            "unavailable",
            "document store is not configured on this hub",
        )
    })
}

fn backend(e: eyre::Report) -> ApiError {
    tracing::error!(error = %e, "pipeline request failed");
    err(
        StatusCode::INTERNAL_SERVER_ERROR,
        "backend",
        "pipeline request failed",
    )
}

fn not_found(scope: &Scope, id: &str) -> ApiError {
    err(
        StatusCode::NOT_FOUND,
        "not_found",
        format!("no pipeline `{id}` in {scope}"),
    )
}

/// A hidden agent's pipeline answers exactly as a missing one does.
fn read_refused(scope: &Scope, id: &str, e: StoreError) -> ApiError {
    match scope {
        Scope::Agent(_) => not_found(scope, id),
        _ => store_error(e),
    }
}

/// The store's read policy, widened for an agent's scope to every caller the
/// `/api/agents` roster shows that agent to.
fn readable(
    caller: &Caller,
    scope: &Scope,
    agent_visible: impl Fn(&str) -> bool,
) -> Result<(), StoreError> {
    caller
        .authorise(scope, Access::Read)
        .or_else(|refused| match scope {
            Scope::Agent(id) if agent_visible(id.as_str()) => Ok(()),
            _ => Err(refused),
        })
}

/// Whether `principal` may read what a pipeline's items cost: an active admin
/// on any pipeline, an active manager on an agent's.
fn may_view_costs(principal: &Principal, scope: &Scope) -> bool {
    let effective = match scope {
        Scope::Agent(_) => is_effective_manager,
        Scope::Global | Scope::User(_) => is_effective_admin,
    };
    effective(principal.role, principal.status)
}

fn item_label(def: &PipelineDefinition) -> &str {
    def.item_label.as_deref().unwrap_or("item")
}

fn no_item(def: &PipelineDefinition, id: &str, item: &str) -> ApiError {
    err(
        StatusCode::NOT_FOUND,
        "not_found",
        format!("no {} `{item}` in pipeline `{id}`", item_label(def)),
    )
}

/// The workflow runs an item's record names, by id, each with the agent it
/// executes as: the one it recorded, else the one its workflow declares.
async fn joined_runs(
    state: &GatewayState,
    telemetry: &dyn TelemetryStore,
    scope: &Scope,
    body: &Value,
) -> Result<HashMap<String, Run>, ApiError> {
    let rows = telemetry
        .get_workflow_instances(&runs_named(body))
        .map_err(backend)?;
    let declared = match (agent_of(scope), &state.workflow_runtime) {
        (Some(_), Some(runtime)) => {
            runtime
                .declared_agents(
                    &rows
                        .iter()
                        .map(|row| row.workflow_id.as_str())
                        .collect::<Vec<_>>(),
                )
                .await
        }
        _ => HashMap::new(),
    };
    Ok(runs_by_id(rows, &declared))
}

fn catalogue_scopes(
    caller: &Caller,
    agent_ids: impl IntoIterator<Item = String>,
    agent_visible: impl Fn(&str) -> bool,
) -> Vec<Scope> {
    std::iter::once(Scope::Global)
        .chain(agent_ids.into_iter().map(Scope::Agent))
        .chain(caller.identity.clone().map(Scope::User))
        .filter(|scope| readable(caller, scope, &agent_visible).is_ok())
        .collect()
}

struct Reader<'a> {
    caller: Caller,
    viewer: AgentViewer<'a>,
}

async fn admit<'a>(
    state: &'a GatewayState,
    headers: &axum::http::HeaderMap,
    token: Option<&str>,
) -> Result<(Reader<'a>, Arc<dyn DocumentStore>), ApiError> {
    let ctx = require_capability_read(state, headers, token, Capability::OwnDataRw).await?;
    let store = store()?;
    let reader = Reader {
        caller: caller_from_request(&ctx),
        viewer: AgentViewer::new(state, ctx),
    };
    Ok((reader, store))
}

async fn load(
    store: &dyn DocumentStore,
    reader: &Reader<'_>,
    scope_spec: &str,
    id: &str,
) -> Result<(Scope, PipelineDefinition, i64), ApiError> {
    let scope = Scope::parse(scope_spec, &reader.caller).map_err(store_error)?;
    readable(&reader.caller, &scope, |agent| reader.viewer.can_see(agent))
        .map_err(|e| read_refused(&scope, id, e))?;
    let (doc, def) = pipeline::load_definition(store, &scope, id)
        .await
        .map_err(store_error)?
        .ok_or_else(|| not_found(&scope, id))?;
    Ok((scope, def, doc.version))
}

fn summary_of(
    scope: &Scope,
    id: &str,
    def: &PipelineDefinition,
    version: i64,
    counts: pipeline::Counts,
) -> Value {
    json!({
        "scope": scope.to_string(),
        "pipeline": id,
        "name": def.name,
        "description": def.description,
        "item_label": item_label(def),
        "collection": def.collection,
        "lanes": def.lanes,
        "exits": def.exits,
        "sources": def.sources,
        "version": version,
        "counts": counts,
    })
}

/// Every pipeline in a scope the caller may read; the other scopes are left
/// out, not refused.
pub(crate) async fn catalogue(
    Query(p): Query<Params>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> ApiResult {
    let (reader, store) = admit(&state, &headers, p.token.as_deref()).await?;
    let scopes = catalogue_scopes(
        &reader.caller,
        state.registry.agent_ids().into_iter().map(|id| id.0),
        |agent| reader.viewer.can_see(agent),
    );
    let mut out = Vec::new();
    for scope in scopes {
        let defs = pipeline::list_definitions(store.as_ref(), &scope)
            .await
            .map_err(store_error)?;
        for (id, def, doc) in defs {
            let items = store
                .list(
                    &scope,
                    &def.collection,
                    &StoreQuery {
                        limit: Some(500),
                        ..Default::default()
                    },
                )
                .await
                .map_err(backend)?
                .documents;
            out.push(summary_of(
                &scope,
                &id,
                &def,
                doc.version,
                pipeline::count(&def, &items),
            ));
        }
    }
    Ok(Json(json!({ "count": out.len(), "pipelines": out })))
}

pub(crate) async fn get_definition(
    Path((scope, id)): Path<(String, String)>,
    Query(p): Query<Params>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> ApiResult {
    let (reader, store) = admit(&state, &headers, p.token.as_deref()).await?;
    let (scope, def, version) = load(store.as_ref(), &reader, &scope, &id).await?;
    let items = store
        .list(
            &scope,
            &def.collection,
            &StoreQuery {
                limit: Some(500),
                ..Default::default()
            },
        )
        .await
        .map_err(backend)?
        .documents;
    let mut out = summary_of(&scope, &id, &def, version, pipeline::count(&def, &items));
    out["definition"] = serde_json::to_value(&def).unwrap_or(Value::Null);
    out["can_view_costs"] = json!(may_view_costs(&reader.viewer.principal(), &scope));
    out["notices"] = json!(notices::active(store.as_ref(), &scope, &id)
        .await
        .map_err(backend)?);
    Ok(Json(out))
}

/// Every record in a collection, a page at a time, in the order the records
/// were created: a write between two pages never moves a record past the
/// cursor. A page that hands back the cursor it was asked with ends the read,
/// so a store that stops advancing cannot hold the request in a loop.
async fn every_record(
    store: &dyn DocumentStore,
    scope: &Scope,
    collection: &str,
) -> Result<Vec<Document>, ApiError> {
    let mut records = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let page = store
            .list(
                scope,
                collection,
                &StoreQuery {
                    order_by: Some("created_at".into()),
                    limit: Some(500),
                    cursor: cursor.clone(),
                    ..Default::default()
                },
            )
            .await
            .map_err(backend)?;
        records.extend(page.documents);
        match page.next_cursor {
            Some(next) if cursor.as_ref() != Some(&next) => cursor = Some(next),
            _ => return Ok(records),
        }
    }
}

/// What one item has cost so far. A pipeline the caller cannot see answers
/// 404 before the cost rule is consulted, so a refusal discloses nothing.
pub(crate) async fn item_costs(
    Path((scope, id, item)): Path<(String, String, String)>,
    Query(p): Query<Params>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> ApiResult {
    let (reader, store) = admit(&state, &headers, p.token.as_deref()).await?;
    let (scope, def, _) = load(store.as_ref(), &reader, &scope, &id).await?;
    if !may_view_costs(&reader.viewer.principal(), &scope) {
        return Err(err(
            StatusCode::FORBIDDEN,
            "denied",
            "pipeline costs are visible to admins, and to managers on an agent's pipeline",
        ));
    }
    let telemetry = state.telemetry_store.as_deref().ok_or_else(|| {
        err(
            StatusCode::SERVICE_UNAVAILABLE,
            "unavailable",
            "telemetry store is not configured on this hub",
        )
    })?;
    let records = every_record(store.as_ref(), &scope, &def.collection).await?;
    let doc = records
        .iter()
        .find(|d| d.doc_id == item)
        .ok_or_else(|| no_item(&def, &id, &item))?;
    let runs = joined_runs(&state, telemetry, &scope, &doc.body).await?;
    let report = costs::item_costs(
        &def,
        doc,
        &records,
        &runs,
        agent_of(&scope),
        chrono::Utc::now(),
    );
    if report.coverage.nothing_matched() {
        tracing::warn!(
            pipeline = %id,
            scope = %scope,
            item = %item,
            runs_named = report.coverage.runs_named,
            "no run the item's record names joins a workflow run this pipeline counts"
        );
    }
    located(report, &id, &scope)
}

/// How long one item has taken so far. Without a telemetry store the work
/// and waiting are left out rather than the request refused.
pub(crate) async fn item_timings(
    Path((scope, id, item)): Path<(String, String, String)>,
    Query(p): Query<Params>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> ApiResult {
    let (reader, store) = admit(&state, &headers, p.token.as_deref()).await?;
    let (scope, def, _) = load(store.as_ref(), &reader, &scope, &id).await?;
    let found = match crate::store::validate_segment("item", &item) {
        Ok(()) => store
            .get(&scope, &def.collection, &item)
            .await
            .map_err(backend)?,
        Err(_) => None,
    };
    let doc = found.ok_or_else(|| no_item(&def, &id, &item))?;
    let runs = match state.telemetry_store.as_deref() {
        Some(telemetry) => Some(joined_runs(&state, telemetry, &scope, &doc.body).await?),
        None => None,
    };
    let report = timings::item_timings(
        &def,
        &doc,
        runs.as_ref(),
        agent_of(&scope),
        chrono::Utc::now(),
    );
    located(report, &id, &scope)
}

/// An item report with the pipeline and scope it was read from.
fn located(report: impl serde::Serialize, id: &str, scope: &Scope) -> ApiResult {
    let mut out = serde_json::to_value(report).map_err(|e| backend(e.into()))?;
    out["pipeline"] = json!(id);
    out["scope"] = json!(scope.to_string());
    Ok(Json(out))
}

/// How `/items` reads: one page of records, or the whole board.
#[derive(Debug, PartialEq)]
enum ItemsRead {
    Page,
    Board(board::Overrides),
}

fn items_read(p: &Params) -> Result<ItemsRead, ApiError> {
    let invalid = |message: String| err(StatusCode::BAD_REQUEST, "invalid", message);
    match p.view.as_deref() {
        None if p.windows.is_some() => Err(invalid("`windows` needs `view=board`".into())),
        None => Ok(ItemsRead::Page),
        Some("board") if p.limit.is_some() || p.cursor.is_some() => Err(invalid(
            "`view=board` reads every item, so it takes no `limit` or `cursor`".into(),
        )),
        Some("board") => p
            .windows
            .as_deref()
            .map_or(Ok(board::Overrides::default()), str::parse)
            .map(ItemsRead::Board)
            .map_err(invalid),
        Some(view) => Err(invalid(format!("`view` may only be `board`, not `{view}`"))),
    }
}

/// One item as `/items` sends it, in either read.
fn item_json(def: &PipelineDefinition, doc: &Document) -> Value {
    json!({
        "item": doc.doc_id,
        "column": pipeline::position_of(def, &doc.body),
        "version": doc.version,
        "updated_at": doc.updated_at,
        "record": doc.body,
    })
}

pub(crate) async fn items(
    Path((scope, id)): Path<(String, String)>,
    Query(p): Query<Params>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> ApiResult {
    let (reader, store) = admit(&state, &headers, p.token.as_deref()).await?;
    let (scope, def, _) = load(store.as_ref(), &reader, &scope, &id).await?;
    match items_read(&p)? {
        ItemsRead::Page => {
            let query = StoreQuery {
                limit: Some(p.limit.unwrap_or(500)),
                cursor: p.cursor.clone(),
                direction: crate::store::Direction::Desc,
                ..Default::default()
            };
            let page = store
                .list(&scope, &def.collection, &query)
                .await
                .map_err(backend)?;
            let items: Vec<Value> = page.documents.iter().map(|d| item_json(&def, d)).collect();
            Ok(Json(
                json!({ "pipeline": id, "scope": scope.to_string(), "count": items.len(), "items": items, "next_cursor": page.next_cursor }),
            ))
        }
        ItemsRead::Board(overrides) => {
            let records = every_record(store.as_ref(), &scope, &def.collection).await?;
            let view = board::board_view(&def, &records, &overrides, chrono::Utc::now());
            let items: Vec<Value> = view.items.iter().map(|d| item_json(&def, d)).collect();
            Ok(Json(
                json!({ "pipeline": id, "scope": scope.to_string(), "count": items.len(), "items": items, "windows": view.windows, "span": view.span }),
            ))
        }
    }
}

/// Every event across the pipeline's items, newest first, with the column
/// the definition puts it in — one flat feed for the page and the replay.
pub(crate) async fn events(
    Path((scope, id)): Path<(String, String)>,
    Query(p): Query<Params>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> ApiResult {
    let (reader, store) = admit(&state, &headers, p.token.as_deref()).await?;
    let (scope, def, _) = load(store.as_ref(), &reader, &scope, &id).await?;
    let page = store
        .list(
            &scope,
            &def.collection,
            &StoreQuery {
                limit: Some(500),
                ..Default::default()
            },
        )
        .await
        .map_err(backend)?;
    let mut events: Vec<Value> = Vec::new();
    for d in &page.documents {
        let Some(history) = d.body.get("history").and_then(Value::as_array) else {
            continue;
        };
        for e in history {
            let stage = e.get("stage").and_then(Value::as_str).unwrap_or("");
            let event = e.get("event").and_then(Value::as_str).unwrap_or("");
            let detail = e.get("detail").cloned().unwrap_or(Value::Null);
            let rule = def.rule_for(stage, event, &detail);
            events.push(json!({
                "item": d.doc_id,
                "at": e.get("at"),
                "stage": stage,
                "event": event,
                "run": e.get("run"),
                "detail": detail,
                "column": rule.and_then(Rule::places),
                "tone": rule.and_then(|r| r.tone),
                "label": rule.map(|r| def.label_of(r, stage, event, &detail)),
                "annotation": rule.is_some_and(|r| r.annotation),
            }));
        }
    }
    events.sort_by(|a, b| {
        b["at"]
            .as_str()
            .unwrap_or("")
            .cmp(a["at"].as_str().unwrap_or(""))
    });
    let limit = p.limit.unwrap_or(200).clamp(1, 2000);
    events.truncate(limit);
    Ok(Json(
        json!({ "pipeline": id, "scope": scope.to_string(), "count": events.len(), "events": events }),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::{gateway_principal, CallerContext};
    use crate::store::intercepted::{Intercepted, Interceptor};
    use workforce_identity::types::{IdentityRole, IdentityStatus};

    fn principal(role: IdentityRole, status: IdentityStatus) -> Principal {
        Principal {
            identity_id: None,
            role_key: role.as_str().to_string(),
            role,
            status,
        }
    }

    fn cost_scopes() -> [Scope; 3] {
        [
            Scope::Global,
            Scope::User("alice".into()),
            Scope::Agent("example".into()),
        ]
    }

    fn sees_costs(principal: &Principal) -> Vec<bool> {
        cost_scopes()
            .iter()
            .map(|scope| may_view_costs(principal, scope))
            .collect()
    }

    #[test]
    fn an_active_admin_sees_costs_on_every_pipeline() {
        let admin = principal(IdentityRole::SystemAdmin, IdentityStatus::Active);
        assert_eq!(sees_costs(&admin), [true, true, true]);
    }

    #[test]
    fn a_manager_sees_costs_on_agent_pipelines_only() {
        let manager = principal(IdentityRole::Manager, IdentityStatus::Active);
        assert_eq!(sees_costs(&manager), [false, false, true]);
    }

    #[test]
    fn a_member_an_inactive_admin_or_manager_and_a_paired_device_never_see_costs() {
        let member = principal(IdentityRole::Member, IdentityStatus::Active);
        let suspended = principal(IdentityRole::SystemAdmin, IdentityStatus::Suspended);
        let provisioned_admin =
            principal(IdentityRole::SystemAdmin, IdentityStatus::AutoProvisioned);
        let provisioned_manager = principal(IdentityRole::Manager, IdentityStatus::AutoProvisioned);
        let device = gateway_principal(
            &CallerContext {
                identity_id: Some(uuid::Uuid::new_v4().to_string()),
                device_id: Some("phone".into()),
                is_admin: true,
                ..Default::default()
            },
            None,
        );
        for nobody in [
            member,
            suspended,
            provisioned_admin,
            provisioned_manager,
            device,
        ] {
            assert_eq!(sees_costs(&nobody), [false, false, false], "{nobody:?}");
        }
    }

    fn member(id: &str) -> Caller {
        Caller {
            identity: Some(id.into()),
            ..Default::default()
        }
    }

    fn admin() -> Caller {
        Caller {
            admin: true,
            ..Default::default()
        }
    }

    fn example_only(id: &str) -> bool {
        id == "example"
    }

    fn agents() -> Vec<String> {
        vec!["example".into(), "quill".into()]
    }

    #[test]
    fn global_pipelines_are_readable_by_every_caller() {
        for caller in [member("alice"), Caller::default()] {
            assert!(readable(&caller, &Scope::Global, |_| false).is_ok());
        }
    }

    #[test]
    fn a_member_reads_the_pipelines_of_an_agent_they_can_see() {
        let scope = Scope::Agent("example".into());
        assert!(readable(&member("alice"), &scope, example_only).is_ok());
    }

    #[test]
    fn a_member_cannot_read_the_pipelines_of_an_agent_they_cannot_see() {
        let scope = Scope::Agent("quill".into());
        assert!(matches!(
            readable(&member("alice"), &scope, example_only),
            Err(StoreError::Denied(_))
        ));
    }

    #[test]
    fn an_allowlisted_member_reads_the_locked_agents_pipeline() {
        let scope = Scope::Agent("quill".into());
        assert!(readable(&member("alice"), &scope, |id| id == "quill").is_ok());
    }

    #[test]
    fn an_admin_reads_every_agent_scope_without_the_roster_rule() {
        let never_consulted = |_: &str| -> bool { panic!("an admin needs no roster rule") };
        let scope = Scope::Agent("quill".into());
        assert!(readable(&admin(), &scope, never_consulted).is_ok());
    }

    #[test]
    fn own_user_scope_is_readable() {
        let scope = Scope::User("alice".into());
        assert!(readable(&member("alice"), &scope, |_| false).is_ok());
    }

    #[test]
    fn another_identitys_user_scope_stays_private() {
        let scope = Scope::User("bob".into());
        let refused = readable(&member("alice"), &scope, |_| true).expect_err("bob's scope");
        let (status, Json(body)) = read_refused(&scope, "orders", refused);
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body["code"], "denied");
    }

    #[test]
    fn a_hidden_agents_pipeline_answers_as_a_missing_one() {
        let scope = Scope::Agent("quill".into());
        let refused = readable(&member("alice"), &scope, example_only).expect_err("quill");
        let (status, Json(body)) = read_refused(&scope, "orders", refused);
        let (missing_status, Json(missing)) = not_found(&scope, "orders");
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(status, missing_status);
        assert_eq!(body, missing);
    }

    #[test]
    fn a_bare_agent_scope_is_a_bad_request() {
        let ctx = CallerContext {
            identity_id: Some("alice".into()),
            ..Default::default()
        };
        let refused = Scope::parse("agent", &caller_from_request(&ctx)).expect_err("no agent");
        let (status, Json(body)) = store_error(refused);
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["code"], "invalid");
    }

    #[test]
    fn the_catalogue_lists_only_scopes_the_caller_may_read() {
        assert_eq!(
            catalogue_scopes(&member("alice"), agents(), example_only),
            [
                Scope::Global,
                Scope::Agent("example".into()),
                Scope::User("alice".into()),
            ]
        );
    }

    #[test]
    fn the_catalogue_lists_every_agent_for_an_admin() {
        assert_eq!(
            catalogue_scopes(&admin(), agents(), |_| false),
            [
                Scope::Global,
                Scope::Agent("example".into()),
                Scope::Agent("quill".into()),
            ]
        );
    }

    #[test]
    fn the_catalogue_has_no_user_scope_without_an_identity() {
        assert_eq!(
            catalogue_scopes(&Caller::default(), agents(), |_| true),
            [
                Scope::Global,
                Scope::Agent("example".into()),
                Scope::Agent("quill".into()),
            ]
        );
    }

    #[tokio::test]
    async fn every_record_reads_past_the_first_page() {
        let store = crate::store::MemoryDocumentStore::new();
        let scope = Scope::Agent("example".into());
        for i in 0..1_203 {
            store
                .set(
                    &scope,
                    "pipeline",
                    &format!("ORD-{i}"),
                    json!({}),
                    None,
                    None,
                    None,
                )
                .await
                .unwrap();
        }
        let records = every_record(&store, &scope, "pipeline").await.unwrap();
        let ids: std::collections::HashSet<&str> =
            records.iter().map(|d| d.doc_id.as_str()).collect();
        assert_eq!((records.len(), ids.len()), (1_203, 1_203));
        assert!(every_record(&store, &scope, "empty")
            .await
            .unwrap()
            .is_empty());
    }

    /// Another run records an event on `touched`, a record on the first page,
    /// just before the second page is read.
    struct Touch {
        touched: &'static str,
        lists: std::sync::atomic::AtomicUsize,
    }

    #[async_trait::async_trait]
    impl Interceptor for Touch {
        async fn before_list(
            &self,
            inner: &dyn DocumentStore,
            scope: &Scope,
            collection: &str,
        ) -> eyre::Result<()> {
            if self.lists.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 1 {
                let doc = inner
                    .get(scope, collection, self.touched)
                    .await?
                    .expect("the touched record");
                let checked = pipeline::EventInput {
                    item: self.touched.into(),
                    stage: "verify".into(),
                    event: "checked".into(),
                    detail: Value::Null,
                    run: None,
                };
                let body = pipeline::apply_event(doc.body, &checked, "2026-09-15T07:00:00Z", None);
                inner
                    .set(scope, collection, self.touched, body, None, None, None)
                    .await?;
            }
            Ok(())
        }
    }

    async fn reads_each_record_once_while_one_on_the_first_page_is_written(
        inner: impl DocumentStore,
    ) {
        let store = Intercepted {
            inner,
            interceptor: Touch {
                touched: "ORD-0",
                lists: Default::default(),
            },
        };
        let scope = Scope::Agent("example".into());
        for i in 0..1_203 {
            let body = json!({"item": format!("ORD-{i}"), "created_at": "2026-09-15T06:00:00Z"});
            store
                .inner
                .set(
                    &scope,
                    "pipeline",
                    &format!("ORD-{i}"),
                    body,
                    None,
                    None,
                    None,
                )
                .await
                .unwrap();
        }
        let records = every_record(&store, &scope, "pipeline").await.unwrap();
        let ids: std::collections::HashSet<&str> =
            records.iter().map(|d| d.doc_id.as_str()).collect();
        assert_eq!((records.len(), ids.len()), (1_203, 1_203));
        assert!(
            store
                .interceptor
                .lists
                .load(std::sync::atomic::Ordering::SeqCst)
                >= 2
        );
    }

    #[tokio::test]
    async fn every_record_reads_each_record_once_while_one_on_the_first_page_is_written() {
        reads_each_record_once_while_one_on_the_first_page_is_written(
            crate::store::MemoryDocumentStore::new(),
        )
        .await;
    }

    #[cfg(feature = "postgres")]
    #[tokio::test(flavor = "multi_thread")]
    async fn every_record_on_postgres_reads_each_record_once_while_one_on_the_first_page_is_written(
    ) {
        workforce_postgres::test_support::with_test_pool(
            crate::store::postgres::build_pool,
            |pool| async move {
                crate::store::postgres::run_migrations(&pool).await.unwrap();
                reads_each_record_once_while_one_on_the_first_page_is_written(
                    crate::store::postgres::PostgresDocumentStore::new(pool),
                )
                .await;
            },
        )
        .await;
    }

    fn params(query: &str) -> Params {
        let uri: axum::http::Uri = format!("/items?{query}").parse().unwrap();
        Query::<Params>::try_from_uri(&uri).unwrap().0
    }

    #[test]
    fn items_read_a_page_unless_the_board_view_is_asked_for() {
        assert_eq!(items_read(&params("")).unwrap(), ItemsRead::Page);
        assert_eq!(
            items_read(&params("limit=10&cursor=abc")).unwrap(),
            ItemsRead::Page
        );
        assert_eq!(
            items_read(&params("view=board")).unwrap(),
            ItemsRead::Board(board::Overrides::default())
        );
        assert_eq!(
            items_read(&params("view=board&windows=all")).unwrap(),
            ItemsRead::Board(board::Overrides::Every(pipeline::Window::All))
        );
    }

    #[test]
    fn items_read_refuses_what_the_board_view_cannot_honour() {
        for (query, message) in [
            ("view=list", "`view` may only be `board`, not `list`"),
            ("windows=7d", "`windows` needs `view=board`"),
            (
                "view=board&limit=10",
                "`view=board` reads every item, so it takes no `limit` or `cursor`",
            ),
            (
                "view=board&cursor=x",
                "`view=board` reads every item, so it takes no `limit` or `cursor`",
            ),
            (
                "view=board&windows=verified:2h",
                "`windows` entry `verified:2h`: `2h` is shorter than 24 hours",
            ),
        ] {
            let (status, Json(body)) = items_read(&params(query)).unwrap_err();
            assert_eq!(status, StatusCode::BAD_REQUEST, "{query}");
            assert_eq!(
                body,
                json!({"code": "invalid", "error": message}),
                "{query}"
            );
        }
    }
}
