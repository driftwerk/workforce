//! Records and runs as the pipeline item views' tests build them.

use std::collections::HashMap;

use chrono::Utc;
use serde_json::{json, Value};
use workforce_common::persistence::WorkflowInstanceRow;
use workforce_orchestrator::workflow::instance::StepRecord;

use super::runs::Run;
use super::{apply_event, EventInput, PipelineDefinition};
use crate::hub_core::pipelines::Scope;
use crate::store::Document;

pub const T0: &str = "2026-09-12T08:00:00Z";
pub const T1: &str = "2026-09-12T09:00:00Z";
pub const T2: &str = "2026-09-12T10:00:00Z";
pub const T3: &str = "2026-09-12T11:00:00Z";

pub fn doc(id: &str, body: Value) -> Document {
    let now = Utc::now();
    Document {
        scope: Scope::Agent("example".into()),
        collection: "pipeline".into(),
        doc_id: id.into(),
        body,
        version: 1,
        created_by: None,
        created_at: now,
        updated_at: now,
        expires_at: None,
        lease_owner: None,
        lease_expires_at: None,
    }
}

/// The example pipeline at revision 9, without its guide.
pub fn orders() -> PipelineDefinition {
    serde_json::from_str(ORDERS_R9).unwrap()
}

pub const ORDERS_R9: &str = include_str!("testdata/orders-r9.json");

/// The example pipeline at revision 11: annotations, merges, and windows on
/// the columns where an item is finished.
pub fn orders_r11() -> PipelineDefinition {
    serde_json::from_str(ORDERS_R11).unwrap()
}

pub const ORDERS_R11: &str = include_str!("testdata/orders-r11-windows.json");

/// An item record as `pipeline.record` builds it, from `(at, stage, event, run)`.
pub fn record(item: &str, events: &[(&str, &str, &str, Option<&str>)]) -> Document {
    let detailed: Vec<_> = events
        .iter()
        .map(|&(at, stage, event, run)| (at, stage, event, run, Value::Null))
        .collect();
    record_with(item, &detailed)
}

/// As [`record`], each event with its `detail`.
pub fn record_with(item: &str, events: &[(&str, &str, &str, Option<&str>, Value)]) -> Document {
    let body = events
        .iter()
        .fold(json!({}), |body, (at, stage, event, run, detail)| {
            apply_event(
                body,
                &EventInput {
                    item: item.into(),
                    stage: (*stage).into(),
                    event: (*event).into(),
                    detail: detail.clone(),
                    run: run.map(str::to_string),
                },
                at,
                None,
            )
        });
    doc(item, body)
}

pub fn run(id: &str, created_at: &str, booked: Option<&str>, steps: Vec<StepRecord>) -> Run {
    Run {
        id: id.into(),
        workflow_id: "order-intake".into(),
        agent_id: Some("example".into()),
        status: "completed".into(),
        created_at: created_at.into(),
        updated_at: created_at.into(),
        current_step: None,
        lease_expires_at: None,
        failure_reason: None,
        booked: booked.map(str::to_string),
        steps,
    }
}

pub fn row(
    id: &str,
    workflow_id: &str,
    agent_id: Option<&str>,
    steps: Vec<StepRecord>,
) -> WorkflowInstanceRow {
    WorkflowInstanceRow {
        id: id.into(),
        workflow_id: workflow_id.into(),
        status: "completed".into(),
        current_step_id: None,
        step_history: serde_json::to_string(&steps).unwrap(),
        variables: "{}".into(),
        trigger_json: "{}".into(),
        agent_id: agent_id.map(str::to_string),
        created_at: T0.into(),
        updated_at: T0.into(),
        parent_instance_id: None,
        channel_id: None,
        thread_id: None,
        owner_instance: None,
        lease_expires_at: None,
        version: 0,
    }
}

pub fn joined(runs: Vec<Run>) -> HashMap<String, Run> {
    runs.into_iter().map(|run| (run.id.clone(), run)).collect()
}

/// ORD-2797 as a live board recorded it: sent back once, re-claimed
/// twice, held for a human merge, deployed, then waiting ten hours for its
/// first verification rung. Details keep only what the rules read.
pub fn ord_2797() -> Document {
    record_with(
        "ORD-2797",
        &[
            (
                "2026-09-14T10:15:03.343111Z",
                "implement",
                "claimed",
                Some("dc86d601"),
                json!({"repo": "acme/app"}),
            ),
            (
                "2026-09-14T11:02:05.543565Z",
                "implement",
                "pr-opened",
                Some("dc86d601"),
                json!({"pr_url": "https://github.com/acme/app/pull/1695"}),
            ),
            (
                "2026-09-14T11:03:02.507453Z",
                "review",
                "claimed",
                Some("4651992f"),
                json!({"reason": "unreviewed"}),
            ),
            (
                "2026-09-14T12:19:48.580991Z",
                "review",
                "claimed",
                Some("cabe2a01"),
                json!({"reason": "unreviewed"}),
            ),
            (
                "2026-09-14T12:43:56.358266Z",
                "review",
                "sent-back",
                Some("cabe2a01"),
                json!({"send_back": 1}),
            ),
            (
                "2026-09-14T13:00:03.550479Z",
                "implement",
                "claimed",
                Some("19d755d8"),
                json!({"repo": "acme/app"}),
            ),
            (
                "2026-09-14T16:30:03.207001Z",
                "implement",
                "claimed",
                Some("2e083958"),
                json!({"repo": "acme/app"}),
            ),
            (
                "2026-09-14T17:19:41.035731Z",
                "implement",
                "pr-opened",
                Some("2e083958"),
                json!({"pr_url": "https://github.com/acme/app/pull/1697"}),
            ),
            (
                "2026-09-14T17:23:05.179169Z",
                "review",
                "claimed",
                Some("343b8454"),
                json!({"reason": "unreviewed"}),
            ),
            (
                "2026-09-14T17:48:24.022657Z",
                "merge",
                "gate",
                Some("343b8454"),
                json!({"outcome": "human", "mode": "dry-run"}),
            ),
            (
                "2026-09-14T17:48:25.575326Z",
                "review",
                "follow-ups",
                Some("343b8454"),
                json!({"existing": []}),
            ),
            (
                "2026-09-14T17:48:26.934771Z",
                "review",
                "finished",
                Some("343b8454"),
                json!({"verdict": "stage-reviewed"}),
            ),
            (
                "2026-09-14T18:26:03.983203Z",
                "deploy",
                "claimed",
                Some("7dfa1eb7"),
                json!({"merged_how": "merged"}),
            ),
            (
                "2026-09-14T18:28:35.428027Z",
                "deploy",
                "finished",
                Some("7dfa1eb7"),
                json!({"label": "stage-deployed"}),
            ),
            (
                "2026-09-15T04:48:00.771949Z",
                "verify",
                "started",
                Some("0e3be51e"),
                json!({"lease_minutes": 150, "rung": "1h"}),
            ),
            (
                "2026-09-15T04:53:20.286220Z",
                "verify",
                "inconclusive",
                Some("0e3be51e"),
                json!({"final": "false", "rung": "1h", "state": "inconclusive"}),
            ),
        ],
    )
}
