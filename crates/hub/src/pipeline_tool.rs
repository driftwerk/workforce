//! `pipeline.*` — pipelines as platform tools, for agent turns ("define a
//! pipeline for the onboarding workflow", "rename the Reviewing lane") and
//! for workflow tool steps (`tool = "pipeline.record"`), so a workflow
//! records its own transitions with no skill in between.
//!
//! Scope and policy are the document store's: `scope` is `agent` (the
//! calling agent's own — the default), `agent:<id>`, `user`, `user:<id>` or
//! `global`, and [`crate::store::Caller::authorise`] decides. `pipeline.runs`
//! and `pipeline.facts` read the hub beyond the documents under that same
//! rule, on a global or user pipeline for admins and system runs only, and
//! exist only where the hub runs workflows.

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use eyre::Result;
use serde_json::{json, Value};
use workforce_agent::tools::registry::ToolRegistry;
use workforce_agent::tools::{Tool, ToolResult};
use workforce_common::persistence::InstanceFilter;

use crate::authz::IsAdminFn;
use crate::pipeline::runs::{agent_of, joinable, run_view, RunView, StepDetail};
use crate::pipeline::{
    self, notices, position, EventInput, HubReads, PipelineDefinition, Unless, DEFINITIONS,
};
use crate::store::{Access, Caller, DocumentStore, Query, Scope, StoreError};
use crate::store_tool::caller_from_context;

/// The most runs one `pipeline.runs` call names or returns.
const RUNS_MAX: usize = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PipelineOp {
    Define,
    List,
    Get,
    Delete,
    Record,
    Item,
    Items,
    Notice,
    Runs,
    Facts,
}

impl PipelineOp {
    /// The ops over a pipeline's own documents.
    pub const DOCUMENTS: [PipelineOp; 8] = [
        PipelineOp::Define,
        PipelineOp::List,
        PipelineOp::Get,
        PipelineOp::Delete,
        PipelineOp::Record,
        PipelineOp::Item,
        PipelineOp::Items,
        PipelineOp::Notice,
    ];

    /// The ops that read the hub beyond the documents.
    pub const HUB: [PipelineOp; 2] = [PipelineOp::Runs, PipelineOp::Facts];

    fn name(self) -> &'static str {
        match self {
            PipelineOp::Define => "pipeline.define",
            PipelineOp::List => "pipeline.list",
            PipelineOp::Get => "pipeline.get",
            PipelineOp::Delete => "pipeline.delete",
            PipelineOp::Record => "pipeline.record",
            PipelineOp::Item => "pipeline.item",
            PipelineOp::Items => "pipeline.items",
            PipelineOp::Notice => "pipeline.notice",
            PipelineOp::Runs => "pipeline.runs",
            PipelineOp::Facts => "pipeline.facts",
        }
    }

    pub fn parse(op: &str) -> Option<PipelineOp> {
        Self::DOCUMENTS
            .into_iter()
            .chain(Self::HUB)
            .find(|o| o.name().strip_prefix("pipeline.") == Some(op))
    }

    fn access(self) -> Access {
        match self {
            PipelineOp::List
            | PipelineOp::Get
            | PipelineOp::Item
            | PipelineOp::Items
            | PipelineOp::Runs
            | PipelineOp::Facts => Access::Read,
            _ => Access::Write,
        }
    }

    fn description(self) -> &'static str {
        match self {
            PipelineOp::Define => "Create or replace a pipeline definition: the lanes an item moves through, the \
                exits it can take, and the rules that put a recorded `stage/event` on a column. Validated \
                before it is stored — errors name what to fix; warnings (a lane nothing lands in, a rule \
                that can never match) come back with the saved definition. To tweak an existing pipeline, \
                pipeline.get it, change the definition, and define it again with the version you read. \
                Definition fields: name, description, guide (markdown shown on the board's \"How this pipeline \
                works\" section — what each lane means, what heals itself, what needs a person; up to 64 KiB), \
                item_label, collection (where item records live, in \
                the same scope), lanes [{id, label, window?}] in order, exits [{id, label, window?}] \
                (window: how long a finished item stays on the board after entering the column, as \
                `<n>h` or `<n>d` from 24h to 3650d, or `all`; only on a column in timings.ends, never on \
                one a warn or bad rule lands in, and an item whose standing annotation is warn or bad stays \
                shown), board {cards?: how many cards a column shows before it scrolls, 1 to 50, default \
                10}, rungs [\"1h\", …] (a \
                ladder shown on items whose rule names a rung_field), events {\"stage/event\": [rules]} where \
                a rule is {when?: {detail_field: value}, column (a lane or exit id; omitted on an annotation), \
                annotation? (true: the event is noted on the item without moving it or changing its current \
                stage; such a rule sets no column, claim, lease_field, due_field or rung fields), \
                tone?: active|good|warn|healing|bad|neutral \
                (warn = a person must act; healing = the pipeline is still working on it, nothing to do), \
                label?, label_field? (a detail field whose text replaces label when it has any: one line, at \
                most 120 characters, with < and > removed), rung_field?, rung_passed?, claim? (true when the \
                event means a run is working the item now; it holds while the event is the item's latest), lease_field? (with claim: the detail field \
                holding the claim's lease in minutes, past which it no longer holds), due_field? (not with claim: \
                the detail field holding when the item is next due, RFC 3339)} and the first rule whose `when` \
                holds wins, summary_field (dotted path into the record, e.g. stages.implement.summary), \
                link_fields [{field, label}], sources [\"workflow:<id>\", \"skill:<name>\"], timings \
                {origin?: {field, label} (the detail field a source records when the item really began, e.g. \
                filed_at, and what that moment is called; else the clock starts when the pipeline first \
                recorded the item), milestones? [lane ids timed from the origin; default the last lane], ends? \
                [lane or exit ids where the item's clock stops; default the last lane]}. To define one for a \
                workflow: its steps become stages, its branch outcomes become events, and each `goto` tells \
                you which lane or exit the event lands in. Call with example=true to see a complete definition.",
            PipelineOp::List => "List the pipelines defined in a scope, with item counts per column and when \
                something last moved.",
            PipelineOp::Get => "Read one pipeline's definition (and its document version, needed to redefine \
                it safely) plus current item counts per column.",
            PipelineOp::Delete => "Delete a pipeline definition. Its item records are left in place; delete \
                them with store.delete or store.batch if they should go too.",
            PipelineOp::Record => "Record that an item moved: `{pipeline, item, stage, event, detail?, unless?}`. \
                The item's record is created on first use; the stage's block takes the detail fields, and the \
                event joins the item's history. Concurrent writers never lose an event. Returns the column \
                the definition puts the item in and its label — and a warning when the definition has no rule \
                for this stage/event, which usually means a typo or a rule to add. An event whose rule is an \
                annotation is noted on the item without moving it: the reply has `annotation: true` and no \
                column. `unless: {events: [\"stage/event\" or \"stage/*\"], detail?: {field: value}}` records \
                the event only when the item's history holds none of those events (with `detail`, none whose \
                detail matches); otherwise nothing is written and the reply has `recorded: false`, `skipped: \
                \"unless\"` and the `matched` event. From a workflow tool step, pass nothing for the run: the \
                hub supplies the instance id. `dry_run: true` (or the string \"true\") resolves the event, \
                `unless` included, and writes nothing — for backtests and rehearsals.",
            PipelineOp::Item => "Read one item's record — its current stage, every stage block and its history — \
                with its `position` ({column, tone, label, at, annotation: the annotation {tone, label, at} \
                noted since it last moved, or null}) and its `activity` at the hub's clock: {kind: working, \
                since, until} while a run's claim holds (until is null when its rule names no lease), {kind: \
                scheduled, due} when it is next due or its claim's lease has run out, else {kind: other}.",
            PipelineOp::Items => "List a pipeline's item records, newest movement first, each with its current \
                column, `position` and `activity` as pipeline.item gives them. Pages of up to 500; pass \
                next_cursor back as cursor.",
            PipelineOp::Notice => "Raise or clear a notice on a pipeline's board, for everyone who can see it: \
                `{pipeline, op: \"raise\", key, tone, title, detail?, link?, ttl_secs?}` shows one line above \
                the board's stats — a worker on an old protocol, a queue left idle — until `{pipeline, op: \
                \"clear\", key}` or until its ttl lapses. Raising a key again replaces its notice but keeps \
                when it was raised, so a viewer who dismissed it does not see it again until it is cleared or \
                lapses and is raised anew. `key` is a \
                lowercase id; `tone` is active|good|warn|healing|bad|neutral; `title` is one line of up to 200 \
                characters; `detail` is up to 2,000; `link` is an http(s) URL or a path on the hub starting \
                with /, with no whitespace or backslash; `ttl_secs` is up to 30 days. A viewer may dismiss a \
                notice for themselves only.",
            PipelineOp::Runs => "Read the workflow runs a pipeline may join — on an agent's pipeline, only runs \
                that execute as that agent; on a global or user pipeline, every run, for admins and system \
                runs only — newest update first: `{pipeline, runs?: [instance ids, at most \
                200], workflows?: [workflow ids, each matching members' copies too], statuses?: [running, \
                completed, failed, …], since?: RFC 3339, steps?: none|coding|all, limit?: 1-200}`. An empty \
                list matches nothing. Each run has its workflow, status, current step, agent, lease \
                ({expires_at, live}: live until the lease runs out) and failure reason; with `steps`, each step \
                (coding: only those a coding engine served) has its outcome, times, engine, model and the \
                attempts its fallback chain handed over with their errors. Errors are redacted and cut to 300 \
                characters; a step's response and outputs are never included.",
            PipelineOp::Facts => "Read what the workflows serving a pipeline depend on: `{pipeline}` gives the \
                hub's build; its runner daemons (`local`, `remote`) with the protocol each last announced, the \
                protocol this hub needs, their health and their last refusal; the skills those workflows' tool \
                steps call that the caller may run, with version, source commit, sha256, contract fingerprint, tools and the tools \
                grant (valid, stale or missing) of the calling identity and of the org; and each workflow — an \
                agent's workflows on its pipeline, else those its `sources` name — with its version, content \
                hash, tools and schedules, each schedule with how its latest runs went (`last_12`: \
                considered, started, skipped at the concurrency cap, failed).",
        }
    }
}

