//! What an item has cost so far, read from the step history of the runs its
//! record names. Each run counts on exactly one item, so a pipeline's items
//! add up to its runs; within an item the stages add up to the total and the
//! models to their stage.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;
use workforce_orchestrator::workflow::instance::{
    StepRecord, DECISION_ENGINE, REPORT_ENGINE, TOOL_ENGINE, WORKFLOW_ENGINE,
};

use super::runs::{joinable, sightings, Run};
use super::{grouped, placements, PipelineDefinition, HISTORY_MAX};
use crate::coding_engine::{billing_of, Billing};
use crate::store::Document;

#[derive(Debug, Clone, Serialize)]
pub struct ItemCosts {
    pub item: String,
    pub currency: &'static str,
    pub as_of: DateTime<Utc>,
    pub total: Totals,
    pub coverage: Coverage,
    pub stages: Vec<StageCosts>,
    pub runs: Vec<RunCosts>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Totals {
    pub billed_usd: f64,
    pub estimated_usd: f64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub runs: usize,
}

/// How much of what the item's record names the figures cover.
#[derive(Debug, Clone, Serialize)]
pub struct Coverage {
    pub runs_named: usize,
    pub runs_costed: usize,
    /// Runs with no workflow instance this pipeline may join: a chat session,
    /// a deleted instance, a run that does not execute as the pipeline's agent.
    pub runs_unmatched: usize,
    /// Runs counted on another item that also recorded them.
    pub runs_shared: usize,
    /// Model steps recorded before step costs were.
    pub steps_unrecorded: usize,
    /// The record's history is at its cap, so older runs may be missing.
    pub history_truncated: bool,
}

impl Coverage {
    /// The record names runs and none of them joined.
    pub fn nothing_matched(&self) -> bool {
        self.runs_named > 0 && self.runs_unmatched == self.runs_named
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct StageCosts {
    pub stage: String,
    pub label: String,
    pub workflows: Vec<String>,
    pub runs: usize,
    pub billed_usd: f64,
    pub estimated_usd: f64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub models: Vec<ModelCosts>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ModelCosts {
    pub engine: Option<String>,
    pub model: Option<String>,
    pub billing: Billing,
    pub attempts: usize,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost_usd: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct RunCosts {
    pub run: String,
    pub workflow: String,
    pub status: String,
    pub stage: String,
    pub label: String,
    pub started_at: String,
    pub updated_at: String,
    /// The item the run counts on.
    pub booked_to: String,
    /// The other items that recorded the run.
    pub also_on: Vec<String>,
    pub billed_usd: f64,
    pub estimated_usd: f64,
    pub steps: Vec<StepCosts>,
}

#[derive(Debug, Clone, Serialize)]
pub struct StepCosts {
    pub step_id: String,
    pub failed: bool,
    pub engine: Option<String>,
    pub model: Option<String>,
    /// Set when the step ran a model on an engine the record names.
    pub billing: Option<Billing>,
    pub rounds: u32,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    /// `None` when the step ran before its cost was recorded.
    pub cost_usd: Option<f64>,
    pub handed_over: Vec<AttemptCosts>,
    pub started_at: DateTime<Utc>,
    pub completed_at: DateTime<Utc>,
    pub budget_capped: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct AttemptCosts {
    pub engine: String,
    pub model: Option<String>,
    pub billing: Billing,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost_usd: f64,
    /// Why the chain moved on from the attempt, when the record kept it.
    pub error: Option<String>,
}

/// The item a run counts on, of the `(item, first recorded at)` holders that
/// recorded it. A run counts once so a pipeline's items add up to its runs:
/// on the ticket it booked its cost to when that ticket holds it, else on the
/// item that recorded it first, the smaller id breaking a tie.
fn owner<'a>(booked: Option<&str>, holders: &[(&'a str, &'a str)]) -> &'a str {
    booked
        .and_then(|ticket| holders.iter().find(|(doc, _)| *doc == ticket))
        .or_else(|| {
            holders
                .iter()
                .min_by(|a, b| a.1.cmp(b.1).then(a.0.cmp(b.0)))
        })
        .map_or("", |(doc, _)| doc)
}

/// For each of `runs`, every record naming it, `item` first, with when each
/// first recorded it.
fn holders<'a>(
    runs: &HashSet<&str>,
    item: &'a Document,
    records: &'a [Document],
) -> HashMap<&'a str, Vec<(&'a str, &'a str)>> {
    std::iter::once(item)
        .chain(records.iter().filter(|doc| doc.doc_id != item.doc_id))
        .flat_map(|doc| {
            sightings(&doc.body)
                .into_iter()
                .filter(|sighting| runs.contains(sighting.run))
                .map(move |sighting| (sighting.run, (doc.doc_id.as_str(), sighting.at)))
        })
        .fold(
            HashMap::new(),
            |mut held: HashMap<_, Vec<_>>, (run, holder)| {
                held.entry(run).or_default().push(holder);
                held
            },
        )
}

/// What one engine run inside a step consumed.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Usage<'a> {
    engine: Option<&'a str>,
    model: Option<&'a str>,
    input_tokens: u64,
    output_tokens: u64,
    cost_usd: f64,
}

/// A step's spend split by the engine runs inside it: each attempt it handed
/// over from, then the attempt that served or failed the step, which gets
/// what the step's totals leave.
fn usage(step: &StepRecord) -> Vec<Usage<'_>> {
    let provenance = &step.provenance;
    let handed: Vec<Usage<'_>> = provenance
        .handed_over
        .iter()
        .map(|attempt| Usage {
            engine: Some(attempt.engine.as_str()),
            model: attempt.model.as_deref(),
            input_tokens: attempt.input_tokens,
            output_tokens: attempt.output_tokens,
            cost_usd: attempt.cost_usd,
        })
        .collect();
    let served = Usage {
        engine: provenance.engine.as_deref(),
        model: provenance.model.as_deref(),
        input_tokens: provenance
            .input_tokens
            .unwrap_or(0)
            .saturating_sub(handed.iter().map(|u| u.input_tokens).sum::<u64>()),
        output_tokens: provenance
            .output_tokens
            .unwrap_or(0)
            .saturating_sub(handed.iter().map(|u| u.output_tokens).sum::<u64>()),
        cost_usd: (step.cost_usd - handed.iter().map(|u| u.cost_usd).sum::<f64>()).max(0.0),
    };
    handed.into_iter().chain(std::iter::once(served)).collect()
}

/// Whether an engine ran a model: any but a tool's, a report's, a decision's
/// or a child run's, including one the record does not name.
fn ran_a_model(engine: Option<&str>) -> bool {
    !matches!(
        engine,
        Some(TOOL_ENGINE | REPORT_ENGINE | DECISION_ENGINE | WORKFLOW_ENGINE)
    )
}

/// A model step recorded before step costs were: it ran rounds, but names no
/// engine and carries no cost. A tool or report step runs no rounds, so its
/// zero is genuine.
fn unrecorded(step: &StepRecord) -> bool {
    step.provenance.engine.is_none() && step.rounds > 0 && step.cost_usd == 0.0
}

/// A usage's billing: its engine's, and billed when the record names none.
fn billing(engine: Option<&str>) -> Billing {
    engine.map_or(Billing::Billed, billing_of)
}

/// Whether a usage is shown against a model: a model ran, and the record
/// names its engine or carries a cost for it.
fn shown_as_model(usage: &Usage<'_>) -> bool {
    ran_a_model(usage.engine) && (usage.engine.is_some() || usage.cost_usd > 0.0)
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct Tally {
    billed_usd: f64,
    estimated_usd: f64,
    input_tokens: u64,
    output_tokens: u64,
}

impl Tally {
    fn of(usage: &Usage<'_>) -> Self {
        let (billed_usd, estimated_usd) = match billing(usage.engine) {
            Billing::Billed => (usage.cost_usd, 0.0),
            Billing::Estimated => (0.0, usage.cost_usd),
        };
        Self {
            billed_usd,
            estimated_usd,
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
        }
    }

    fn plus(self, other: Self) -> Self {
        Self {
            billed_usd: self.billed_usd + other.billed_usd,
            estimated_usd: self.estimated_usd + other.estimated_usd,
            input_tokens: self.input_tokens + other.input_tokens,
            output_tokens: self.output_tokens + other.output_tokens,
        }
    }

    fn sum<'a>(usages: impl IntoIterator<Item = &'a Usage<'a>>) -> Self {
        usages
            .into_iter()
            .map(Self::of)
            .fold(Self::default(), Self::plus)
    }
}

/// One row per engine, model and billing the usages ran on, costliest first.
fn models<'a>(usages: impl IntoIterator<Item = &'a Usage<'a>>) -> Vec<ModelCosts> {
    let mut rows: Vec<ModelCosts> = usages
        .into_iter()
        .filter(|usage| shown_as_model(usage))
        .fold(
            HashMap::new(),
            |mut rows: HashMap<(Option<&str>, Option<&str>, Billing), ModelCosts>, usage| {
                let bill = billing(usage.engine);
                let row = rows
                    .entry((usage.engine, usage.model, bill))
                    .or_insert_with(|| ModelCosts {
                        engine: usage.engine.map(str::to_string),
                        model: usage.model.map(str::to_string),
                        billing: bill,
                        attempts: 0,
                        input_tokens: 0,
                        output_tokens: 0,
                        cost_usd: 0.0,
                    });
                row.attempts += 1;
                row.input_tokens += usage.input_tokens;
                row.output_tokens += usage.output_tokens;
                row.cost_usd += usage.cost_usd;
                rows
            },
        )
        .into_values()
        .collect();
    rows.sort_by(|a, b| {
        b.cost_usd
            .total_cmp(&a.cost_usd)
            .then_with(|| a.engine.cmp(&b.engine))
            .then_with(|| a.model.cmp(&b.model))
    });
    rows
}

fn step_costs(step: &StepRecord) -> StepCosts {
    let provenance = &step.provenance;
    StepCosts {
        step_id: step.step_id.clone(),
        failed: step.failed,
        engine: provenance.engine.clone(),
        model: provenance.model.clone(),
        billing: provenance
            .engine
            .as_deref()
            .filter(|engine| ran_a_model(Some(engine)))
            .map(billing_of),
        rounds: step.rounds,
        input_tokens: provenance.input_tokens,
        output_tokens: provenance.output_tokens,
        cost_usd: (!unrecorded(step)).then_some(step.cost_usd),
        handed_over: provenance
            .handed_over
            .iter()
            .map(|attempt| AttemptCosts {
                engine: attempt.engine.clone(),
                model: attempt.model.clone(),
                billing: billing_of(&attempt.engine),
                input_tokens: attempt.input_tokens,
                output_tokens: attempt.output_tokens,
                cost_usd: attempt.cost_usd,
                error: attempt.error.clone(),
            })
            .collect(),
        started_at: step.started_at,
        completed_at: step.completed_at,
        budget_capped: step.budget_capped,
    }
}

/// A stage as the board names it: the column the definition places the
/// item's first placed event in that stage in, else the stage id.
fn stage_label(def: &PipelineDefinition, body: &Value, stage: &str) -> String {
    placements(def, body)
        .find(|placement| placement.stage == stage)
        .map_or_else(
            || stage.to_string(),
            |placement| def.column_label(placement.column),
        )
}

/// A run the item's record names and this pipeline may join.
struct Listed<'a> {
    stage: &'a str,
    costs: RunCosts,
    /// The run, when it counts on this item.
    costed: Option<&'a Run>,
}

