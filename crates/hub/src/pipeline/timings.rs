//! How long an item has taken so far: each stretch it spent in one column,
//! the time the workflow runs its record names spent working it, and the
//! rest as waiting, from where its clock starts to where it stops.
//!
//! Time is not additive across items the way cost is, so a run several items
//! name counts as work on each of them.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;

use super::runs::{joinable, sightings, text, Run};
use super::{grouped, history, placements, PipelineDefinition, HISTORY_MAX};
use crate::store::Document;

/// How far a recorded time may fall behind an earlier one, or run time fall
/// outside the item's span, before the figures say the clock was adjusted.
const SKEW_TOLERANCE_MS: u64 = 60_000;

const RUNNING: &str = "running";

#[derive(Debug, Clone, Serialize)]
pub struct ItemTimings {
    pub item: String,
    pub as_of: DateTime<Utc>,
    pub origin: OriginTime,
    /// When the pipeline first recorded the item.
    pub started_at: DateTime<Utc>,
    /// When the item reached a column where its clock stops.
    pub finished_at: Option<DateTime<Utc>>,
    pub total: Totals,
    pub milestones: Vec<Milestone>,
    pub visits: Vec<Visit>,
    pub columns: Vec<ColumnTimes>,
    pub runs: Vec<RunTimes>,
    pub coverage: TimingCoverage,
}

/// Where the item's clock starts.
#[derive(Debug, Clone, Serialize)]
pub struct OriginTime {
    pub at: DateTime<Utc>,
    /// What the definition calls the moment, when it declares one.
    pub label: Option<String>,
    /// A source recorded the moment; otherwise the clock starts when the
    /// pipeline first recorded the item.
    pub recorded: bool,
}