const SCOPE_DESCRIPTION: &str =
    "Where the pipeline lives: `agent` (the calling agent's own — the default), \
    `agent:<id>`, `user`, `user:<id>` or `global`.";

pub struct PipelineTool {
    store: Arc<dyn DocumentStore>,
    is_admin: IsAdminFn,
    op: PipelineOp,
    hub: Option<Arc<dyn HubReads>>,
}

impl PipelineTool {
    pub fn new(store: Arc<dyn DocumentStore>, is_admin: IsAdminFn, op: PipelineOp) -> Self {
        Self {
            store,
            is_admin,
            op,
            hub: None,
        }
    }

    /// The tool reading the hub through `hub` where its op needs it.
    pub fn with_hub(self, hub: Arc<dyn HubReads>) -> Self {
        Self {
            hub: Some(hub),
            ..self
        }
    }
}

/// Register the ops over a pipeline's documents.
pub fn register(tools: &ToolRegistry, store: Arc<dyn DocumentStore>, is_admin: IsAdminFn) {
    for op in PipelineOp::DOCUMENTS {
        tools.register(Arc::new(PipelineTool::new(
            Arc::clone(&store),
            Arc::clone(&is_admin),
            op,
        )));
    }
}

/// Register `pipeline.runs` and `pipeline.facts`, read through `hub`.
pub fn register_hub_reads(
    tools: &ToolRegistry,
    store: Arc<dyn DocumentStore>,
    is_admin: IsAdminFn,
    hub: Arc<dyn HubReads>,
) {
    for op in PipelineOp::HUB {
        tools.register(Arc::new(
            PipelineTool::new(Arc::clone(&store), Arc::clone(&is_admin), op)
                .with_hub(Arc::clone(&hub)),
        ));
    }
}

fn str_field<'a>(input: &'a Value, key: &str) -> Result<&'a str, StoreError> {
    input
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| StoreError::Invalid(format!("`{key}` is required")))
}

fn i64_field(input: &Value, key: &str) -> Result<Option<i64>, StoreError> {
    match input.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_i64()
            .map(Some)
            .ok_or_else(|| StoreError::Invalid(format!("`{key}` must be an integer"))),
    }
}

/// An optional list of strings; present but empty is a list that matches nothing.
fn texts_field(input: &Value, key: &str) -> Result<Option<Vec<String>>, StoreError> {
    let refused = || StoreError::Invalid(format!("`{key}` must be a list of strings"));
    match input.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| item.as_str().map(str::to_string))
            .collect::<Option<Vec<_>>>()
            .map(Some)
            .ok_or_else(refused),
        Some(_) => Err(refused()),
    }
}