/// What `item` has cost so far: every run its record names, joined through
/// `runs` (limited to `agent`'s when the pipeline is an agent's) and counted
/// on one item of `records`, rolled up by stage.
pub fn item_costs(
    def: &PipelineDefinition,
    item: &Document,
    records: &[Document],
    runs: &HashMap<String, Run>,
    agent: Option<&str>,
    as_of: DateTime<Utc>,
) -> ItemCosts {
    let seen = sightings(&item.body);
    let named: HashSet<&str> = seen.iter().map(|sighting| sighting.run).collect();
    let held = holders(&named, item, records);
    let listed: Vec<Listed<'_>> = seen
        .iter()
        .filter_map(|sighting| {
            let run = runs.get(sighting.run).filter(|run| joinable(run, agent))?;
            let holding = held
                .get(sighting.run)
                .map(Vec::as_slice)
                .unwrap_or_default();
            let counted_on = owner(run.booked.as_deref(), holding);
            let costed = (counted_on == item.doc_id).then_some(run);
            let tally = costed
                .map(|run| Tally::sum(&run.steps.iter().flat_map(usage).collect::<Vec<_>>()))
                .unwrap_or_default();
            Some(Listed {
                stage: sighting.stage,
                costs: RunCosts {
                    run: run.id.clone(),
                    workflow: run.workflow_id.clone(),
                    status: run.status.clone(),
                    stage: sighting.stage.to_string(),
                    label: stage_label(def, &item.body, sighting.stage),
                    started_at: run.created_at.clone(),
                    updated_at: run.updated_at.clone(),
                    booked_to: counted_on.to_string(),
                    also_on: holding
                        .iter()
                        .map(|(doc, _)| *doc)
                        .filter(|doc| *doc != counted_on)
                        .map(str::to_string)
                        .collect(),
                    billed_usd: tally.billed_usd,
                    estimated_usd: tally.estimated_usd,
                    steps: costed
                        .map(|run| run.steps.iter().map(step_costs).collect())
                        .unwrap_or_default(),
                },
                costed,
            })
        })
        .collect();

    let costed: Vec<(&str, &str, &Run)> = listed
        .iter()
        .filter_map(|listed| Some((listed.stage, listed.costs.label.as_str(), listed.costed?)))
        .collect();
    let (stages, tallies): (Vec<StageCosts>, Vec<Tally>) =
        grouped(costed.iter().copied(), |&(stage, _, _)| stage)
            .into_iter()
            .map(|(stage, members)| {
                let usages: Vec<Usage<'_>> = members
                    .iter()
                    .flat_map(|&(_, _, run)| run.steps.iter().flat_map(usage))
                    .collect();
                let tally = Tally::sum(&usages);
                let stage_costs = StageCosts {
                    stage: stage.to_string(),
                    label: members[0].1.to_string(),
                    workflows: grouped(members.iter(), |&&(_, _, run)| run.workflow_id.as_str())
                        .into_iter()
                        .map(|(workflow, _)| workflow.to_string())
                        .collect(),
                    runs: members.len(),
                    billed_usd: tally.billed_usd,
                    estimated_usd: tally.estimated_usd,
                    input_tokens: tally.input_tokens,
                    output_tokens: tally.output_tokens,
                    models: models(&usages),
                };
                (stage_costs, tally)
            })
            .unzip();
    let total = tallies.into_iter().fold(Tally::default(), Tally::plus);
    let coverage = Coverage {
        runs_named: seen.len(),
        runs_costed: costed.len(),
        runs_unmatched: seen.len() - listed.len(),
        runs_shared: listed.len() - costed.len(),
        steps_unrecorded: costed
            .iter()
            .flat_map(|&(_, _, run)| &run.steps)
            .filter(|step| unrecorded(step))
            .count(),
        history_truncated: item
            .body
            .get("history")
            .and_then(Value::as_array)
            .is_some_and(|history| history.len() >= HISTORY_MAX),
    };
    let runs_costed = costed.len();
    let mut run_costs: Vec<RunCosts> = listed.into_iter().map(|listed| listed.costs).collect();
    run_costs.sort_by_cached_key(|run| DateTime::parse_from_rfc3339(&run.started_at).ok());

    ItemCosts {
        item: item.doc_id.clone(),
        currency: "USD",
        as_of,
        total: Totals {
            billed_usd: total.billed_usd,
            estimated_usd: total.estimated_usd,
            input_tokens: total.input_tokens,
            output_tokens: total.output_tokens,
            runs: runs_costed,
        },
        coverage,
        stages,
        runs: run_costs,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use workforce_common::persistence::WorkflowInstanceRow;
    use workforce_orchestrator::workflow::instance::{EngineAttempt, StepProvenance};

    use crate::pipeline::fixtures::{doc, joined, record, row, run, T0, T1, T2, T3};
    use crate::pipeline::runs::{runs_by_id, runs_named};
    use crate::pipeline::{apply_event, EventInput};

    fn def() -> PipelineDefinition {
        serde_json::from_value(json!({
            "name": "Orders",
            "collection": "pipeline",
            "lanes": [
                {"id": "implementing", "label": "Implementing"},
                {"id": "reviewing", "label": "Reviewing"},
                {"id": "verifying", "label": "Verifying"}
            ],
            "events": {
                "implement/claimed": [{"column": "implementing"}],
                "review/claimed": [{"column": "reviewing"}],
                "verify/rung": [{"column": "verifying"}]
            }
        }))
        .unwrap()
    }

    /// A completed step on `engine` that consumed 1,000 tokens in and 100 out.
    fn step(
        step_id: &str,
        engine: Option<&str>,
        model: Option<&str>,
        rounds: u32,
        cost_usd: f64,
    ) -> StepRecord {
        let at = Utc::now();
        StepRecord {
            step_id: step_id.into(),
            outcome: None,
            agent_response: String::new(),
            outputs: None,
            report: None,
            started_at: at,
            completed_at: at,
            rounds,
            cost_usd,
            budget_capped: false,
            failed: false,
            correction: None,
            provenance: StepProvenance {
                engine: engine.map(str::to_string),
                model: model.map(str::to_string),
                input_tokens: engine.map(|_| 1_000),
                output_tokens: engine.map(|_| 100),
                ..Default::default()
            },
        }
    }

    fn costs_of(item: &Document, records: &[Document], runs: &HashMap<String, Run>) -> ItemCosts {
        item_costs(&def(), item, records, runs, Some("example"), Utc::now())
    }

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn stage_blocks_recover_runs_a_capped_history_dropped() {
        let first = record(
            "ORD-1",
            &[
                (T0, "implement", "claimed", Some("r0")),
                (T1, "review", "claimed", Some("r1")),
            ],
        );
        let capped = (0..HISTORY_MAX).fold(first.body, |body, i| {
            apply_event(
                body,
                &EventInput {
                    item: "ORD-1".into(),
                    stage: "review".into(),
                    event: "claimed".into(),
                    detail: Value::Null,
                    run: Some("r1".into()),
                },
                &format!("2026-09-12T1{}:{:02}:00Z", 2 + i / 60, i % 60),
                None,
            )
        });
        assert_eq!(capped["history"].as_array().unwrap().len(), HISTORY_MAX);
        assert_eq!(runs_named(&capped), ["r0", "r1"]);

        let item = doc("ORD-1", capped);
        let runs = joined(vec![
            run(
                "r0",
                T0,
                None,
                vec![step("implement", Some("claude-code:bedrock"), None, 3, 1.0)],
            ),
            run(
                "r1",
                T1,
                None,
                vec![step("review", Some("claude-code:bedrock"), None, 3, 0.5)],
            ),
        ]);
        let costs = costs_of(&item, std::slice::from_ref(&item), &runs);
        assert!(costs.coverage.history_truncated);
        assert_eq!(costs.coverage.runs_costed, 2);
        assert_eq!(costs.stages[0].stage, "implement");
        assert_eq!(
            costs.stages[0].label, "implement",
            "no event of the stage is left to place"
        );
        assert_eq!(costs.stages[1].label, "Reviewing");
        assert!(close(costs.total.billed_usd, 1.5));
    }

    #[test]
    fn a_run_on_one_item_is_that_items() {
        assert_eq!(owner(None, &[("ORD-1", T0)]), "ORD-1");
        assert_eq!(owner(Some("ORD-9"), &[("ORD-1", T0)]), "ORD-1");

        let item = record("ORD-1", &[(T0, "implement", "claimed", Some("r1"))]);
        let runs = joined(vec![run(
            "r1",
            T0,
            Some("ORD-9"),
            vec![step("implement", Some("claude-code:bedrock"), None, 3, 1.0)],
        )]);
        let costs = costs_of(&item, std::slice::from_ref(&item), &runs);
        assert_eq!(costs.runs[0].booked_to, "ORD-1");
        assert!(costs.runs[0].also_on.is_empty());
        assert_eq!(costs.coverage.runs_costed, 1);
    }

    #[test]
    fn a_run_on_several_items_is_the_booked_tickets() {
        assert_eq!(
            owner(Some("ORD-2"), &[("ORD-1", T0), ("ORD-2", T1)]),
            "ORD-2"
        );
    }

    #[test]
    fn without_a_booked_holder_the_first_item_to_record_the_run_owns_it() {
        assert_eq!(
            owner(None, &[("ORD-2", T1), ("ORD-3", T2), ("ORD-1", T0)]),
            "ORD-1"
        );
        assert_eq!(
            owner(Some("DW-9"), &[("ORD-2", T1), ("ORD-3", T0)]),
            "ORD-3",
            "a booked ticket outside the pipeline falls back to the first holder"
        );
        assert_eq!(
            owner(None, &[("ORD-2", T0), ("ORD-1", T0)]),
            "ORD-1",
            "a tie goes to the smaller id"
        );
    }

    #[test]
    fn a_shared_run_is_listed_at_zero_and_left_out_of_the_totals() {
        let first = record("ORD-1", &[(T0, "implement", "claimed", Some("r1"))]);
        let booked = record("ORD-2", &[(T1, "implement", "claimed", Some("r1"))]);
        let records = [first.clone(), booked.clone()];
        let runs = joined(vec![run(
            "r1",
            T0,
            Some("ORD-2"),
            vec![step(
                "implement",
                Some("claude-code:bedrock"),
                Some("claude-opus-4-8"),
                10,
                2.0,
            )],
        )]);

        let shared = costs_of(&first, &records, &runs);
        assert_eq!(shared.runs.len(), 1);
        assert_eq!(shared.runs[0].booked_to, "ORD-2");
        assert_eq!(shared.runs[0].also_on, ["ORD-1"]);
        assert_eq!(shared.runs[0].billed_usd, 0.0);
        assert!(shared.runs[0].steps.is_empty());
        assert_eq!(shared.total.billed_usd, 0.0);
        assert_eq!(shared.total.runs, 0);
        assert!(shared.stages.is_empty());
        assert_eq!(
            (shared.coverage.runs_shared, shared.coverage.runs_costed),
            (1, 0)
        );

        let owned = costs_of(&booked, &records, &runs);
        assert_eq!(owned.runs[0].booked_to, "ORD-2");
        assert_eq!(owned.runs[0].also_on, ["ORD-1"]);
        assert!(close(owned.total.billed_usd, 2.0));
        assert_eq!(
            (owned.coverage.runs_shared, owned.coverage.runs_costed),
            (0, 1)
        );
    }

    #[test]
    fn billed_and_estimated_split_by_engine() {
        let item = record("ORD-1", &[(T0, "implement", "claimed", Some("r1"))]);
        let runs = joined(vec![run(
            "r1",
            T0,
            None,
            vec![
                step("a", Some("claude-code:bedrock"), None, 1, 1.0),
                step("b", Some("claude-code"), None, 1, 2.0),
                step("c", Some("codex"), None, 1, 0.5),
                step("d", Some("codex_api"), None, 1, 0.25),
                step("e", Some("agent"), None, 1, 0.125),
                step("f", Some("claude-pilot"), None, 1, 0.25),
            ],
        )]);
        let costs = costs_of(&item, std::slice::from_ref(&item), &runs);
        assert!(close(costs.total.billed_usd, 1.375));
        assert!(close(costs.total.estimated_usd, 2.75));
        assert!(close(costs.runs[0].billed_usd, 1.375));
        assert!(close(costs.runs[0].estimated_usd, 2.75));
        let billing: Vec<Option<Billing>> = costs.runs[0].steps.iter().map(|s| s.billing).collect();
        assert_eq!(
            billing,
            [
                Some(Billing::Billed),
                Some(Billing::Estimated),
                Some(Billing::Estimated),
                Some(Billing::Billed),
                Some(Billing::Billed),
                Some(Billing::Estimated),
            ]
        );
    }

    #[test]
    fn handed_over_attempts_are_itemised_and_the_served_model_gets_the_remainder() {
        let attempt = |engine: &str, model: &str, cost_usd: f64| EngineAttempt {
            engine: engine.into(),
            model: Some(model.into()),
            input_tokens: 1_000,
            output_tokens: 100,
            cost_usd,
            error: None,
        };
        let fell_back = StepRecord {
            provenance: StepProvenance {
                engine: Some("claude-code:bedrock".into()),
                model: Some("claude-sonnet-5".into()),
                input_tokens: Some(4_000),
                output_tokens: Some(500),
                handed_over: vec![
                    attempt("claude-code", "claude-opus-4-8", 0.5),
                    attempt("codex", "gpt-6-astra", 0.25),
                ],
                ..Default::default()
            },
            ..step("implement", None, None, 30, 1.75)
        };
        let item = record("ORD-1", &[(T0, "implement", "claimed", Some("r1"))]);
        let runs = joined(vec![run("r1", T0, None, vec![fell_back])]);
        let costs = costs_of(&item, std::slice::from_ref(&item), &runs);

        let chips: Vec<(Option<&str>, Option<&str>, Billing, u64, f64)> = costs.stages[0]
            .models
            .iter()
            .map(|m| {
                (
                    m.engine.as_deref(),
                    m.model.as_deref(),
                    m.billing,
                    m.input_tokens,
                    m.cost_usd,
                )
            })
            .collect();
        assert_eq!(
            chips,
            [
                (
                    Some("claude-code:bedrock"),
                    Some("claude-sonnet-5"),
                    Billing::Billed,
                    2_000,
                    1.0
                ),
                (
                    Some("claude-code"),
                    Some("claude-opus-4-8"),
                    Billing::Estimated,
                    1_000,
                    0.5
                ),
                (
                    Some("codex"),
                    Some("gpt-6-astra"),
                    Billing::Estimated,
                    1_000,
                    0.25
                ),
            ]
        );
        let step = &costs.runs[0].steps[0];
        assert_eq!(step.cost_usd, Some(1.75));
        assert_eq!(step.handed_over.len(), 2);
        assert_eq!(step.handed_over[0].billing, Billing::Estimated);
        assert!(close(costs.total.billed_usd, 1.0));
        assert!(close(costs.total.estimated_usd, 0.75));
        assert_eq!(costs.total.input_tokens, 4_000);
        assert_eq!(costs.total.output_tokens, 500);
    }

    #[test]
    fn failed_attempts_count_toward_their_stage() {
        let failed = StepRecord {
            failed: true,
            correction: None,
            ..step("implement", Some("claude-code:bedrock"), None, 12, 0.625)
        };
        let item = record("ORD-1", &[(T0, "implement", "claimed", Some("r1"))]);
        let runs = joined(vec![run(
            "r1",
            T0,
            None,
            vec![
                failed,
                step("implement", Some("claude-code:bedrock"), None, 9, 0.375),
            ],
        )]);
        let costs = costs_of(&item, std::slice::from_ref(&item), &runs);
        assert!(close(costs.stages[0].billed_usd, 1.0));
        assert_eq!(costs.stages[0].models[0].attempts, 2);
        assert!(costs.runs[0].steps[0].failed);
        assert!(!costs.runs[0].steps[1].failed);
    }

    #[test]
    fn an_errored_attempt_is_a_zero_cost_chip_that_carries_its_error() {
        let error = "secure command runner unavailable: output consumer dropped";
        let served = StepRecord {
            provenance: StepProvenance {
                engine: Some("claude-code:bedrock".into()),
                model: Some("claude-sonnet-5".into()),
                input_tokens: Some(1_000),
                output_tokens: Some(100),
                handed_over: vec![EngineAttempt {
                    engine: "codex".into(),
                    model: Some("gpt-5.6-sol".into()),
                    error: Some(error.into()),
                    ..Default::default()
                }],
                ..Default::default()
            },
            ..step("implement", None, None, 12, 0.5)
        };
        let item = record("ORD-1", &[(T0, "implement", "claimed", Some("r1"))]);
        let runs = joined(vec![run("r1", T0, None, vec![served])]);
        let costs = costs_of(&item, std::slice::from_ref(&item), &runs);

        let codex = costs.stages[0]
            .models
            .iter()
            .find(|m| m.engine.as_deref() == Some("codex"))
            .expect("the errored attempt is shown");
        assert_eq!((codex.attempts, codex.cost_usd), (1, 0.0));
        assert_eq!(codex.model.as_deref(), Some("gpt-5.6-sol"));
        assert_eq!(
            costs.runs[0].steps[0].handed_over[0].error.as_deref(),
            Some(error)
        );
        assert!(close(costs.total.billed_usd, 0.5));
        assert!(close(costs.total.estimated_usd, 0.0));
        assert_eq!(costs.total.input_tokens, 1_000);
        assert_eq!(costs.total.output_tokens, 100);
    }

    #[test]
    fn model_steps_before_provenance_are_not_recorded_but_tool_steps_are_free() {
        let item = record("ORD-1", &[(T0, "implement", "claimed", Some("r1"))]);
        let runs = joined(vec![run(
            "r1",
            T0,
            None,
            vec![
                step("pick", None, None, 0, 0.0),
                step("implement", None, None, 14, 0.0),
                step("review", None, None, 6, 0.3),
            ],
        )]);
        let costs = costs_of(&item, std::slice::from_ref(&item), &runs);
        let steps = &costs.runs[0].steps;
        assert_eq!(steps[0].cost_usd, Some(0.0));
        assert_eq!(steps[1].cost_usd, None);
        assert_eq!(steps[2].cost_usd, Some(0.3));
        assert!(steps.iter().all(|step| step.billing.is_none()));
        assert_eq!(costs.coverage.steps_unrecorded, 1);
        assert!(close(costs.total.billed_usd, 0.3));
        let models = &costs.stages[0].models;
        assert_eq!(models.len(), 1, "only the legacy step with a cost is shown");
        assert_eq!(
            (models[0].engine.as_deref(), models[0].billing),
            (None, Billing::Billed)
        );
    }

    #[test]
    fn runs_of_another_agent_or_with_no_agent_are_unmatched() {
        let item = record(
            "ORD-1",
            &[
                (T0, "implement", "claimed", Some("mine")),
                (T1, "review", "claimed", Some("quills")),
                (T2, "review", "claimed", Some("nobodys")),
                (T3, "verify", "rung", Some("chat-session")),
            ],
        );
        let billed = || vec![step("s", Some("claude-code:bedrock"), None, 1, 1.0)];
        let runs = joined(vec![
            run("mine", T0, None, billed()),
            Run {
                agent_id: Some("quill".into()),
                ..run("quills", T1, None, billed())
            },
            Run {
                agent_id: None,
                ..run("nobodys", T2, None, billed())
            },
        ]);
        let costs = costs_of(&item, std::slice::from_ref(&item), &runs);
        assert_eq!(
            costs
                .runs
                .iter()
                .map(|r| r.run.as_str())
                .collect::<Vec<_>>(),
            ["mine"]
        );
        assert_eq!(costs.coverage.runs_named, 4);
        assert_eq!(costs.coverage.runs_unmatched, 3);
        assert!(close(costs.total.billed_usd, 1.0));
        assert!(!costs.coverage.nothing_matched());
    }

    #[test]
    fn an_agents_pipeline_costs_the_runs_it_ran_before_runs_recorded_their_agent() {
        let item = record(
            "ORD-2797",
            &[
                (T0, "implement", "claimed", Some("r-implement")),
                (T1, "review", "claimed", Some("r-review")),
                (T2, "review", "claimed", Some("r-review-again")),
                (T3, "sweep", "merged", Some("r-sweep")),
                ("2026-09-12T12:00:00Z", "verify", "rung", Some("r-verify")),
            ],
        );
        let billed = |cost_usd| vec![step("s", Some("claude-code:bedrock"), None, 1, cost_usd)];
        let rows = || {
            vec![
                row("r-implement", "order-intake", None, billed(2.0)),
                row("r-review", "order-review", None, billed(0.5)),
                WorkflowInstanceRow {
                    status: "failed".into(),
                    ..row("r-review-again", "order-review", None, billed(0.25))
                },
                row("r-sweep", "order-release", None, billed(0.125)),
                row("r-verify", "order-verify", None, billed(1.0)),
            ]
        };
        let declared: HashMap<String, String> = [
            ("order-intake", "example"),
            ("order-review", "example"),
            ("order-release", "example"),
            ("order-verify", "quill"),
        ]
        .into_iter()
        .map(|(workflow, agent)| (workflow.to_string(), agent.to_string()))
        .collect();

        let costs = costs_of(
            &item,
            std::slice::from_ref(&item),
            &runs_by_id(rows(), &declared),
        );
        assert_eq!(costs.coverage.runs_named, 5);
        assert_eq!(costs.coverage.runs_costed, 4);
        assert_eq!(costs.coverage.runs_unmatched, 1);
        assert!(close(costs.total.billed_usd, 2.875));
        assert!(!costs.coverage.nothing_matched());

        let undeclared = costs_of(
            &item,
            std::slice::from_ref(&item),
            &runs_by_id(rows(), &HashMap::new()),
        );
        assert_eq!(
            undeclared.coverage.runs_unmatched,
            undeclared.coverage.runs_named
        );
        assert!(undeclared.coverage.nothing_matched());
        assert!(undeclared.stages.is_empty());
        assert!(undeclared.runs.is_empty());
    }

    #[test]
    fn a_global_pipeline_joins_every_agents_runs() {
        let item = record(
            "ORD-1",
            &[
                (T0, "implement", "claimed", Some("quills")),
                (T1, "review", "claimed", Some("nobodys")),
            ],
        );
        let billed = || vec![step("s", Some("claude-code:bedrock"), None, 1, 1.0)];
        let runs = joined(vec![
            Run {
                agent_id: Some("quill".into()),
                ..run("quills", T0, None, billed())
            },
            Run {
                agent_id: None,
                ..run("nobodys", T1, None, billed())
            },
        ]);
        let costs = item_costs(
            &def(),
            &item,
            std::slice::from_ref(&item),
            &runs,
            None,
            Utc::now(),
        );
        assert_eq!(costs.coverage.runs_costed, 2);
        assert_eq!(costs.coverage.runs_unmatched, 0);
        assert!(close(costs.total.billed_usd, 2.0));
    }

    #[test]
    fn stages_follow_first_appearance_and_take_the_column_label() {
        let item = record(
            "ORD-1",
            &[
                (T0, "implement", "claimed", Some("r-implement")),
                (T1, "merge", "gate", Some("r-merge")),
                (T2, "review", "claimed", Some("r-review")),
            ],
        );
        let billed = || vec![step("s", Some("claude-code:bedrock"), None, 1, 1.0)];
        let runs = joined(vec![
            run("r-implement", T2, None, billed()),
            run("r-merge", T1, None, billed()),
            run("r-review", T0, None, billed()),
        ]);
        let costs = costs_of(&item, std::slice::from_ref(&item), &runs);
        let stages: Vec<(&str, &str)> = costs
            .stages
            .iter()
            .map(|s| (s.stage.as_str(), s.label.as_str()))
            .collect();
        assert_eq!(
            stages,
            [
                ("implement", "Implementing"),
                ("merge", "merge"),
                ("review", "Reviewing")
            ]
        );
        assert_eq!(
            costs
                .runs
                .iter()
                .map(|r| r.run.as_str())
                .collect::<Vec<_>>(),
            ["r-review", "r-merge", "r-implement"],
            "runs are listed by when they started"
        );
        assert_eq!(costs.stages[0].workflows, ["order-intake"]);
    }

    #[test]
    fn tool_and_report_steps_add_no_model_rows() {
        let item = record("ORD-1", &[(T0, "implement", "claimed", Some("r1"))]);
        let runs = joined(vec![run(
            "r1",
            T0,
            None,
            vec![
                step("pick", Some(TOOL_ENGINE), None, 0, 0.0),
                step("post", Some(REPORT_ENGINE), None, 0, 0.0),
            ],
        )]);
        let costs = costs_of(&item, std::slice::from_ref(&item), &runs);
        assert!(costs.stages[0].models.is_empty());
        assert!(costs.runs[0]
            .steps
            .iter()
            .all(|step| step.billing.is_none()));
        assert_eq!(costs.coverage.steps_unrecorded, 0);
    }

    #[test]
    fn models_are_ordered_by_cost() {
        let item = record("ORD-1", &[(T0, "implement", "claimed", Some("r1"))]);
        let runs = joined(vec![run(
            "r1",
            T0,
            None,
            vec![
                step("a", Some("codex"), Some("gpt-6-astra"), 1, 0.5),
                step(
                    "b",
                    Some("claude-code:bedrock"),
                    Some("claude-sonnet-5"),
                    1,
                    0.5,
                ),
                step(
                    "c",
                    Some("claude-code:bedrock"),
                    Some("claude-opus-4-8"),
                    1,
                    3.0,
                ),
                step(
                    "d",
                    Some("claude-code:bedrock"),
                    Some("claude-haiku-5"),
                    1,
                    0.5,
                ),
            ],
        )]);
        let costs = costs_of(&item, std::slice::from_ref(&item), &runs);
        let order: Vec<(&str, &str)> = costs.stages[0]
            .models
            .iter()
            .map(|m| (m.engine.as_deref().unwrap(), m.model.as_deref().unwrap()))
            .collect();
        assert_eq!(
            order,
            [
                ("claude-code:bedrock", "claude-opus-4-8"),
                ("claude-code:bedrock", "claude-haiku-5"),
                ("claude-code:bedrock", "claude-sonnet-5"),
                ("codex", "gpt-6-astra"),
            ]
        );
    }

    #[test]
    fn a_running_run_is_costed_so_far() {
        let item = record("ORD-1", &[(T0, "implement", "claimed", Some("r1"))]);
        let runs = joined(vec![Run {
            status: "running".into(),
            ..run(
                "r1",
                T0,
                None,
                vec![step(
                    "implement",
                    Some("claude-code:bedrock"),
                    None,
                    4,
                    0.75,
                )],
            )
        }]);
        let costs = costs_of(&item, std::slice::from_ref(&item), &runs);
        assert_eq!(costs.runs[0].status, "running");
        assert!(close(costs.total.billed_usd, 0.75));
    }

    #[test]
    fn a_ticket_through_the_whole_pipeline_adds_up_exactly() {
        let item = record(
            "ORD-2797",
            &[
                (T0, "implement", "claimed", Some("r-implement")),
                (T1, "review", "claimed", Some("r-review")),
                (T2, "review", "claimed", Some("r-review-again")),
                (T3, "verify", "rung", Some("r-verify")),
            ],
        );
        let bedrock = Some("claude-code:bedrock");
        let runs = joined(vec![
            run(
                "r-implement",
                T0,
                Some("ORD-2797"),
                vec![
                    StepRecord {
                        provenance: StepProvenance {
                            engine: Some(TOOL_ENGINE.into()),
                            ..Default::default()
                        },
                        ..step("pick", None, None, 0, 0.0)
                    },
                    step("implement", bedrock, Some("claude-opus-4-8"), 42, 12.25),
                ],
            ),
            run(
                "r-review",
                T1,
                Some("ORD-2797"),
                vec![step("review", bedrock, Some("claude-sonnet-5"), 20, 1.5)],
            ),
            run(
                "r-review-again",
                T2,
                Some("ORD-2797"),
                vec![step("review", bedrock, Some("claude-sonnet-5"), 18, 2.25)],
            ),
            run(
                "r-verify",
                T3,
                Some("ORD-2797"),
                vec![step("verify", Some("codex"), Some("gpt-6-astra"), 9, 0.75)],
            ),
        ]);
        let costs = costs_of(&item, std::slice::from_ref(&item), &runs);

        assert_eq!(costs.total.billed_usd, 16.0);
        assert_eq!(costs.total.estimated_usd, 0.75);
        assert_eq!(costs.total.runs, 4);
        assert_eq!(costs.total.input_tokens, 4_000);
        assert_eq!(
            costs.stages.iter().map(|s| s.billed_usd).sum::<f64>(),
            costs.total.billed_usd
        );
        assert_eq!(
            costs.stages.iter().map(|s| s.estimated_usd).sum::<f64>(),
            costs.total.estimated_usd
        );
        for stage in &costs.stages {
            assert_eq!(
                stage.models.iter().map(|m| m.cost_usd).sum::<f64>(),
                stage.billed_usd + stage.estimated_usd,
                "{}",
                stage.stage
            );
        }
        let review = &costs.stages[1];
        assert_eq!((review.label.as_str(), review.runs), ("Reviewing", 2));
        assert_eq!(review.models.len(), 1);
        assert_eq!(
            (review.models[0].attempts, review.models[0].cost_usd),
            (2, 3.75)
        );
        assert_eq!(costs.stages[2].models[0].billing, Billing::Estimated);

        let json = serde_json::to_value(&costs).unwrap();
        assert_eq!(json["currency"], "USD");
        assert_eq!(json["stages"][2]["models"][0]["billing"], "estimated");
        assert_eq!(json["runs"][0]["steps"][1]["cost_usd"], 12.25);
    }
}