/// Durations in milliseconds. Work and waiting are `None` when the hub keeps
/// no workflow runs to read work from.
#[derive(Debug, Clone, Serialize)]
pub struct Totals {
    /// From the origin to where the clock stops, or to `as_of` while it runs.
    pub elapsed_ms: u64,
    /// As `elapsed_ms`, from when the pipeline first recorded the item.
    pub in_pipeline_ms: u64,
    pub work_ms: Option<u64>,
    pub waiting_ms: Option<u64>,
    /// The clock is still running.
    pub open: bool,
    /// A run the item's record names is in flight.
    pub working: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Milestone {
    pub column: String,
    pub label: String,
    pub reached_at: Option<DateTime<Utc>>,
    pub from_origin_ms: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VisitKind {
    /// From the recorded origin to when the pipeline first recorded the item.
    BeforePipeline,
    /// Before the earliest event a capped history still holds.
    Trimmed,
    /// Recorded, but not yet placed in a column by any event.
    Unplaced,
    /// A stretch in one column.
    Column,
}

/// One stretch of the item's time.
#[derive(Debug, Clone, Serialize)]
pub struct Visit {
    pub kind: VisitKind,
    pub column: Option<String>,
    pub label: Option<String>,
    pub entered_at: DateTime<Utc>,
    /// When the next stretch began; `None` for the stretch still running.
    pub left_at: Option<DateTime<Utc>>,
    pub elapsed_ms: u64,
    pub work_ms: Option<u64>,
    pub waiting_ms: Option<u64>,
}

/// A column's visits added up.
#[derive(Debug, Clone, Serialize)]
pub struct ColumnTimes {
    pub column: String,
    pub label: String,
    pub visits: usize,
    pub elapsed_ms: u64,
    pub work_ms: Option<u64>,
    pub waiting_ms: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TimedBy {
    /// The intervals of its steps.
    Steps,
    /// Its whole lifetime: it has no step history to read.
    Lifetime,
}

#[derive(Debug, Clone, Serialize)]
pub struct RunTimes {
    pub run: String,
    pub workflow: String,
    pub status: String,
    pub started_at: Option<DateTime<Utc>>,
    /// `None` while the run is running.
    pub ended_at: Option<DateTime<Utc>>,
    /// Its work within the item's span.
    pub work_ms: u64,
    pub timed_by: TimedBy,
}

/// How much of what the item's record names the figures cover.
#[derive(Debug, Clone, Serialize)]
pub struct TimingCoverage {
    pub runs_named: usize,
    pub runs_timed: usize,
    /// Runs with no workflow instance this pipeline may join; their time
    /// shows as waiting.
    pub runs_unmatched: usize,
    pub runs_by_lifetime: usize,
    pub work_unavailable: bool,
    pub origin_recorded: bool,
    /// The recorded origin is later than when the pipeline first recorded the
    /// item, so the clock starts at the latter.
    pub origin_after_start: bool,
    pub history_truncated: bool,
    /// Events recorded out of order, or run time outside the item's span,
    /// were adjusted.
    pub clock_adjusted: bool,
}

/// A non-empty stretch of time in whole milliseconds since the epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Span {
    from: i64,
    to: i64,
}

impl Span {
    fn new(from: DateTime<Utc>, to: DateTime<Utc>) -> Option<Self> {
        let span = Self {
            from: from.timestamp_millis(),
            to: to.timestamp_millis(),
        };
        (span.to > span.from).then_some(span)
    }

    fn ms(self) -> u64 {
        self.to.abs_diff(self.from)
    }
}

fn ms_between(from: DateTime<Utc>, to: DateTime<Utc>) -> u64 {
    Span::new(from, to).map_or(0, Span::ms)
}

pub(crate) fn parse_at(at: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(at)
        .ok()
        .map(|at| at.with_timezone(&Utc))
}

/// `spans` merged into disjoint spans, earliest first.
fn union(mut spans: Vec<Span>) -> Vec<Span> {
    spans.sort_unstable();
    spans
        .into_iter()
        .fold(Vec::new(), |mut merged: Vec<Span>, span| {
            match merged.last_mut() {
                Some(last) if span.from <= last.to => last.to = last.to.max(span.to),
                _ => merged.push(span),
            }
            merged
        })
}

/// How much of `window` the disjoint `spans` cover.
fn overlap(spans: &[Span], window: Span) -> u64 {
    spans
        .iter()
        .map(|span| {
            (span.to.min(window.to) - span.from.max(window.from))
                .max(0)
                .unsigned_abs()
        })
        .sum()
}

fn work_between(spans: &[Span], from: DateTime<Utc>, to: DateTime<Utc>) -> u64 {
    Span::new(from, to).map_or(0, |window| overlap(spans, window))
}

/// The time a run spent working, as disjoint spans: its steps, plus the step
/// in flight while it runs; its lifetime when it has no steps to read.
fn run_spans(run: &Run, as_of: DateTime<Utc>) -> (Vec<Span>, TimedBy) {
    let created = parse_at(&run.created_at);
    let running = run.status == RUNNING;
    if run.steps.is_empty() {
        let ended = if running {
            Some(as_of)
        } else {
            parse_at(&run.updated_at)
        };
        let lifetime = created
            .zip(ended)
            .and_then(|(from, to)| Span::new(from, to));
        return (lifetime.into_iter().collect(), TimedBy::Lifetime);
    }
    let in_flight = running
        .then(|| {
            let last_completed = run.steps.iter().map(|step| step.completed_at).max();
            Span::new(created.into_iter().chain(last_completed).max()?, as_of)
        })
        .flatten();
    let spans = run
        .steps
        .iter()
        .filter_map(|step| Span::new(step.started_at, step.completed_at))
        .chain(in_flight)
        .collect();
    (union(spans), TimedBy::Steps)
}

/// When the pipeline first recorded the item.
fn started_at(body: &Value) -> Option<DateTime<Utc>> {
    text(body, "created_at")
        .and_then(parse_at)
        .or_else(|| history(body).find_map(|event| text(event, "at").and_then(parse_at)))
}

/// The moment a source recorded in `field`: from the earliest history event
/// carrying it, else the earliest a stage block still holds.
fn recorded_at(body: &Value, field: &str) -> Option<DateTime<Utc>> {
    let of = |value: &Value| text(value, field).and_then(parse_at);
    history(body)
        .find_map(|event| event.get("detail").and_then(of))
        .or_else(|| {
            body.get("stages")
                .and_then(Value::as_object)
                .into_iter()
                .flatten()
                .filter_map(|(_, block)| of(block))
                .min()
        })
}

/// Where the clock starts, and whether a recorded origin had to be pulled
/// back to `started` by more than the tolerance.
fn origin(def: &PipelineDefinition, body: &Value, started: DateTime<Utc>) -> (OriginTime, bool) {
    let declared = def.timings.as_ref().and_then(|t| t.origin.as_ref());
    let recorded = declared.and_then(|origin| recorded_at(body, &origin.field));
    let origin = OriginTime {
        at: recorded.map_or(started, |at| at.min(started)),
        label: declared.map(|origin| origin.label.clone()),
        recorded: recorded.is_some(),
    };
    let after_start = recorded.is_some_and(|at| ms_between(started, at) > SKEW_TOLERANCE_MS);
    (origin, after_start)
}

/// Where each run of consecutive placements in one column began, on a clock
/// that never runs backwards, and whether it had to hold back an event
/// recorded more than the tolerance behind an earlier one.
fn entries<'a>(
    def: &'a PipelineDefinition,
    body: &'a Value,
    started: DateTime<Utc>,
) -> (Vec<(&'a str, DateTime<Utc>)>, bool) {
    let (entries, _, adjusted) = placements(def, body).fold(
        (Vec::new(), started, false),
        |(mut entries, clock, adjusted): (Vec<(&str, DateTime<Utc>)>, _, _), placement| {
            let at = parse_at(placement.at).unwrap_or(clock);
            let clock_behind = ms_between(at, clock) > SKEW_TOLERANCE_MS;
            let clock = clock.max(at);
            let column = placement.column;
            if entries.last().is_none_or(|&(last, _)| last != column) {
                entries.push((column, clock));
            }
            (entries, clock, adjusted || clock_behind)
        },
    );
    (entries, adjusted)
}

/// The stretch of time an item's visits cover.
struct Bounds {
    origin: DateTime<Utc>,
    recorded: bool,
    started: DateTime<Utc>,
    end: DateTime<Utc>,
    open: bool,
}

/// The item's time cut into visits: before the pipeline, before the first
/// placement, then one per column entry, each ending where the next begins.
fn visits(
    def: &PipelineDefinition,
    bounds: &Bounds,
    entries: &[(&str, DateTime<Utc>)],
    truncated: bool,
    work: Option<&[Span]>,
) -> Vec<Visit> {
    let first_placed = entries.first().map_or(bounds.end, |&(_, at)| at);
    let before_first = if truncated {
        VisitKind::Trimmed
    } else {
        VisitKind::Unplaced
    };
    let leading = bounds
        .recorded
        .then_some((VisitKind::BeforePipeline, bounds.origin, bounds.started))
        .into_iter()
        .chain([(before_first, bounds.started, first_placed)])
        .filter(|&(_, from, to)| ms_between(from, to) > 0)
        .map(|(kind, from, _)| (kind, None, from));
    let starts: Vec<(VisitKind, Option<&str>, DateTime<Utc>)> = leading
        .chain(
            entries
                .iter()
                .map(|&(column, at)| (VisitKind::Column, Some(column), at)),
        )
        .collect();
    starts
        .iter()
        .enumerate()
        .map(|(i, &(kind, column, entered_at))| {
            let next = starts.get(i + 1).map(|&(_, _, at)| at);
            let until = next.unwrap_or(bounds.end);
            let elapsed_ms = ms_between(entered_at, until);
            let work_ms = work.map(|work| work_between(work, entered_at, until));
            Visit {
                kind,
                column: column.map(str::to_string),
                label: column.map(|column| def.column_label(column)),
                entered_at,
                left_at: next.or((!bounds.open).then_some(bounds.end)),
                elapsed_ms,
                work_ms,
                waiting_ms: work_ms.map(|work| elapsed_ms.saturating_sub(work)),
            }
        })
        .collect()
}

/// Each milestone lane with when it was reached: the first lane visit at or
/// after it since the item was last in an earlier lane. Exits neither reach
/// nor undo one.
fn milestones(def: &PipelineDefinition, visits: &[Visit], origin: DateTime<Utc>) -> Vec<Milestone> {
    let lane_visits: Vec<(usize, DateTime<Utc>)> = visits
        .iter()
        .filter_map(|visit| Some((def.lane_index(visit.column.as_deref()?)?, visit.entered_at)))
        .collect();
    def.milestones()
        .into_iter()
        .filter_map(|lane| Some((def.lane_index(&lane.id)?, lane)))
        .map(|(index, lane)| {
            let reached_at = lane_visits.iter().fold(None, |reached, &(at_index, at)| {
                (at_index >= index).then(|| reached.unwrap_or(at))
            });
            Milestone {
                column: lane.id.clone(),
                label: lane.label.clone(),
                reached_at,
                from_origin_ms: reached_at.map(|at| ms_between(origin, at)),
            }
        })
        .collect()
}

fn columns(visits: &[Visit]) -> Vec<ColumnTimes> {
    grouped(
        visits
            .iter()
            .filter(|visit| visit.kind == VisitKind::Column),
        |&visit| visit.column.as_deref(),
    )
    .into_iter()
    .map(|(column, members)| ColumnTimes {
        column: column.unwrap_or_default().to_string(),
        label: members[0].label.clone().unwrap_or_default(),
        visits: members.len(),
        elapsed_ms: members.iter().map(|visit| visit.elapsed_ms).sum(),
        work_ms: members.iter().map(|visit| visit.work_ms).sum(),
        waiting_ms: members.iter().map(|visit| visit.waiting_ms).sum(),
    })
    .collect()
}

/// How long `item` has taken so far: its visits read through `def`, its work
/// from the runs its record names that `runs` holds (limited to `agent`'s
/// when the pipeline is an agent's), and the rest as waiting. `runs` is
/// `None` when the hub keeps no workflow runs; work and waiting are then
/// left out.
pub fn item_timings(
    def: &PipelineDefinition,
    item: &Document,
    runs: Option<&HashMap<String, Run>>,
    agent: Option<&str>,
    as_of: DateTime<Utc>,
) -> ItemTimings {
    let body = &item.body;
    let started = started_at(body).unwrap_or(as_of);
    let (origin, origin_after_start) = origin(def, body, started);
    let (entries, out_of_order) = entries(def, body, started);
    let ends = def.ends();
    let finished_at = entries
        .last()
        .filter(|(column, _)| ends.contains(column))
        .map(|&(_, at)| at);
    let latest = entries.last().map_or(started, |&(_, at)| at);
    let bounds = Bounds {
        origin: origin.at,
        recorded: origin.recorded,
        started,
        end: finished_at.unwrap_or(latest.max(as_of)),
        open: finished_at.is_none(),
    };
    let truncated = history(body).count() >= HISTORY_MAX;

    let named = sightings(body);
    let timed: Option<Vec<(&Run, Vec<Span>, TimedBy)>> = runs.map(|runs| {
        named
            .iter()
            .filter_map(|sighting| runs.get(sighting.run))
            .filter(|run| joinable(run, agent))
            .map(|run| {
                let (spans, timed_by) = run_spans(run, as_of);
                (run, spans, timed_by)
            })
            .collect()
    });
    let work: Option<Vec<Span>> = timed.as_ref().map(|timed| {
        union(
            timed
                .iter()
                .flat_map(|(_, spans, _)| spans.iter().copied())
                .collect(),
        )
    });
    let visits = visits(def, &bounds, &entries, truncated, work.as_deref());
    let elapsed_ms = ms_between(bounds.origin, bounds.end);
    let work_ms = work
        .as_deref()
        .map(|work| work_between(work, bounds.origin, bounds.end));
    let run_time_clipped = work.as_deref().zip(work_ms).is_some_and(|(work, within)| {
        work.iter()
            .copied()
            .map(Span::ms)
            .sum::<u64>()
            .saturating_sub(within)
            > SKEW_TOLERANCE_MS
    });
    let clock_ahead = bounds.open && ms_between(as_of, latest) > SKEW_TOLERANCE_MS;
    let timed = timed.unwrap_or_default();

    let mut run_times: Vec<RunTimes> = timed
        .iter()
        .map(|&(run, ref spans, timed_by)| RunTimes {
            run: run.id.clone(),
            workflow: run.workflow_id.clone(),
            status: run.status.clone(),
            started_at: parse_at(&run.created_at),
            ended_at: (run.status != RUNNING)
                .then(|| parse_at(&run.updated_at))
                .flatten(),
            work_ms: work_between(spans, bounds.origin, bounds.end),
            timed_by,
        })
        .collect();
    run_times.sort_by_key(|run| run.started_at);

    ItemTimings {
        item: item.doc_id.clone(),
        as_of,
        started_at: started,
        finished_at,
        total: Totals {
            elapsed_ms,
            in_pipeline_ms: ms_between(started, bounds.end),
            work_ms,
            waiting_ms: work_ms.map(|work| elapsed_ms.saturating_sub(work)),
            open: bounds.open,
            working: bounds.open && timed.iter().any(|(run, _, _)| run.status == RUNNING),
        },
        milestones: milestones(def, &visits, bounds.origin),
        columns: columns(&visits),
        coverage: TimingCoverage {
            runs_named: named.len(),
            runs_timed: timed.len(),
            runs_unmatched: runs.map_or(0, |_| named.len() - timed.len()),
            runs_by_lifetime: timed
                .iter()
                .filter(|(_, _, timed_by)| *timed_by == TimedBy::Lifetime)
                .count(),
            work_unavailable: runs.is_none(),
            origin_recorded: origin.recorded,
            origin_after_start,
            history_truncated: truncated,
            clock_adjusted: out_of_order || run_time_clipped || clock_ahead,
        },
        origin,
        visits,
        runs: run_times,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use workforce_common::persistence::WorkflowInstanceRow;
    use workforce_orchestrator::workflow::instance::{
        EngineAttempt, StepProvenance, StepRecord, REPORT_ENGINE, TOOL_ENGINE,
    };

    use super::*;
    use crate::pipeline::fixtures::{
        doc, joined, ord_2797, orders, record, record_with, row, run, T0, T1, T2, T3,
    };
    use crate::pipeline::runs::runs_by_id;
    use crate::pipeline::{apply_event, EventInput, Origin, Rule, Timings, Tone};

    fn at(s: &str) -> DateTime<Utc> {
        parse_at(s).unwrap()
    }

    /// The example pipeline as revision 10 times it: from filing, to Deployed and Verified,
    /// stopping at Verified, Not a code change and Duplicate.
    fn timed() -> PipelineDefinition {
        PipelineDefinition {
            timings: Some(Timings {
                origin: Some(Origin {
                    field: "filed_at".into(),
                    label: "Filed".into(),
                }),
                milestones: Some(vec!["deployed".into(), "verified".into()]),
                ends: Some(vec![
                    "verified".into(),
                    "not-code".into(),
                    "duplicate".into(),
                ]),
            }),
            ..orders()
        }
    }

    fn step(from: &str, to: &str) -> StepRecord {
        StepRecord {
            step_id: "step".into(),
            outcome: None,
            agent_response: String::new(),
            outputs: None,
            report: None,
            started_at: at(from),
            completed_at: at(to),
            rounds: 0,
            cost_usd: 0.0,
            budget_capped: false,
            failed: false,
            correction: None,
            provenance: StepProvenance::default(),
        }
    }

    fn with_status(status: &str, run: Run) -> Run {
        Run {
            status: status.into(),
            ..run
        }
    }

    /// The timings, checked to add up: leading and column visits to the total
    /// elapsed, work and waiting to each visit, the visits' work to the total
    /// and each column to its visits.
    fn timings_of(
        def: &PipelineDefinition,
        item: &Document,
        runs: Option<&HashMap<String, Run>>,
        as_of: &str,
    ) -> ItemTimings {
        let t = item_timings(def, item, runs, Some("example"), at(as_of));
        assert_eq!(
            t.visits.iter().map(|v| v.elapsed_ms).sum::<u64>(),
            t.total.elapsed_ms
        );
        for v in &t.visits {
            match (v.work_ms, v.waiting_ms) {
                (Some(work), Some(waiting)) => assert_eq!(work + waiting, v.elapsed_ms),
                other => assert_eq!(other, (None, None)),
            }
        }
        assert_eq!(
            t.visits.iter().map(|v| v.work_ms).sum::<Option<u64>>(),
            t.total.work_ms
        );
        match (t.total.work_ms, t.total.waiting_ms) {
            (Some(work), Some(waiting)) => assert_eq!(work + waiting, t.total.elapsed_ms),
            other => assert_eq!(other, (None, None)),
        }
        for c in &t.columns {
            let members = t
                .visits
                .iter()
                .filter(|v| v.column.as_deref() == Some(c.column.as_str()));
            assert_eq!(
                members.clone().map(|v| v.elapsed_ms).sum::<u64>(),
                c.elapsed_ms
            );
            assert_eq!(members.map(|v| v.work_ms).sum::<Option<u64>>(), c.work_ms);
        }
        t
    }

    fn visit_starts(t: &ItemTimings) -> Vec<(VisitKind, Option<&str>, String)> {
        t.visits
            .iter()
            .map(|v| (v.kind, v.column.as_deref(), v.entered_at.to_rfc3339()))
            .collect()
    }

    fn columns_and_ms(t: &ItemTimings) -> Vec<(&str, u64)> {
        t.visits
            .iter()
            .map(|v| (v.column.as_deref().unwrap_or("-"), v.elapsed_ms))
            .collect()
    }

    const ORD_2797_NOW: &str = "2026-09-15T04:55:00Z";

    #[test]
    fn a_recorded_item_reproduces_the_boards_visits() {
        let t = timings_of(&orders(), &ord_2797(), None, ORD_2797_NOW);
        assert_eq!(
            columns_and_ms(&t),
            [
                ("implementing", 2_822_200),
                ("pr-open", 56_964),
                ("reviewing", 6_053_851),
                ("sent-back", 967_192),
                ("implementing", 15_577_485),
                ("pr-open", 204_144),
                ("reviewing", 1_518_843),
                ("human-merge", 2_912),
                ("reviewed", 2_257_049),
                ("deploying", 151_445),
                ("deployed", 37_165_343),
                ("verifying", 419_229),
            ]
        );
        assert!(t.visits.iter().all(|v| v.kind == VisitKind::Column));
        let verifying = t.visits.last().unwrap();
        assert_eq!(verifying.entered_at, at("2026-09-15T04:48:00.771949Z"));
        assert_eq!(verifying.left_at, None);
        assert_eq!(
            t.visits[10].left_at,
            Some(verifying.entered_at),
            "a visit ends where the next begins"
        );
        assert_eq!(t.visits[7].label.as_deref(), Some("Awaiting a human merge"));
        assert_eq!(t.total.elapsed_ms, 67_196_657);
        assert_eq!(t.total.in_pipeline_ms, 67_196_657);
        assert!(t.total.open);
        assert_eq!(t.finished_at, None);

        let per_column: Vec<(&str, usize, u64)> = t
            .columns
            .iter()
            .map(|c| (c.column.as_str(), c.visits, c.elapsed_ms))
            .collect();
        assert_eq!(per_column[0], ("implementing", 2, 18_399_685));
        assert_eq!(per_column[1], ("pr-open", 2, 261_108));
        assert_eq!(per_column[2], ("reviewing", 2, 7_572_694));
        assert_eq!(per_column.len(), 9);
    }

    #[test]
    fn an_annotation_neither_splits_nor_starts_a_visit() {
        let mut def = orders();
        def.events.insert(
            "review/overlap-judged".into(),
            vec![Rule {
                annotation: true,
                tone: Some(Tone::Warn),
                label: Some("judged".into()),
                ..Default::default()
            }],
        );
        let plain = [
            (T0, "implement", "claimed", None, json!({})),
            (T2, "review", "claimed", None, json!({})),
        ];
        let noted = [
            plain[0].clone(),
            (T1, "review", "overlap-judged", None, json!({})),
            (
                "2026-09-12T09:30:00Z",
                "review",
                "overlap-judged",
                None,
                json!({}),
            ),
            plain[1].clone(),
        ];
        let without = timings_of(&def, &record_with("ORD-1", &plain), None, T3);
        let with = timings_of(&def, &record_with("ORD-1", &noted), None, T3);
        assert_eq!(visit_starts(&with), visit_starts(&without));
        assert_eq!(columns_and_ms(&with), columns_and_ms(&without));
        assert_eq!(
            columns_and_ms(&with),
            [("implementing", 2 * 3_600_000), ("reviewing", 3_600_000)]
        );
    }

    #[test]
    fn visits_change_on_column_not_event() {
        let item = record_with(
            "ORD-1",
            &[
                (T0, "review", "follow-ups", Some("r0"), json!({})),
                (
                    "2026-09-12T08:30:00Z",
                    "implement",
                    "claimed",
                    Some("r1"),
                    json!({}),
                ),
                (
                    "2026-09-12T08:40:00Z",
                    "review",
                    "follow-ups",
                    Some("r1"),
                    json!({}),
                ),
                (T1, "implement", "claimed", Some("r2"), json!({})),
                (T2, "verify", "started", Some("r3"), json!({"rung": "1h"})),
                (
                    "2026-09-12T10:20:00Z",
                    "verify",
                    "inconclusive",
                    Some("r3"),
                    json!({"final": "false"}),
                ),
                (
                    "2026-09-12T10:30:00Z",
                    "verify",
                    "not-live",
                    Some("r4"),
                    json!({}),
                ),
            ],
        );
        let t = timings_of(&orders(), &item, None, T3);
        assert_eq!(
            visit_starts(&t),
            [
                (VisitKind::Unplaced, None, at(T0).to_rfc3339()),
                (
                    VisitKind::Column,
                    Some("implementing"),
                    at("2026-09-12T08:30:00Z").to_rfc3339()
                ),
                (VisitKind::Column, Some("verifying"), at(T2).to_rfc3339()),
                (
                    VisitKind::Column,
                    Some("not-live"),
                    at("2026-09-12T10:30:00Z").to_rfc3339()
                ),
            ]
        );
        assert_eq!(t.visits[0].elapsed_ms, 30 * 60_000);
        assert_eq!(t.visits[1].elapsed_ms, 90 * 60_000);
    }

    #[test]
    fn the_earliest_recorded_origin_wins_and_starts_the_clock_before_the_pipeline() {
        let events = [
            (
                "2026-09-14T10:15:03Z",
                "implement",
                "claimed",
                Some("r1"),
                json!({"filed_at": "2026-09-14T09:00:00Z"}),
            ),
            (
                "2026-09-14T13:00:03Z",
                "implement",
                "claimed",
                Some("r2"),
                json!({"filed_at": "2026-09-14T08:00:00Z"}),
            ),
        ];
        let t = timings_of(
            &timed(),
            &record_with("ORD-1", &events),
            None,
            "2026-09-14T14:00:00Z",
        );
        assert_eq!(t.origin.at, at("2026-09-14T09:00:00Z"));
        assert!(t.origin.recorded && t.coverage.origin_recorded);
        assert_eq!(t.origin.label.as_deref(), Some("Filed"));
        assert_eq!(t.started_at, at("2026-09-14T10:15:03Z"));
        assert_eq!(t.visits[0].kind, VisitKind::BeforePipeline);
        assert_eq!(t.visits[0].elapsed_ms, 75 * 60_000 + 3_000);
        assert_eq!(t.visits[0].left_at, Some(t.started_at));
        assert_eq!(t.total.elapsed_ms, 5 * 3_600_000);
        assert_eq!(
            t.total.in_pipeline_ms,
            t.total.elapsed_ms - t.visits[0].elapsed_ms
        );

        let mut trimmed = record_with("ORD-1", &events);
        for event in trimmed.body["history"].as_array_mut().unwrap() {
            event["detail"] = json!({});
        }
        let t = timings_of(&timed(), &trimmed, None, "2026-09-14T14:00:00Z");
        assert_eq!(
            t.origin.at,
            at("2026-09-14T08:00:00Z"),
            "a stage block recovers the origin a history no longer carries"
        );
        assert!(t.origin.recorded);
    }

    #[test]
    fn without_a_recorded_origin_the_clock_starts_when_the_pipeline_first_recorded_the_item() {
        let t = timings_of(&timed(), &ord_2797(), None, ORD_2797_NOW);
        assert_eq!(t.origin.at, at("2026-09-14T10:15:03.343111Z"));
        assert_eq!(t.origin.at, t.started_at);
        assert!(!t.origin.recorded && !t.coverage.origin_recorded);
        assert_eq!(t.origin.label.as_deref(), Some("Filed"));
        assert_eq!(t.visits[0].kind, VisitKind::Column);

        let untimed = timings_of(&orders(), &ord_2797(), None, ORD_2797_NOW);
        assert_eq!(untimed.origin.label, None);
    }

    #[test]
    fn an_origin_after_the_first_event_is_clamped_and_flagged_past_the_tolerance() {
        let filed = |filed_at: &str| {
            record_with(
                "ORD-1",
                &[(
                    T0,
                    "implement",
                    "claimed",
                    Some("r1"),
                    json!({"filed_at": filed_at}),
                )],
            )
        };
        let late = timings_of(&timed(), &filed("2026-09-12T08:05:00Z"), None, T1);
        assert_eq!(late.origin.at, at(T0));
        assert!(late.coverage.origin_after_start);
        assert_eq!(late.total.elapsed_ms, 3_600_000);

        let skewed = timings_of(&timed(), &filed("2026-09-12T08:00:30Z"), None, T1);
        assert_eq!(skewed.origin.at, at(T0));
        assert!(!skewed.coverage.origin_after_start);
        assert!(skewed.visits.iter().all(|v| v.kind == VisitKind::Column));
    }

    #[test]
    fn mixed_timestamp_formats_order_by_instant() {
        let item = record(
            "ORD-1",
            &[
                (
                    "2026-09-12T20:01:04.159189141+01:00",
                    "implement",
                    "claimed",
                    None,
                ),
                ("2026-09-12T19:30:00.286220Z", "review", "claimed", None),
            ],
        );
        let t = timings_of(&orders(), &item, None, "2026-09-12T20:00:00Z");
        assert!(!t.coverage.clock_adjusted);
        assert_eq!(t.visits[0].elapsed_ms, 28 * 60_000 + 56_127);
        assert_eq!(t.visits[1].elapsed_ms, 29 * 60_000 + 59_714);
    }

    #[test]
    fn an_event_recorded_behind_an_earlier_one_holds_the_clock_and_is_flagged_past_the_tolerance() {
        let retried = |late: &str| {
            record(
                "ORD-1",
                &[
                    (T1, "implement", "claimed", None),
                    (late, "review", "claimed", None),
                    (T2, "deploy", "claimed", None),
                ],
            )
        };
        let within = timings_of(&orders(), &retried("2026-09-12T08:59:30Z"), None, T3);
        assert_eq!(
            columns_and_ms(&within),
            [
                ("implementing", 0),
                ("reviewing", 3_600_000),
                ("deploying", 3_600_000)
            ]
        );
        assert!(!within.coverage.clock_adjusted);

        let beyond = timings_of(&orders(), &retried("2026-09-12T08:30:00Z"), None, T3);
        assert_eq!(beyond.visits[0].elapsed_ms, 0);
        assert!(beyond.coverage.clock_adjusted);
    }

    fn two_stage_item() -> Document {
        record(
            "ORD-1",
            &[
                (T0, "implement", "claimed", Some("r1")),
                (T1, "review", "claimed", Some("r2")),
            ],
        )
    }

    #[test]
    fn work_is_the_union_of_step_intervals_and_the_gaps_are_waiting() {
        let runs = joined(vec![
            run(
                "r1",
                T0,
                None,
                vec![
                    step("2026-09-12T08:00:00Z", "2026-09-12T08:10:00Z"),
                    step("2026-09-12T08:05:00Z", "2026-09-12T08:15:00Z"),
                ],
            ),
            run(
                "r2",
                T0,
                None,
                vec![
                    step("2026-09-12T08:10:00Z", "2026-09-12T08:20:00Z"),
                    step("2026-09-12T09:30:00Z", "2026-09-12T09:40:00Z"),
                ],
            ),
        ]);
        let t = timings_of(&orders(), &two_stage_item(), Some(&runs), T2);
        let split: Vec<(Option<u64>, Option<u64>)> =
            t.visits.iter().map(|v| (v.work_ms, v.waiting_ms)).collect();
        assert_eq!(
            split,
            [
                (Some(20 * 60_000), Some(40 * 60_000)),
                (Some(10 * 60_000), Some(50 * 60_000))
            ]
        );
        assert_eq!(t.total.work_ms, Some(30 * 60_000));
        assert_eq!(t.total.waiting_ms, Some(90 * 60_000));
        assert_eq!(t.coverage.runs_timed, 2);
        let per_run: Vec<(&str, u64, TimedBy)> = t
            .runs
            .iter()
            .map(|r| (r.run.as_str(), r.work_ms, r.timed_by))
            .collect();
        assert_eq!(
            per_run,
            [
                ("r1", 15 * 60_000, TimedBy::Steps),
                ("r2", 20 * 60_000, TimedBy::Steps)
            ]
        );
        assert!(!t.total.working);
    }

    #[test]
    fn tool_report_and_failed_steps_count_and_handed_over_attempts_count_once() {
        let with_engine = |engine: &str, from: &str, to: &str| StepRecord {
            provenance: StepProvenance {
                engine: Some(engine.into()),
                ..Default::default()
            },
            ..step(from, to)
        };
        let fell_back = StepRecord {
            provenance: StepProvenance {
                engine: Some("claude-code:bedrock".into()),
                handed_over: vec![
                    EngineAttempt {
                        engine: "claude-code".into(),
                        ..Default::default()
                    },
                    EngineAttempt {
                        engine: "codex".into(),
                        ..Default::default()
                    },
                ],
                ..Default::default()
            },
            ..step("2026-09-12T08:30:00Z", "2026-09-12T08:40:00Z")
        };
        let runs = joined(vec![run(
            "r1",
            T0,
            None,
            vec![
                with_engine(TOOL_ENGINE, "2026-09-12T08:00:00Z", "2026-09-12T08:05:00Z"),
                with_engine(
                    REPORT_ENGINE,
                    "2026-09-12T08:05:00Z",
                    "2026-09-12T08:10:00Z",
                ),
                StepRecord {
                    failed: true,
                    correction: None,
                    ..step("2026-09-12T08:10:00Z", "2026-09-12T08:20:00Z")
                },
                fell_back,
            ],
        )]);
        let t = timings_of(&orders(), &two_stage_item(), Some(&runs), T1);
        assert_eq!(t.total.work_ms, Some(30 * 60_000));
    }

    #[test]
    fn a_running_run_works_to_now_and_paused_or_queued_runs_do_not() {
        let item = record("ORD-1", &[(T0, "implement", "claimed", Some("r1"))]);
        let running = joined(vec![with_status(
            "running",
            run(
                "r1",
                T0,
                None,
                vec![step("2026-09-12T08:00:00Z", "2026-09-12T08:10:00Z")],
            ),
        )]);
        let t = timings_of(&orders(), &item, Some(&running), T2);
        assert_eq!(t.total.work_ms, Some(2 * 3_600_000));
        assert!(t.total.working);
        assert_eq!(t.runs[0].ended_at, None);

        let waiting = record(
            "ORD-1",
            &[
                (T0, "implement", "claimed", Some("paused")),
                (T0, "implement", "claimed", Some("queued")),
            ],
        );
        let runs = joined(vec![
            with_status(
                "paused",
                run(
                    "paused",
                    T0,
                    None,
                    vec![step("2026-09-12T08:00:00Z", "2026-09-12T08:05:00Z")],
                ),
            ),
            with_status(
                "queued",
                run(
                    "queued",
                    T0,
                    None,
                    vec![step("2026-09-12T08:20:00Z", "2026-09-12T08:25:00Z")],
                ),
            ),
        ]);
        let t = timings_of(&orders(), &waiting, Some(&runs), T2);
        assert_eq!(t.total.work_ms, Some(10 * 60_000));
        assert!(!t.total.working);
    }

    #[test]
    fn a_run_without_step_history_is_timed_by_its_lifetime() {
        let item = record(
            "ORD-1",
            &[
                (T0, "implement", "claimed", Some("empty")),
                (T0, "implement", "claimed", Some("garbage")),
            ],
        );
        let rows = vec![
            WorkflowInstanceRow {
                updated_at: "2026-09-12T08:30:00Z".into(),
                ..row("empty", "order-intake", Some("example"), Vec::new())
            },
            WorkflowInstanceRow {
                status: "running".into(),
                step_history: "not json".into(),
                created_at: "2026-09-12T09:00:00Z".into(),
                ..row("garbage", "order-intake", Some("example"), Vec::new())
            },
        ];
        let runs = runs_by_id(rows, &HashMap::new());
        let t = timings_of(&orders(), &item, Some(&runs), T2);
        assert_eq!(t.total.work_ms, Some(90 * 60_000));
        assert_eq!(t.coverage.runs_by_lifetime, 2);
        assert!(t.runs.iter().all(|r| r.timed_by == TimedBy::Lifetime));
        assert!(t.total.working);
    }

    #[test]
    fn an_agents_pipeline_times_only_its_runs_and_a_global_one_times_every_run() {
        let item = record(
            "ORD-1",
            &[
                (T0, "implement", "claimed", Some("mine")),
                (T0, "implement", "claimed", Some("quills")),
                (T0, "implement", "claimed", Some("chat-session")),
            ],
        );
        let ten_minutes = || vec![step("2026-09-12T08:00:00Z", "2026-09-12T08:10:00Z")];
        let runs = joined(vec![
            run("mine", T0, None, ten_minutes()),
            Run {
                agent_id: Some("quill".into()),
                ..run(
                    "quills",
                    T0,
                    None,
                    vec![step("2026-09-12T08:30:00Z", "2026-09-12T08:40:00Z")],
                )
            },
        ]);
        let t = timings_of(&orders(), &item, Some(&runs), T1);
        assert_eq!(
            (
                t.coverage.runs_named,
                t.coverage.runs_timed,
                t.coverage.runs_unmatched
            ),
            (3, 1, 2)
        );
        assert_eq!(t.total.work_ms, Some(10 * 60_000));

        let global = item_timings(&orders(), &item, Some(&runs), None, at(T1));
        assert_eq!(
            (global.coverage.runs_timed, global.coverage.runs_unmatched),
            (2, 1)
        );
        assert_eq!(global.total.work_ms, Some(20 * 60_000));
    }

    #[test]
    fn without_runs_work_and_waiting_are_unavailable_and_elapsed_stands() {
        let t = timings_of(&orders(), &ord_2797(), None, ORD_2797_NOW);
        assert!(t.coverage.work_unavailable);
        assert_eq!((t.total.work_ms, t.total.waiting_ms), (None, None));
        assert!(t
            .visits
            .iter()
            .all(|v| v.work_ms.is_none() && v.waiting_ms.is_none()));
        assert!(t.columns.iter().all(|c| c.work_ms.is_none()));
        assert_eq!(t.coverage.runs_named, 8);
        assert_eq!((t.coverage.runs_timed, t.coverage.runs_unmatched), (0, 0));
        assert!(t.runs.is_empty());

        let none_matched = timings_of(&orders(), &ord_2797(), Some(&HashMap::new()), ORD_2797_NOW);
        assert!(!none_matched.coverage.work_unavailable);
        assert_eq!(none_matched.total.work_ms, Some(0));
        assert_eq!(
            none_matched.total.waiting_ms,
            Some(none_matched.total.elapsed_ms)
        );
        assert_eq!(none_matched.coverage.runs_unmatched, 8);
    }

    #[test]
    fn work_before_the_origin_and_after_the_clock_stops_is_excluded() {
        let item = record(
            "ORD-1",
            &[
                (T0, "implement", "claimed", Some("r1")),
                (T1, "verify", "verified", Some("r1")),
            ],
        );
        let clipped = |before: &str, after: &str| {
            joined(vec![run(
                "r1",
                T0,
                None,
                vec![step(before, "2026-09-12T08:30:00Z"), step(T1, after)],
            )])
        };
        let t = timings_of(
            &orders(),
            &item,
            Some(&clipped("2026-09-12T07:00:00Z", "2026-09-12T09:30:00Z")),
            T3,
        );
        assert_eq!(t.finished_at, Some(at(T1)));
        assert_eq!(t.total.work_ms, Some(30 * 60_000));
        assert_eq!(t.runs[0].work_ms, 30 * 60_000);
        assert!(t.coverage.clock_adjusted);

        let t = timings_of(
            &orders(),
            &item,
            Some(&clipped("2026-09-12T07:59:40Z", "2026-09-12T09:00:20Z")),
            T3,
        );
        assert_eq!(t.total.work_ms, Some(30 * 60_000));
        assert!(
            !t.coverage.clock_adjusted,
            "40 s clipped is within the tolerance"
        );
    }

    fn ord_2786() -> Document {
        record_with(
            "ORD-2786",
            &[
                (
                    "2026-09-12T19:10:02.634029109+00:00",
                    "implement",
                    "claimed",
                    Some("667f36b9"),
                    json!({}),
                ),
                (
                    "2026-09-12T19:29:57.609633187+00:00",
                    "implement",
                    "pr-opened",
                    Some("667f36b9"),
                    json!({}),
                ),
                (
                    "2026-09-12T19:37:06.957011743+00:00",
                    "review",
                    "claimed",
                    Some("c264cc5e"),
                    json!({}),
                ),
                (
                    "2026-09-12T19:56:42.329362175+00:00",
                    "merge",
                    "gate",
                    Some("c264cc5e"),
                    json!({"outcome": "human"}),
                ),
                (
                    "2026-09-12T19:56:45.209338617+00:00",
                    "review",
                    "finished",
                    Some("c264cc5e"),
                    json!({"verdict": "stage-reviewed"}),
                ),
                (
                    "2026-09-13T04:46:03.375300Z",
                    "deploy",
                    "claimed",
                    Some("62a48a75"),
                    json!({}),
                ),
                (
                    "2026-09-13T04:53:31.503577Z",
                    "deploy",
                    "finished",
                    Some("62a48a75"),
                    json!({"label": "stage-deployed"}),
                ),
                (
                    "2026-09-13T09:22:17.571585Z",
                    "verify",
                    "rung",
                    Some("838fdb04"),
                    json!({"state": "pass", "rung": "1h"}),
                ),
                (
                    "2026-09-13T12:53:53.767341Z",
                    "verify",
                    "verified",
                    Some("4587bdd6"),
                    json!({"state": "pass", "rung": "6h"}),
                ),
                (
                    "2026-09-13T12:59:41.454694Z",
                    "verify",
                    "rung",
                    Some("d27673e6"),
                    json!({"state": "pass", "rung": "6h"}),
                ),
                (
                    "2026-09-15T04:36:57.544095Z",
                    "verify",
                    "verified",
                    Some("6b043b6d"),
                    json!({"reconciled": "true", "rung": "6h"}),
                ),
            ],
        )
    }

    fn reached(t: &ItemTimings) -> Vec<(&str, Option<DateTime<Utc>>)> {
        t.milestones
            .iter()
            .map(|m| (m.column.as_str(), m.reached_at))
            .collect()
    }

    #[test]
    fn a_later_lane_reaches_an_earlier_milestone() {
        let item = record_with(
            "ORD-2758",
            &[
                (
                    "2026-09-13T16:33:05.633028Z",
                    "verify",
                    "started",
                    Some("098e2cc1"),
                    json!({"rung": "24h"}),
                ),
                (
                    "2026-09-13T16:38:00.919031Z",
                    "verify",
                    "inconclusive",
                    Some("098e2cc1"),
                    json!({"final": "false", "rung": "24h"}),
                ),
            ],
        );
        let t = timings_of(&timed(), &item, None, "2026-09-15T04:55:00Z");
        assert_eq!(
            reached(&t),
            [
                ("deployed", Some(at("2026-09-13T16:33:05.633028Z"))),
                ("verified", None)
            ]
        );
        assert_eq!(t.milestones[0].from_origin_ms, Some(0));
        assert_eq!(t.visits.len(), 1);
    }

    #[test]
    fn an_earlier_lane_undoes_a_milestone_until_it_is_reached_again() {
        let t = timings_of(&timed(), &ord_2786(), None, "2026-09-15T06:00:00Z");
        assert_eq!(
            reached(&t),
            [
                ("deployed", Some(at("2026-09-13T04:53:31.503577Z"))),
                ("verified", Some(at("2026-09-15T04:36:57.544095Z"))),
            ]
        );
        assert_eq!(t.finished_at, Some(at("2026-09-15T04:36:57.544095Z")));
        let last = t.visits.last().unwrap();
        assert_eq!(
            (last.column.as_deref(), last.elapsed_ms),
            (Some("verified"), 0)
        );
        assert_eq!(last.left_at, Some(last.entered_at));
        assert!(!t.total.open);
        assert_eq!(
            t.milestones[1].from_origin_ms,
            Some(ms_between(
                at("2026-09-12T19:10:02.634Z"),
                at("2026-09-15T04:36:57.544Z")
            ))
        );
    }

    #[test]
    fn exits_neither_reach_nor_undo_a_milestone() {
        let t = timings_of(&timed(), &ord_2797(), None, ORD_2797_NOW);
        assert_eq!(
            reached(&t),
            [
                ("deployed", Some(at("2026-09-14T18:28:35.428027Z"))),
                ("verified", None)
            ]
        );
        assert_eq!(t.milestones[0].from_origin_ms, Some(29_612_085));

        let not_live = record_with(
            "ORD-1",
            &[
                (
                    T0,
                    "deploy",
                    "finished",
                    None,
                    json!({"label": "stage-deployed"}),
                ),
                (T1, "verify", "not-live", None, json!({})),
            ],
        );
        let t = timings_of(&timed(), &not_live, None, T2);
        assert_eq!(reached(&t)[0], ("deployed", Some(at(T0))));
        assert!(t.total.open);
    }

    #[test]
    fn a_blocked_deploy_does_not_reach_deployed() {
        let item = record_with(
            "ORD-1",
            &[
                (T0, "deploy", "claimed", None, json!({})),
                (
                    T1,
                    "deploy",
                    "finished",
                    None,
                    json!({"label": "stage-deploy-blocked"}),
                ),
            ],
        );
        let t = timings_of(&timed(), &item, None, T2);
        assert_eq!(reached(&t)[0], ("deployed", None));
        assert_eq!(t.visits[1].column.as_deref(), Some("deploy-blocked"));
    }

    #[test]
    fn duplicate_deploys_reach_deployed_at_the_first() {
        let deployed = json!({"label": "stage-deployed"});
        let item = record_with(
            "ORD-2811",
            &[
                (
                    "2026-09-14T18:16:05.003890Z",
                    "deploy",
                    "claimed",
                    Some("7e6bbacb"),
                    json!({}),
                ),
                (
                    "2026-09-14T18:46:20.293784Z",
                    "deploy",
                    "finished",
                    Some("474142d6"),
                    deployed.clone(),
                ),
                (
                    "2026-09-14T18:46:22.075370Z",
                    "deploy",
                    "finished",
                    Some("9532db33"),
                    deployed.clone(),
                ),
                (
                    "2026-09-14T18:50:24.869635Z",
                    "deploy",
                    "finished",
                    Some("a42ac0f3"),
                    deployed,
                ),
            ],
        );
        let t = timings_of(&timed(), &item, None, "2026-09-15T04:00:00Z");
        assert_eq!(
            reached(&t)[0],
            ("deployed", Some(at("2026-09-14T18:46:20.293784Z")))
        );
        assert_eq!(t.visits.len(), 2);
    }

    #[test]
    fn a_failed_verification_sent_back_to_implementing_restarts_the_milestones() {
        let deployed = json!({"label": "stage-deployed"});
        let item = record_with(
            "ORD-1",
            &[
                (T0, "deploy", "finished", None, deployed.clone()),
                (
                    "2026-09-12T08:30:00Z",
                    "verify",
                    "failed",
                    None,
                    json!({"state": "not-fixed"}),
                ),
                (T1, "implement", "claimed", None, json!({})),
                (T2, "deploy", "finished", None, deployed),
            ],
        );
        let t = timings_of(&timed(), &item, None, T3);
        assert_eq!(reached(&t)[0], ("deployed", Some(at(T2))));
    }

    #[test]
    fn an_end_exit_stops_the_clock_and_a_person_s_exit_keeps_it_running() {
        let skipped = record_with(
            "ORD-1",
            &[
                (T0, "implement", "claimed", None, json!({})),
                (
                    T1,
                    "implement",
                    "released",
                    None,
                    json!({"label": "stage-skipped"}),
                ),
            ],
        );
        let t = timings_of(&timed(), &skipped, None, T3);
        assert_eq!(t.finished_at, Some(at(T1)));
        assert_eq!(t.total.elapsed_ms, 3_600_000);
        assert_eq!(reached(&t), [("deployed", None), ("verified", None)]);
        assert!(!t.total.open);

        let released = record_with(
            "ORD-1",
            &[
                (T0, "implement", "claimed", None, json!({})),
                (
                    T1,
                    "implement",
                    "released",
                    None,
                    json!({"label": "stage-blocked"}),
                ),
            ],
        );
        let now = timings_of(&timed(), &released, None, T2);
        let later = timings_of(&timed(), &released, None, T3);
        assert!(now.total.open && later.total.open);
        assert_eq!(later.total.elapsed_ms - now.total.elapsed_ms, 3_600_000);
        assert_eq!(later.visits[1].left_at, None);
    }

    #[test]
    fn a_capped_history_starts_with_a_trimmed_visit() {
        let body = (0..HISTORY_MAX + 5).fold(
            record("ORD-1", &[(T0, "implement", "claimed", None)]).body,
            |body, i| {
                apply_event(
                    body,
                    &EventInput {
                        item: "ORD-1".into(),
                        stage: "review".into(),
                        event: "claimed".into(),
                        detail: Value::Null,
                        run: None,
                    },
                    &format!("2026-09-12T09:{:02}:{:02}Z", i / 60, i % 60),
                    None,
                )
            },
        );
        let t = timings_of(&orders(), &doc("ORD-1", body), None, T2);
        assert!(t.coverage.history_truncated);
        assert_eq!(t.visits[0].kind, VisitKind::Trimmed);
        assert_eq!(t.visits[0].entered_at, at(T0));
        assert_eq!(t.visits[0].elapsed_ms, 3_600_000 + 5_000);
        assert_eq!(t.visits[1].column.as_deref(), Some("reviewing"));
    }

    #[test]
    fn an_empty_record_or_one_without_history_has_degenerate_times() {
        let empty = timings_of(
            &orders(),
            &doc("ORD-1", json!({})),
            Some(&HashMap::new()),
            T1,
        );
        assert!(empty.visits.is_empty());
        assert_eq!((empty.started_at, empty.total.elapsed_ms), (at(T1), 0));
        assert!(empty.total.open);
        assert!(empty.milestones.iter().all(|m| m.reached_at.is_none()));

        let bare = timings_of(
            &orders(),
            &doc("ORD-1", json!({"created_at": T0, "history": "none"})),
            None,
            T1,
        );
        assert_eq!(
            visit_starts(&bare),
            [(VisitKind::Unplaced, None, at(T0).to_rfc3339())]
        );
        assert_eq!(bare.visits[0].left_at, None);
        assert_eq!(bare.total.elapsed_ms, 3_600_000);
    }

    #[test]
    fn the_timings_serialise_with_their_field_names_and_spellings() {
        let runs = joined(vec![run(
            "0e3be51e",
            "2026-09-15T04:47:58Z",
            Some("ORD-2797"),
            vec![step("2026-09-15T04:47:58Z", "2026-09-15T04:53:21Z")],
        )]);
        let t = timings_of(&timed(), &ord_2797(), Some(&runs), ORD_2797_NOW);
        let json = serde_json::to_value(&t).unwrap();
        let keys = |v: &Value| {
            let mut keys: Vec<String> = v.as_object().unwrap().keys().cloned().collect();
            keys.sort_unstable();
            keys
        };
        assert_eq!(
            keys(&json),
            [
                "as_of",
                "columns",
                "coverage",
                "finished_at",
                "item",
                "milestones",
                "origin",
                "runs",
                "started_at",
                "total",
                "visits"
            ]
        );
        assert_eq!(keys(&json["origin"]), ["at", "label", "recorded"]);
        assert_eq!(
            keys(&json["total"]),
            [
                "elapsed_ms",
                "in_pipeline_ms",
                "open",
                "waiting_ms",
                "work_ms",
                "working"
            ]
        );
        assert_eq!(
            keys(&json["visits"][0]),
            [
                "column",
                "elapsed_ms",
                "entered_at",
                "kind",
                "label",
                "left_at",
                "waiting_ms",
                "work_ms"
            ]
        );
        assert_eq!(
            keys(&json["runs"][0]),
            [
                "ended_at",
                "run",
                "started_at",
                "status",
                "timed_by",
                "work_ms",
                "workflow"
            ]
        );
        assert_eq!(
            keys(&json["coverage"]),
            [
                "clock_adjusted",
                "history_truncated",
                "origin_after_start",
                "origin_recorded",
                "runs_by_lifetime",
                "runs_named",
                "runs_timed",
                "runs_unmatched",
                "work_unavailable"
            ]
        );
        assert_eq!(
            keys(&json["milestones"][0]),
            ["column", "from_origin_ms", "label", "reached_at"]
        );
        assert_eq!(
            keys(&json["columns"][0]),
            [
                "column",
                "elapsed_ms",
                "label",
                "visits",
                "waiting_ms",
                "work_ms"
            ]
        );
        assert_eq!(json["visits"][0]["kind"], "column");
        assert_eq!(json["runs"][0]["timed_by"], "steps");
        assert_eq!(json["origin"]["at"], "2026-09-14T10:15:03.343111Z");
        assert_eq!(json["visits"][11]["left_at"], Value::Null);
        assert_eq!(json["total"]["work_ms"], 5 * 60_000 + 23_000);
        assert_eq!(json["milestones"][1]["reached_at"], Value::Null);
    }
}