fn run_filter(input: &Value) -> Result<InstanceFilter, StoreError> {
    let ids = texts_field(input, "runs")?;
    if ids.as_ref().is_some_and(|ids| ids.len() > RUNS_MAX) {
        return Err(StoreError::Invalid(format!(
            "`runs` names at most {RUNS_MAX} runs"
        )));
    }
    let since = match input.get("since") {
        None | Some(Value::Null) => None,
        Some(value) => Some(
            value
                .as_str()
                .and_then(|at| DateTime::parse_from_rfc3339(at).ok())
                .map(|at| at.with_timezone(&Utc))
                .ok_or_else(|| StoreError::Invalid("`since` must be an RFC 3339 time".into()))?,
        ),
    };
    let limit = i64_field(input, "limit")?.map_or(RUNS_MAX, |limit| {
        usize::try_from(limit.max(1)).map_or(RUNS_MAX, |limit| limit.min(RUNS_MAX))
    });
    Ok(InstanceFilter {
        ids,
        workflows: texts_field(input, "workflows")?,
        statuses: texts_field(input, "statuses")?,
        since,
        runs_as: None,
        limit,
    })
}

fn step_detail(input: &Value) -> Result<StepDetail, StoreError> {
    input
        .get("steps")
        .filter(|steps| !steps.is_null())
        .map_or(Ok(StepDetail::None), |steps| {
            serde_json::from_value(steps.clone())
                .map_err(|_| StoreError::Invalid("`steps` is none, coding or all".into()))
        })
}

/// The run doing the recording: the workflow instance the hub put on a
/// tool step's input, else the session.
fn run_of(input: &Value) -> Option<String> {
    input
        .get("run")
        .or_else(|| input.get("_instance_id"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| {
            workforce_agent::runner::CURRENT_SESSION_ID
                .try_with(|s| s.clone())
                .ok()
                .filter(|s| !s.is_empty())
        })
}

fn failure(start: Instant, e: StoreError) -> ToolResult {
    ToolResult::failure(
        format!("{e} ({})", e.code()),
        start.elapsed().as_millis() as u64,
    )
}

async fn defined(
    store: &dyn DocumentStore,
    scope: &Scope,
    input: &Value,
) -> Result<PipelineDefinition, StoreError> {
    let id = str_field(input, "pipeline")?;
    pipeline::load_definition(store, scope, id)
        .await?
        .map(|(_, def)| def)
        .ok_or(StoreError::NotFound)
}

/// The hub, for an op in `scope` that reads it. A global or user pipeline
/// joins every agent's runs and may name any workflow, so only admins and
/// system runs read the hub through one.
fn reading_hub<'a>(
    hub: Option<&'a dyn HubReads>,
    op: PipelineOp,
    caller: &Caller,
    scope: &Scope,
) -> Result<&'a dyn HubReads, StoreError> {
    let hub = hub.ok_or_else(|| {
        StoreError::Invalid(format!(
            "{} reads the hub's workflow runtime, which does not run here",
            op.name()
        ))
    })?;
    if agent_of(scope).is_none() && !(caller.admin || caller.system) {
        return Err(StoreError::Denied(format!(
            "{} on a global or user pipeline reads every agent's workflows, so only admins \
             and system runs may call it there; an agent's pipeline reads that agent's",
            op.name()
        )));
    }
    Ok(hub)
}

fn hub_failed(e: eyre::Report) -> StoreError {
    StoreError::Backend(e.to_string())
}

/// Run one pipeline op for `caller`, reading the hub through `hub` for the
/// ops that need it. Shared by the tools and any bridge.
pub async fn perform(
    store: &dyn DocumentStore,
    hub: Option<&dyn HubReads>,
    caller: &Caller,
    op: PipelineOp,
    input: &Value,
) -> Result<Value, StoreError> {
    if op == PipelineOp::Define && input.get("example").and_then(Value::as_bool) == Some(true) {
        return Ok(json!({ "example": pipeline::example() }));
    }
    let scope_spec = input
        .get("scope")
        .and_then(Value::as_str)
        .unwrap_or("agent");
    let scope = Scope::parse(scope_spec, caller)?;
    caller.authorise(&scope, op.access())?;
    let actor = caller.identity.clone().or_else(|| caller.agent_id.clone());
    match op {
        PipelineOp::Define => {
            let id = str_field(input, "pipeline").or_else(|_| str_field(input, "id"))?;
            let def: PipelineDefinition =
                serde_json::from_value(input.get("definition").cloned().ok_or_else(|| {
                    StoreError::Invalid(
                        "`definition` is required (call with example=true to see one)".into(),
                    )
                })?)
                .map_err(|e| StoreError::Invalid(format!("definition does not parse: {e}")))?;
            let (doc, findings) = pipeline::define(
                store,
                &scope,
                id,
                &def,
                i64_field(input, "if_version")?,
                actor.as_deref(),
            )
            .await?;
            Ok(json!({
                "defined": true,
                "pipeline": id,
                "scope": scope.to_string(),
                "version": doc.version,
                "warnings": findings.warnings,
                "board": format!("/pipelines/{}/{id}", scope),
            }))
        }
        PipelineOp::List => {
            let defs = pipeline::list_definitions(store, &scope).await?;
            let mut out = Vec::with_capacity(defs.len());
            for (id, def, doc) in defs {
                let items = store
                    .list(
                        &scope,
                        &def.collection,
                        &Query {
                            limit: Some(500),
                            ..Default::default()
                        },
                    )
                    .await?
                    .documents;
                let counts = pipeline::count(&def, &items);
                out.push(json!({
                    "pipeline": id,
                    "scope": scope.to_string(),
                    "name": def.name,
                    "description": def.description,
                    "item_label": def.item_label,
                    "collection": def.collection,
                    "lanes": def.lanes.iter().map(|l| &l.id).collect::<Vec<_>>(),
                    "version": doc.version,
                    "counts": counts,
                    "board": format!("/pipelines/{}/{id}", scope),
                }));
            }
            Ok(json!({ "count": out.len(), "pipelines": out }))
        }
        PipelineOp::Get => {
            let id = str_field(input, "pipeline")?;
            let (doc, def) = pipeline::load_definition(store, &scope, id)
                .await?
                .ok_or(StoreError::NotFound)?;
            let items = store
                .list(
                    &scope,
                    &def.collection,
                    &Query {
                        limit: Some(500),
                        ..Default::default()
                    },
                )
                .await?
                .documents;
            Ok(json!({
                "pipeline": id,
                "scope": scope.to_string(),
                "version": doc.version,
                "definition": def,
                "counts": pipeline::count(&def, &items),
                "board": format!("/pipelines/{}/{id}", scope),
            }))
        }
        PipelineOp::Delete => {
            let id = str_field(input, "pipeline")?;
            let existed = store
                .delete(
                    &scope,
                    DEFINITIONS,
                    id,
                    i64_field(input, "if_version")?,
                    actor.as_deref(),
                )
                .await?;
            Ok(json!({ "deleted": true, "existed": existed }))
        }
        PipelineOp::Record => {
            let id = str_field(input, "pipeline")?;
            let (_, def) = pipeline::load_definition(store, &scope, id)
                .await?
                .ok_or_else(|| {
                    StoreError::Invalid(format!(
                        "no pipeline `{id}` in {scope}; define it first with pipeline.define"
                    ))
                })?;
            let event = EventInput {
                item: str_field(input, "item")?.to_string(),
                stage: str_field(input, "stage")?.to_string(),
                event: str_field(input, "event")?.to_string(),
                detail: input.get("detail").cloned().unwrap_or(Value::Null),
                run: run_of(input),
            };
            let unless = input
                .get("unless")
                .filter(|unless| !unless.is_null())
                .map(|unless| serde_json::from_value::<Unless>(unless.clone()))
                .transpose()
                .map_err(|e| StoreError::Invalid(format!("`unless` does not parse: {e}")))?;
            let dry_run = matches!(input.get("dry_run"), Some(Value::Bool(true)))
                || input
                    .get("dry_run")
                    .and_then(Value::as_str)
                    .is_some_and(|s| s.trim() == "true");
            let recorded = if dry_run {
                let current = match &unless {
                    Some(_) => store
                        .get(&scope, &def.collection, &event.item)
                        .await?
                        .map_or(Value::Null, |doc| doc.body),
                    None => Value::Null,
                };
                pipeline::preview_event(&def, &event, unless.as_ref(), &current)?
            } else {
                pipeline::record_event(store, &scope, &def, &event, unless.as_ref()).await?
            };
            let skipped = recorded.matched.is_some();
            Ok(json!({
                "recorded": !dry_run && !skipped,
                "dry_run": dry_run,
                "item": recorded.item,
                "version": recorded.version,
                "column": recorded.column,
                "label": recorded.label,
                "annotation": recorded.annotation,
                "warning": recorded.warning,
                "skipped": skipped.then_some("unless"),
                "matched": recorded.matched,
            }))
        }
        PipelineOp::Item => {
            let id = str_field(input, "pipeline")?;
            let item = str_field(input, "item")?;
            let (_, def) = pipeline::load_definition(store, &scope, id)
                .await?
                .ok_or(StoreError::NotFound)?;
            Ok(match store.get(&scope, &def.collection, item).await? {
                Some(doc) => json!({
                    "found": true,
                    "column": pipeline::position_of(&def, &doc.body),
                    "position": position::position(&def, &doc.body),
                    "activity": position::activity(&def, &doc.body, Utc::now()),
                    "version": doc.version,
                    "record": doc.body,
                }),
                None => json!({ "found": false, "pipeline": id, "item": item }),
            })
        }
        PipelineOp::Items => {
            let id = str_field(input, "pipeline")?;
            let (_, def) = pipeline::load_definition(store, &scope, id)
                .await?
                .ok_or(StoreError::NotFound)?;
            let mut query = Query::from_json(input)?;
            query.direction = crate::store::Direction::Desc;
            let page = store.list(&scope, &def.collection, &query).await?;
            let now = Utc::now();
            let items: Vec<Value> = page
                .documents
                .iter()
                .map(|d| {
                    json!({
                        "item": d.doc_id,
                        "column": pipeline::position_of(&def, &d.body),
                        "position": position::position(&def, &d.body),
                        "activity": position::activity(&def, &d.body, now),
                        "stage": d.body.get("stage"),
                        "updated_at": d.body.get("updated_at"),
                        "version": d.version,
                        "record": d.body,
                    })
                })
                .collect();
            Ok(json!({ "count": items.len(), "items": items, "next_cursor": page.next_cursor }))
        }
        PipelineOp::Runs => {
            defined(store, &scope, input).await?;
            let filter = run_filter(input)?;
            let detail = step_detail(input)?;
            let agent = agent_of(&scope);
            let now = Utc::now();
            let runs: Vec<RunView> = reading_hub(hub, op, caller, &scope)?
                .runs(&filter, agent)
                .await
                .map_err(hub_failed)?
                .into_iter()
                .filter(|run| joinable(run, agent))
                .map(|run| run_view(run, detail, now))
                .collect();
            Ok(json!({ "count": runs.len(), "runs": runs }))
        }
        PipelineOp::Facts => {
            let def = defined(store, &scope, input).await?;
            let facts = reading_hub(hub, op, caller, &scope)?
                .facts(agent_of(&scope), &def.sources, caller)
                .await
                .map_err(hub_failed)?;
            serde_json::to_value(facts).map_err(|e| StoreError::Backend(e.to_string()))
        }
        PipelineOp::Notice => {
            let id = str_field(input, "pipeline")?;
            defined(store, &scope, input).await?;
            match str_field(input, "op")? {
                "raise" => {
                    let raise: notices::Raise =
                        serde_json::from_value(input.clone()).map_err(|e| {
                            StoreError::Invalid(format!(
                                "a notice to raise needs `key`, `tone` and `title`: {e}"
                            ))
                        })?;
                    let notice = notices::raise(store, &scope, id, raise, actor.as_deref()).await?;
                    Ok(json!({ "raised": true, "pipeline": id, "notice": notice }))
                }
                "clear" => {
                    let key = str_field(input, "key")?;
                    let existed = notices::clear(store, &scope, id, key, actor.as_deref()).await?;
                    Ok(json!({ "cleared": true, "pipeline": id, "existed": existed }))
                }
                other => Err(StoreError::Invalid(format!(
                    "`op` is raise or clear, not `{other}`"
                ))),
            }
        }
    }
}

#[async_trait]
impl Tool for PipelineTool {
    fn name(&self) -> &str {
        self.op.name()
    }

    fn description(&self) -> &str {
        self.op.description()
    }

    fn input_schema(&self) -> Value {
        let scope = json!({"type": "string", "description": SCOPE_DESCRIPTION});
        let pipeline = json!({"type": "string", "description": "The pipeline's id: lowercase letters, digits, - or _"});
        match self.op {
            PipelineOp::Define => json!({
                "type": "object",
                "properties": {
                    "scope": scope,
                    "pipeline": pipeline,
                    "definition": {"type": "object", "description": "The definition (see the tool description); call with example=true for a complete one"},
                    "if_version": {"type": "integer", "description": "Redefine only if still at this version (from pipeline.get); 0 = create only"},
                    "example": {"type": "boolean", "description": "Return a worked example instead of defining anything"}
                },
                "required": []
            }),
            PipelineOp::List => json!({"type": "object", "properties": {"scope": scope}}),
            PipelineOp::Get | PipelineOp::Delete => json!({
                "type": "object",
                "properties": {"scope": scope, "pipeline": pipeline, "if_version": {"type": "integer"}},
                "required": ["pipeline"]
            }),
            PipelineOp::Record => json!({
                "type": "object",
                "properties": {
                    "scope": scope, "pipeline": pipeline,
                    "item": {"type": "string", "description": "The item's id, e.g. a ticket key"},
                    "stage": {"type": "string", "description": "Lowercase id of the stage that acted"},
                    "event": {"type": "string", "description": "Lowercase id of what happened, e.g. claimed, finished, failed"},
                    "detail": {"type": "object", "description": "Fields for the stage block — a verdict, a URL, a one-line detail; rules match on these"},
                    "run": {"type": "string", "description": "The workflow instance doing the recording; omitted from a tool step, the hub fills it in"},
                    "dry_run": {"type": ["boolean", "string"], "description": "Resolve the event but write nothing"},
                    "unless": {
                        "type": "object",
                        "description": "Record only when the item's history holds none of `events` (with `detail`, none whose detail matches)",
                        "properties": {
                            "events": {"type": "array", "items": {"type": "string"}, "description": "\"stage/event\" or \"stage/*\" keys"},
                            "detail": {"type": "object", "additionalProperties": {"type": "string"}}
                        },
                        "required": ["events"]
                    }
                },
                "required": ["pipeline", "item", "stage", "event"]
            }),
            PipelineOp::Item => json!({
                "type": "object",
                "properties": {"scope": scope, "pipeline": pipeline, "item": {"type": "string"}},
                "required": ["pipeline", "item"]
            }),
            PipelineOp::Items => json!({
                "type": "object",
                "properties": {
                    "scope": scope, "pipeline": pipeline,
                    "limit": {"type": "integer"}, "cursor": {"type": "string"}
                },
                "required": ["pipeline"]
            }),
            PipelineOp::Notice => json!({
                "type": "object",
                "properties": {
                    "scope": scope, "pipeline": pipeline,
                    "op": {"type": "string", "enum": ["raise", "clear"]},
                    "key": {"type": "string", "description": "Lowercase id naming the notice; raising it again replaces it"},
                    "tone": {"type": "string", "enum": ["active", "good", "warn", "healing", "bad", "neutral"]},
                    "title": {"type": "string", "description": "One line of up to 200 characters"},
                    "detail": {"type": "string", "description": "Up to 2,000 characters"},
                    "link": {"type": "string", "description": "An http(s) URL, or a path on the hub starting with /"},
                    "ttl_secs": {"type": "integer", "minimum": 1, "maximum": notices::TTL_MAX_SECS, "description": "Seconds until it lapses; absent, it stands until cleared"}
                },
                "required": ["pipeline", "op", "key"]
            }),
            PipelineOp::Runs => json!({
                "type": "object",
                "properties": {
                    "scope": scope, "pipeline": pipeline,
                    "runs": {"type": "array", "items": {"type": "string"}, "maxItems": RUNS_MAX, "description": "Only these workflow instance ids; an empty list matches none"},
                    "workflows": {"type": "array", "items": {"type": "string"}, "description": "Only runs of these workflows, members' copies included"},
                    "statuses": {"type": "array", "items": {"type": "string"}, "description": "Only runs in these statuses: queued, running, paused, completed, failed, exited, cancelled"},
                    "since": {"type": "string", "description": "Only runs updated at or after this RFC 3339 time"},
                    "steps": {"type": "string", "enum": ["none", "coding", "all"], "description": "Which steps each run carries; default none"},
                    "limit": {"type": "integer", "minimum": 1, "maximum": RUNS_MAX, "description": "At most this many runs; default 200"}
                },
                "required": ["pipeline"]
            }),
            PipelineOp::Facts => json!({
                "type": "object",
                "properties": {"scope": scope, "pipeline": pipeline},
                "required": ["pipeline"]
            }),
        }
    }

    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = Instant::now();
        let caller = caller_from_context(&self.is_admin);
        match perform(
            self.store.as_ref(),
            self.hub.as_deref(),
            &caller,
            self.op,
            &input,
        )
        .await
        {
            Ok(out) => Ok(ToolResult::success(out, start.elapsed().as_millis() as u64)),
            Err(e) => Ok(failure(start, e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::facts::Facts;
    use crate::pipeline::fixtures::{run, T0};
    use crate::pipeline::runs::Run;
    use crate::store::MemoryDocumentStore;
    use workforce_agent::runner::{CURRENT_AGENT_ID, CURRENT_USER_ID};
    use workforce_orchestrator::workflow::instance::{StepProvenance, StepRecord};

    type FactsAsked = (Option<String>, Vec<String>, Option<String>);

    /// A hub holding one of example's runs and one of quill's, recording what
    /// each read asked for.
    #[derive(Default)]
    struct RecordingHub {
        filters: parking_lot::Mutex<Vec<(InstanceFilter, Option<String>)>>,
        facts: parking_lot::Mutex<Vec<FactsAsked>>,
    }

    #[async_trait]
    impl HubReads for RecordingHub {
        async fn runs(&self, filter: &InstanceFilter, agent: Option<&str>) -> Result<Vec<Run>> {
            self.filters
                .lock()
                .push((filter.clone(), agent.map(str::to_string)));
            let coding = StepRecord {
                step_id: "implement".into(),
                outcome: None,
                agent_response: "transcript".into(),
                outputs: None,
                report: None,
                started_at: T0.parse().unwrap(),
                completed_at: T0.parse().unwrap(),
                rounds: 1,
                cost_usd: 0.0,
                budget_capped: false,
                failed: false,
                correction: None,
                provenance: StepProvenance {
                    engine: Some("claude-code".into()),
                    ..Default::default()
                },
            };
            Ok(vec![
                Run {
                    status: "running".into(),
                    lease_expires_at: Some(Utc::now() + chrono::Duration::minutes(5)),
                    steps: vec![
                        coding.clone(),
                        StepRecord {
                            step_id: "pick".into(),
                            provenance: StepProvenance {
                                engine: Some("wasm".into()),
                                ..Default::default()
                            },
                            ..coding
                        },
                    ],
                    ..run("mine", T0, None, Vec::new())
                },
                Run {
                    agent_id: Some("quill".into()),
                    ..run("quills", T0, None, Vec::new())
                },
            ])
        }

        async fn facts(
            &self,
            agent: Option<&str>,
            sources: &[String],
            caller: &Caller,
        ) -> Result<Facts> {
            self.facts.lock().push((
                agent.map(str::to_string),
                sources.to_vec(),
                caller.identity.clone(),
            ));
            Ok(Facts {
                build: json!({"version": "test"}),
                workers: Vec::new(),
                skills: Vec::new(),
                workflows: Vec::new(),
            })
        }
    }

    fn tools() -> Vec<PipelineTool> {
        let store: Arc<dyn DocumentStore> = Arc::new(MemoryDocumentStore::new());
        let is_admin: IsAdminFn = Arc::new(|| false);
        PipelineOp::DOCUMENTS
            .iter()
            .map(|op| PipelineTool::new(Arc::clone(&store), Arc::clone(&is_admin), *op))
            .collect()
    }

    fn tool(tools: &[PipelineTool], op: PipelineOp) -> &PipelineTool {
        tools.iter().find(|t| t.op == op).unwrap()
    }

    async fn as_agent<F: std::future::Future>(fut: F) -> F::Output {
        CURRENT_USER_ID
            .scope(
                "alice".into(),
                CURRENT_AGENT_ID.scope("example".into(), fut),
            )
            .await
    }

    #[tokio::test]
    async fn define_record_list_round_trip_through_the_tools() {
        let tools = tools();
        as_agent(async {
            let ex = tool(&tools, PipelineOp::Define)
                .execute(json!({"example": true}))
                .await
                .unwrap();
            assert!(ex.success);
            let definition = ex.output["example"]["definition"].clone();

            let r = tool(&tools, PipelineOp::Define)
                .execute(json!({"pipeline": "onboarding", "definition": definition, "if_version": 0}))
                .await
                .unwrap();
            assert!(r.success, "{:?}", r.error);
            assert_eq!(r.output["version"], 1);
            assert_eq!(r.output["board"], "/pipelines/agent:example/onboarding");

            let r = tool(&tools, PipelineOp::Define)
                .execute(json!({"pipeline": "onboarding", "definition": {"name": "x", "collection": "c", "lanes": [], "events": {}}}))
                .await
                .unwrap();
            assert!(!r.success);
            assert!(r.error.as_deref().unwrap().contains("`lanes` needs at least one lane"), "{:?}", r.error);

            let r = tool(&tools, PipelineOp::Record)
                .execute(json!({"pipeline": "onboarding", "item": "jane", "stage": "accounts", "event": "started", "detail": {"name": "Jane"}, "_instance_id": "inst-9"}))
                .await
                .unwrap();
            assert!(r.success, "{:?}", r.error);
            assert_eq!(r.output["column"], "accounts");
            assert_eq!(r.output["label"], "Creating accounts");

            let r = tool(&tools, PipelineOp::Record)
                .execute(json!({"pipeline": "onboarding", "item": "jane", "stage": "accounts", "event": "typo"}))
                .await
                .unwrap();
            assert!(r.success);
            assert!(r.output["warning"].as_str().unwrap().contains("no rule for `accounts/typo`"));

            let r = tool(&tools, PipelineOp::Record)
                .execute(json!({"pipeline": "missing", "item": "jane", "stage": "a", "event": "b"}))
                .await
                .unwrap();
            assert!(r.error.as_deref().unwrap().contains("define it first"));

            let r = tool(&tools, PipelineOp::Record)
                .execute(json!({"pipeline": "onboarding", "item": "ghost", "stage": "access", "event": "failed", "dry_run": "true"}))
                .await
                .unwrap();
            assert!(r.success, "{:?}", r.error);
            assert_eq!(r.output["recorded"], false);
            assert_eq!(r.output["column"], "blocked");
            let r = tool(&tools, PipelineOp::Item)
                .execute(json!({"pipeline": "onboarding", "item": "ghost"}))
                .await
                .unwrap();
            assert_eq!(r.output["found"], false, "a dry run writes nothing");

            let r = tool(&tools, PipelineOp::Item)
                .execute(json!({"pipeline": "onboarding", "item": "jane"}))
                .await
                .unwrap();
            assert_eq!(r.output["found"], true);
            assert_eq!(r.output["record"]["stages"]["accounts"]["run"], "inst-9");
            assert_eq!(r.output["column"], "accounts", "an unknown event does not move the item");

            let r = tool(&tools, PipelineOp::List).execute(json!({})).await.unwrap();
            assert_eq!(r.output["count"], 1);
            assert_eq!(r.output["pipelines"][0]["counts"]["items"], 1);
            assert_eq!(r.output["pipelines"][0]["counts"]["columns"]["accounts"], 1);

            let r = tool(&tools, PipelineOp::Items)
                .execute(json!({"pipeline": "onboarding"}))
                .await
                .unwrap();
            assert_eq!(r.output["items"][0]["item"], "jane");

            let r = tool(&tools, PipelineOp::Define)
                .execute(json!({"scope": "global", "pipeline": "x", "definition": ex.output["example"]["definition"]}))
                .await
                .unwrap();
            assert!(!r.success, "a member may not define a global pipeline");
        })
        .await;
    }

    async fn onboarding(store: &dyn DocumentStore) {
        let definition: PipelineDefinition =
            serde_json::from_value(pipeline::example()["definition"].clone()).unwrap();
        pipeline::define(
            store,
            &Scope::Agent("example".into()),
            "onboarding",
            &definition,
            None,
            None,
        )
        .await
        .unwrap();
    }

    fn member() -> Caller {
        Caller {
            identity: Some("alice".into()),
            agent_id: Some("example".into()),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn runs_join_only_the_pipeline_agents_runs_with_the_steps_asked_for() {
        let store = MemoryDocumentStore::new();
        onboarding(&store).await;
        let hub = RecordingHub::default();
        let out = perform(
            &store,
            Some(&hub),
            &member(),
            PipelineOp::Runs,
            &json!({
                "pipeline": "onboarding",
                "workflows": ["order-intake"],
                "statuses": ["running"],
                "since": "2026-09-12T09:00:00+01:00",
                "steps": "coding",
                "limit": 500
            }),
        )
        .await
        .unwrap();
        assert_eq!(out["count"], 1);
        let mine = &out["runs"][0];
        assert_eq!(mine["run"], "mine");
        assert_eq!(mine["lease"]["live"], true);
        assert_eq!(mine["steps"].as_array().unwrap().len(), 1);
        assert_eq!(mine["steps"][0]["engine"], "claude-code");
        assert!(mine["steps"][0].get("agent_response").is_none());
        assert_eq!(
            hub.filters.lock().as_slice(),
            [(
                InstanceFilter {
                    ids: None,
                    workflows: Some(vec!["order-intake".into()]),
                    statuses: Some(vec!["running".into()]),
                    since: Some("2026-09-12T08:00:00Z".parse().unwrap()),
                    runs_as: None,
                    limit: RUNS_MAX,
                },
                Some("example".to_string())
            )]
        );

        let named = perform(
            &store,
            Some(&hub),
            &member(),
            PipelineOp::Runs,
            &json!({"pipeline": "onboarding", "runs": [], "limit": 0}),
        )
        .await
        .unwrap();
        assert_eq!(named["runs"][0]["steps"], json!([]));
        let (asked, _) = hub.filters.lock().last().cloned().unwrap();
        assert_eq!((asked.ids, asked.limit), (Some(Vec::new()), 1));
    }

    #[tokio::test]
    async fn runs_refuse_a_malformed_request_naming_the_field() {
        let store = MemoryDocumentStore::new();
        onboarding(&store).await;
        let hub = RecordingHub::default();
        let too_many: Vec<String> = (0..=RUNS_MAX).map(|i| format!("run-{i}")).collect();
        for (input, expected) in [
            (
                json!({"pipeline": "onboarding", "runs": too_many}),
                "at most 200",
            ),
            (
                json!({"pipeline": "onboarding", "since": "yesterday"}),
                "RFC 3339",
            ),
            (
                json!({"pipeline": "onboarding", "steps": "heavy"}),
                "none, coding or all",
            ),
            (
                json!({"pipeline": "onboarding", "workflows": "order-intake"}),
                "list of strings",
            ),
            (
                json!({"pipeline": "onboarding", "statuses": [1]}),
                "list of strings",
            ),
            (
                json!({"pipeline": "onboarding", "limit": "ten"}),
                "must be an integer",
            ),
            (json!({}), "`pipeline` is required"),
        ] {
            let err = perform(&store, Some(&hub), &member(), PipelineOp::Runs, &input)
                .await
                .unwrap_err();
            assert!(err.to_string().contains(expected), "{input}: {err}");
        }
        let missing = perform(
            &store,
            Some(&hub),
            &member(),
            PipelineOp::Runs,
            &json!({"pipeline": "nowhere"}),
        )
        .await
        .unwrap_err();
        assert!(matches!(missing, StoreError::NotFound));
        assert!(
            hub.filters.lock().is_empty(),
            "a refused request reads nothing"
        );
    }

    #[tokio::test]
    async fn facts_follow_the_scope_rule_of_every_pipeline_op() {
        let store = MemoryDocumentStore::new();
        onboarding(&store).await;
        let hub = RecordingHub::default();
        let out = perform(
            &store,
            Some(&hub),
            &member(),
            PipelineOp::Facts,
            &json!({"pipeline": "onboarding"}),
        )
        .await
        .unwrap();
        assert_eq!(out["build"]["version"], "test");
        assert_eq!(
            hub.facts.lock().as_slice(),
            [(
                Some("example".to_string()),
                vec!["workflow:new-starter".to_string()],
                Some("alice".to_string())
            )]
        );

        let another_agent = Caller {
            agent_id: Some("quill".into()),
            ..member()
        };
        for op in PipelineOp::HUB {
            let refused = perform(
                &store,
                Some(&hub),
                &another_agent,
                op,
                &json!({"scope": "agent:example", "pipeline": "onboarding"}),
            )
            .await
            .unwrap_err();
            assert!(
                matches!(refused, StoreError::Denied(_)),
                "{op:?}: {refused}"
            );
        }
        let admin = Caller {
            admin: true,
            ..another_agent
        };
        assert!(perform(
            &store,
            Some(&hub),
            &admin,
            PipelineOp::Facts,
            &json!({"scope": "agent:example", "pipeline": "onboarding"}),
        )
        .await
        .is_ok());
        assert_eq!(hub.facts.lock().len(), 2);
    }

    #[tokio::test]
    async fn the_hub_reads_on_a_global_or_user_pipeline_are_for_admins_and_system_runs() {
        let store = MemoryDocumentStore::new();
        let definition: PipelineDefinition =
            serde_json::from_value(pipeline::example()["definition"].clone()).unwrap();
        for scope in [Scope::Global, Scope::User("alice".into())] {
            pipeline::define(&store, &scope, "onboarding", &definition, None, None)
                .await
                .unwrap();
        }
        let hub = RecordingHub::default();
        for scope in ["global", "user"] {
            for op in PipelineOp::HUB {
                let refused = perform(
                    &store,
                    Some(&hub),
                    &member(),
                    op,
                    &json!({"scope": scope, "pipeline": "onboarding"}),
                )
                .await
                .unwrap_err();
                assert!(
                    matches!(&refused, StoreError::Denied(message) if message.contains("only admins and system runs")),
                    "{scope} {op:?}: {refused}"
                );
            }
        }
        assert!(hub.filters.lock().is_empty() && hub.facts.lock().is_empty());

        let admin = Caller {
            admin: true,
            ..member()
        };
        let system = Caller {
            system: true,
            ..member()
        };
        for (caller, scope) in [(&admin, "global"), (&system, "user")] {
            let out = perform(
                &store,
                Some(&hub),
                caller,
                PipelineOp::Runs,
                &json!({"scope": scope, "pipeline": "onboarding"}),
            )
            .await
            .unwrap();
            assert_eq!(out["count"], 2, "{scope} joins every agent's runs");
        }
        assert!(hub.filters.lock().iter().all(|(_, agent)| agent.is_none()));
    }

    #[tokio::test]
    async fn the_hub_reads_are_named_and_registered_only_with_a_hub() {
        let store: Arc<dyn DocumentStore> = Arc::new(MemoryDocumentStore::new());
        let registry = ToolRegistry::new();
        register(&registry, Arc::clone(&store), Arc::new(|| false));
        assert!(registry.get("pipeline.record").is_some());
        assert!(registry.get("pipeline.runs").is_none());
        assert!(registry.get("pipeline.facts").is_none());
        register_hub_reads(
            &registry,
            Arc::clone(&store),
            Arc::new(|| false),
            Arc::new(RecordingHub::default()),
        );
        assert!(registry.get("pipeline.runs").is_some());
        assert!(registry.get("pipeline.facts").is_some());
        assert_eq!(PipelineOp::parse("facts"), Some(PipelineOp::Facts));
        assert_eq!(PipelineOp::parse("runs"), Some(PipelineOp::Runs));

        onboarding(store.as_ref()).await;
        let unwired = perform(
            store.as_ref(),
            None,
            &member(),
            PipelineOp::Runs,
            &json!({"pipeline": "onboarding"}),
        )
        .await
        .unwrap_err();
        assert!(
            unwired.to_string().contains("does not run here"),
            "{unwired}"
        );
    }

    fn annotated_orders() -> PipelineDefinition {
        let mut def = crate::pipeline::fixtures::orders();
        let started = &mut def.events.get_mut("verify/started").unwrap()[0];
        started.claim = true;
        started.lease_field = Some("lease_minutes".into());
        def.events.insert(
            "review/overlap-judged".into(),
            vec![pipeline::Rule {
                annotation: true,
                tone: Some(pipeline::Tone::Warn),
                label_field: Some("headline".into()),
                ..Default::default()
            }],
        );
        def
    }

    async fn call(
        store: &dyn DocumentStore,
        op: PipelineOp,
        input: Value,
    ) -> Result<Value, StoreError> {
        perform(store, None, &member(), op, &input).await
    }

    #[tokio::test]
    async fn record_notes_annotations_honours_unless_and_items_carry_position_and_activity() {
        let store = MemoryDocumentStore::new();
        pipeline::define(
            &store,
            &Scope::Agent("example".into()),
            "orders",
            &annotated_orders(),
            None,
            None,
        )
        .await
        .unwrap();
        let record = |event: &str, detail: Value, extra: Value| {
            let (stage, name) = event.split_once('/').unwrap();
            let mut input = json!({"pipeline": "orders", "item": "ORD-1", "stage": stage, "event": name, "detail": detail});
            input
                .as_object_mut()
                .unwrap()
                .extend(extra.as_object().cloned().unwrap_or_default());
            call(&store, PipelineOp::Record, input)
        };

        let started = record(
            "verify/started",
            json!({"rung": "1h", "lease_minutes": 150}),
            json!({}),
        )
        .await
        .unwrap();
        assert_eq!(
            (
                &started["recorded"],
                &started["annotation"],
                &started["column"]
            ),
            (&json!(true), &json!(false), &json!("verifying"))
        );
        let headline = "#1698 and #1702 change the same code";
        let judged = record(
            "review/overlap-judged",
            json!({"relation": "conflicting", "headline": headline}),
            json!({}),
        )
        .await
        .unwrap();
        assert_eq!(
            (
                &judged["annotation"],
                &judged["column"],
                &judged["label"],
                &judged["warning"]
            ),
            (&json!(true), &Value::Null, &json!(headline), &Value::Null)
        );

        let item = call(
            &store,
            PipelineOp::Item,
            json!({"pipeline": "orders", "item": "ORD-1"}),
        )
        .await
        .unwrap();
        assert_eq!(
            item["record"]["stage"], "verify",
            "an annotation leaves the current stage"
        );
        assert_eq!(item["position"]["column"], "verifying");
        assert_eq!(
            item["position"]["annotation"],
            json!({"tone": "warn", "label": headline, "at": item["record"]["history"][1]["at"]})
        );
        let placed_at = item["position"]["at"].as_str().unwrap();
        let since: DateTime<Utc> = placed_at.parse().unwrap();
        assert_eq!(
            item["activity"],
            json!({"kind": "working", "since": placed_at, "until": since + chrono::Duration::minutes(150)})
        );

        for dry_run in [false, true] {
            let skipped = record(
                "review/overlap-judged",
                json!({"relation": "conflicting", "headline": "again"}),
                json!({"unless": {"events": ["review/*"], "detail": {"relation": "conflicting"}}, "dry_run": dry_run}),
            )
            .await
            .unwrap();
            assert_eq!(
                (
                    &skipped["recorded"],
                    &skipped["skipped"],
                    &skipped["matched"]["event"]
                ),
                (&json!(false), &json!("unless"), &json!("overlap-judged")),
                "dry_run {dry_run}"
            );
        }
        let rehearsed = record(
            "review/overlap-judged",
            json!({"headline": "again"}),
            json!({"unless": {"events": ["merge/*"]}, "dry_run": "true"}),
        )
        .await
        .unwrap();
        assert_eq!(
            (
                &rehearsed["recorded"],
                &rehearsed["skipped"],
                &rehearsed["annotation"]
            ),
            (&json!(false), &Value::Null, &json!(true))
        );
        let refused = record(
            "review/overlap-judged",
            json!({}),
            json!({"unless": {"events": "review/*"}}),
        )
        .await
        .unwrap_err();
        assert!(
            refused.to_string().contains("`unless` does not parse"),
            "{refused}"
        );

        let items = call(&store, PipelineOp::Items, json!({"pipeline": "orders"}))
            .await
            .unwrap();
        assert_eq!(items["items"][0]["position"]["column"], "verifying");
        assert_eq!(items["items"][0]["activity"]["kind"], "working");
        assert_eq!(
            items["items"][0]["record"]["history"]
                .as_array()
                .unwrap()
                .len(),
            2,
            "neither a skipped nor a rehearsed event was written"
        );
    }

    #[tokio::test]
    async fn notices_are_raised_and_cleared_under_the_scope_rule() {
        let store = MemoryDocumentStore::new();
        onboarding(&store).await;
        let scope = Scope::Agent("example".into());
        let raised = call(
            &store,
            PipelineOp::Notice,
            json!({"pipeline": "onboarding", "op": "raise", "key": "hlt-2", "tone": "warn", "title": "the worker speaks protocol v2", "link": "/pipelines/agent:example/onboarding", "ttl_secs": 3600, "_instance_id": "run-1"}),
        )
        .await
        .unwrap();
        assert_eq!(raised["raised"], true);
        assert_eq!(raised["notice"]["tone"], "warn");
        assert!(raised["notice"]["expires_at"].is_string());
        assert_eq!(
            notices::active(&store, &scope, "onboarding")
                .await
                .unwrap()
                .len(),
            1
        );

        for (input, expected) in [
            (
                json!({"pipeline": "onboarding", "op": "raise", "key": "hlt-2", "title": "t"}),
                "needs `key`, `tone` and `title`",
            ),
            (
                json!({"pipeline": "onboarding", "op": "raise", "key": "hlt-2", "tone": "loud", "title": "t"}),
                "needs `key`, `tone` and `title`",
            ),
            (
                json!({"pipeline": "onboarding", "op": "shout", "key": "hlt-2"}),
                "raise or clear",
            ),
            (
                json!({"pipeline": "onboarding", "key": "hlt-2"}),
                "`op` is required",
            ),
        ] {
            let err = call(&store, PipelineOp::Notice, input.clone())
                .await
                .unwrap_err();
            assert!(err.to_string().contains(expected), "{input}: {err}");
        }
        assert!(matches!(
            call(
                &store,
                PipelineOp::Notice,
                json!({"pipeline": "nowhere", "op": "clear", "key": "hlt-2"})
            )
            .await
            .unwrap_err(),
            StoreError::NotFound
        ));
        let another_agent = Caller {
            agent_id: Some("quill".into()),
            ..member()
        };
        let refused = perform(
            &store,
            None,
            &another_agent,
            PipelineOp::Notice,
            &json!({"scope": "agent:example", "pipeline": "onboarding", "op": "clear", "key": "hlt-2"}),
        )
        .await
        .unwrap_err();
        assert!(matches!(refused, StoreError::Denied(_)), "{refused}");

        let cleared = call(
            &store,
            PipelineOp::Notice,
            json!({"pipeline": "onboarding", "op": "clear", "key": "hlt-2"}),
        )
        .await
        .unwrap();
        assert_eq!(
            (&cleared["cleared"], &cleared["existed"]),
            (&json!(true), &json!(true))
        );
        assert!(notices::active(&store, &scope, "onboarding")
            .await
            .unwrap()
            .is_empty());
    }
}
