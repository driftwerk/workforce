//! Gathering training examples from an install's own stores, for
//! `workforce decide gather`.
//!
//! Every example is the `workforce_decide::Input` the live path builds for
//! the same thing — a branch example through [`crate::decide::branch_input`]
//! from the definition revision the step ran under, a scope, routing or
//! `decide_first` example as the record of the request that was asked — so
//! the exporter's
//! one builder turns it into exactly the request a hub sends. Nothing here
//! shapes a state.
//!
//! The stores are opened read-only and never migrated: this runs on the
//! host beside a live hub, possibly from a newer binary.

use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use eyre::{Result, WrapErr};
use serde_json::{json, Value};
use workforce_agent::trace::TraceHeader;
use workforce_common::settings::WorkforceConfig;
use workforce_common::telemetry::TelemetryStore;
use workforce_decide::branch::NONE_OF_THESE;
use workforce_decide::export::{Example, Tags};
use workforce_decide::{Input, Use};
use workforce_orchestrator::workflow::definition::{StepDefinition, StepKind, WorkflowDefinition};
use workforce_orchestrator::workflow::engine::{
    declared_outcome, matching_branch, without_outcome,
};
use workforce_orchestrator::workflow::instance::{
    Actor, Chosen, Intervention, InterventionKind, OutcomeBy, StepRecord, WorkflowInstance,
};

use crate::decide::{branch_input, Produced};
use crate::decision_records::{DecisionRecords, Record};

/// What a label a person gave by hand counts for, beside 1 for any other:
/// a correction is the rarest and surest label there is.
pub const PERSON_WEIGHT: f64 = 3.0;

/// Runs read at a time: a run carries every response its steps wrote, so
/// the walk never holds more than a page of them.
const RUNS_PER_PAGE: usize = 100;

/// Records read at a time: each holds a whole request.
const RECORDS_PER_PAGE: usize = 500;

/// The use to gather examples for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gather {
    Branch,
    Scope,
    Routing,
    DecideFirst,
}

/// How many examples were written, and how many candidates were left out
/// for each reason.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Tally {
    pub examples: usize,
    pub left_out: BTreeMap<&'static str, usize>,
}

impl Tally {
    fn leave(&mut self, why: &'static str) {
        *self.left_out.entry(why).or_default() += 1;
    }
}

/// A definition a step ran under, and whether it is the very revision
/// (`false`: the run recorded none, so the definition as it stands today).
type Resolved = Result<(Arc<WorkflowDefinition>, bool), &'static str>;

/// The step's response as the fallback is asked about one: without the
/// `OUTCOME:` line when that names a declared branch, since a step that
/// wrote one is never asked about and the line would give the label away.
fn concluded(step: &StepDefinition, response: &str) -> String {
    match declared_outcome(step, response) {
        Some(_) => without_outcome(response),
        None => response.to_string(),
    }
}

/// The completed run of `step_id` that someone stepping in at `at` acted
/// on: the last one before then, with its index.
fn acted_on<'a>(
    instance: &'a WorkflowInstance,
    step_id: &str,
    at: DateTime<Utc>,
) -> Option<(usize, &'a StepRecord)> {
    instance
        .step_history
        .iter()
        .enumerate()
        .rev()
        .find(|(_, r)| !r.failed && r.step_id == step_id && r.completed_at <= at)
}

/// The records whose step went on with the outcome a person resumed it
/// with, by index. A resume kept before it said whether it took effect
/// counts when the branch its outcome names is the one its record holds.
fn resumed_by_person(
    instance: &WorkflowInstance,
    interventions: &[Intervention],
    definition: &dyn Fn(&StepRecord) -> Resolved,
) -> HashSet<usize> {
    interventions
        .iter()
        .filter(|i| i.kind == InterventionKind::Resume && matches!(i.actor, Actor::Person { .. }))
        .filter_map(|resume| {
            let chosen = resume
                .chosen
                .as_ref()
                .filter(|chosen| at_its_step(resume, chosen))?;
            let (index, record) = acted_on(instance, &chosen.step_id, resume.at)?;
            let took_effect = match resume.applied {
                Some(applied) => applied,
                None => {
                    let (def, _) = definition(record).ok()?;
                    let step = def.get_step(&record.step_id)?;
                    let named = matching_branch(step, resume.outcome.as_deref()?)?;
                    record.outcome.as_ref() == Some(&named.when)
                }
            };
            took_effect.then_some(index)
        })
        .collect()
}

/// Whether `intervention` was made at the step `chosen` ran: one made at a
/// step that had not run yet acted on no run of the one before it.
fn at_its_step(intervention: &Intervention, chosen: &Chosen) -> bool {
    intervention.from_step.as_deref() == Some(chosen.step_id.as_str())
}

/// The records, by index, that hold an outcome a resume gave at a later
/// step: a build before a resume's outcome went to the step the run stood
/// at set it on the last completed run of whichever step came before.
fn misplaced_by_resume(
    instance: &WorkflowInstance,
    interventions: &[Intervention],
) -> HashSet<usize> {
    interventions
        .iter()
        .filter(|i| i.kind == InterventionKind::Resume)
        .filter_map(|resume| {
            let chosen = resume
                .chosen
                .as_ref()
                .filter(|chosen| !at_its_step(resume, chosen))?;
            let (index, record) = acted_on(instance, &chosen.step_id, resume.at)?;
            let holds_it = record.provenance.outcome_by == Some(OutcomeBy::Resume)
                && record.outcome.is_some()
                && record.outcome == resume.outcome;
            let changed = (record.outcome.as_ref(), record.provenance.outcome_by)
                != (chosen.outcome.as_ref(), chosen.by);
            (holds_it && changed).then_some(index)
        })
        .collect()
}

/// The label `record` teaches for `step`, who gave it and what it counts
/// for; or why it teaches none. `by_person` says a person's resume routed it.
fn label_of(
    step: &StepDefinition,
    record: &StepRecord,
    by_person: bool,
) -> Result<(String, &'static str, f64), &'static str> {
    let outcome = || record.outcome.clone().ok_or("no outcome recorded");
    match record.provenance.outcome_by {
        Some(OutcomeBy::OutcomeLine) => Ok((outcome()?, "agent", 1.0)),
        Some(OutcomeBy::Resume) if by_person => Ok((outcome()?, "person", PERSON_WEIGHT)),
        Some(OutcomeBy::Resume) => step
            .branches
            .iter()
            .find(|branch| record.outcome.as_ref() == Some(&branch.when))
            .map(|branch| (branch.when.clone(), "resume", 1.0))
            .ok_or("a resume that named no branch"),
        Some(OutcomeBy::Default) => Ok((NONE_OF_THESE.to_string(), "default", 1.0)),
        Some(OutcomeBy::IterationCap) => Err("outcome forced by the iteration cap"),
        Some(OutcomeBy::DecisionModel) => Err("the decision model's own pick"),
        // A run from before outcomes were recorded: the agent's own line
        // still says what it chose, and nothing says what a step without
        // one went on to do.
        None => step
            .pause
            .is_none()
            .then(|| declared_outcome(step, &record.agent_response))
            .flatten()
            .map(|label| (label, "agent", 1.0))
            .ok_or("no outcome recorded"),
    }
}

/// What a person sending the run to `to` says `step` should have chosen:
/// the one branch that leads there, or `none-of-these` when only the step's
/// default route does.
fn corrected(def: &WorkflowDefinition, step: &StepDefinition, to: &str) -> Option<String> {
    let mut leading: Vec<&str> = step
        .branches
        .iter()
        .filter(|branch| branch.goto == to)
        .map(|branch| branch.when.as_str())
        .collect();
    leading.dedup();
    let default = step.goto.clone().or_else(|| {
        let next = def.step_index(&step.id)? + 1;
        def.steps.get(next).map(|next| next.id.clone())
    });
    match leading[..] {
        [label] => Some(label.to_string()),
        [] if default.as_deref() == Some(to) => Some(NONE_OF_THESE.to_string()),
        _ => None,
    }
}

/// The records a person corrected by sending the run elsewhere, each with
/// the label that says so.
fn corrections(
    instance: &WorkflowInstance,
    interventions: &[Intervention],
    definition: &dyn Fn(&StepRecord) -> Resolved,
    tally: &mut Tally,
) -> BTreeMap<usize, String> {
    interventions
        .iter()
        .filter(|i| i.kind == InterventionKind::Goto && matches!(i.actor, Actor::Person { .. }))
        .filter_map(|goto| {
            let (chosen, to) = (goto.chosen.as_ref()?, goto.to_step.as_deref()?);
            let (index, record) = acted_on(instance, &chosen.step_id, goto.at)?;
            let (def, _) = definition(record).ok()?;
            let step = def.get_step(&record.step_id)?;
            if step.branches.is_empty() || !matches!(step.kind(), StepKind::Agent) {
                return None;
            }
            let Some(label) = corrected(&def, step, to) else {
                tally.leave("sent to a step no one branch leads to");
                return None;
            };
            let routed = chosen.outcome.as_deref().unwrap_or(NONE_OF_THESE);
            (label != routed).then_some((index, label))
        })
        .collect()
}

/// The branch examples of one run: every completed run of an agent step
/// with branches, since `since`, labelled with what it chose — or with what
/// a person said it should have, by the `interventions` kept on the run.
pub fn branch_examples(
    instance: &WorkflowInstance,
    interventions: &[Intervention],
    since: DateTime<Utc>,
    definition: &dyn Fn(&StepRecord) -> Resolved,
    tally: &mut Tally,
) -> Vec<Example> {
    let corrections = corrections(instance, interventions, definition, tally);
    let by_person = resumed_by_person(instance, interventions, definition);
    let misplaced = misplaced_by_resume(instance, interventions);
    instance
        .step_history
        .iter()
        .enumerate()
        .filter(|(_, record)| !record.failed && record.completed_at >= since)
        .filter_map(|(index, record)| {
            let (def, exact) = match definition(record) {
                Ok(resolved) => resolved,
                Err(why) => {
                    tally.leave(why);
                    return None;
                }
            };
            let Some(step) = def.get_step(&record.step_id) else {
                tally.leave("step no longer in the definition");
                return None;
            };
            if step.branches.is_empty() || !matches!(step.kind(), StepKind::Agent) {
                return None;
            }
            let labelled = match corrections.get(&index) {
                Some(label) => Ok((label.clone(), "correction", PERSON_WEIGHT)),
                None if misplaced.contains(&index) => {
                    Err("a resume's outcome given at a later step")
                }
                None => label_of(step, record, by_person.contains(&index)),
            };
            let (label, source, weight) = match labelled {
                Ok(labelled) => labelled,
                Err(why) => {
                    tally.leave(why);
                    return None;
                }
            };
            let response = concluded(step, &record.agent_response);
            let produced = Produced {
                response: &response,
                outputs: record.outputs.as_ref(),
                report: record.report.as_deref(),
            };
            Some(Example {
                input: branch_input(&def, step, &produced),
                label: Value::String(label),
                tags: Tags {
                    id: Some(format!("branch:{}:{index}", instance.id)),
                    source: Some(source.to_string()),
                    weight: Some(weight),
                    day: Some(record.completed_at.format("%Y-%m-%d").to_string()),
                    meta: json!({
                        "workflow": instance.workflow_id,
                        "instance": instance.id,
                        "step": record.step_id,
                        "definition": if exact { "revision" } else { "current" },
                        "model": record.provenance.model,
                        "decision": record.provenance.branch_decided_by,
                    }),
                },
            })
        })
        .collect()
}

fn of_record(record: &Record, label: Value, source: Option<&str>) -> Example {
    Example {
        input: record.record.input.clone(),
        label,
        tags: Tags {
            id: Some(format!("{}:{}", record.record.usage, record.id)),
            source: source.map(str::to_string),
            weight: Some(1.0),
            day: Some(record.at.format("%Y-%m-%d").to_string()),
            meta: json!({
                "origin": "record",
                "agent": record.record.agent_id,
                "channel": record.record.channel,
                "backend": record.record.backend,
                "model": record.record.model,
                "acted": record.record.acted,
                "request_chars": record.record.request_chars,
                "model_answers": record.record.answers,
            }),
        },
    }
}

/// A scope example: the request as the gate asked it, labelled with what
/// the LLM classifier said beside the model. A message only the model
/// judged has no label: its own verdict is never one.
pub fn scope_example(record: &Record) -> Example {
    let label = match record.record.verdict.as_deref() {
        Some("on_topic") => Value::Bool(true),
        Some("off_topic") => Value::Bool(false),
        _ => Value::Null,
    };
    let source = (!label.is_null()).then_some("classifier");
    of_record(record, label, source)
}

/// A `decide_first` example: the state and question a step was asked
/// before its turn, labelled with the outcome its agent's turn then chose.
/// A step the model settled ran no turn, and the model's own pick is never
/// a label; nor is a turn that chose nothing.
pub fn decide_first_example(record: &Record) -> Example {
    let label = match (&record.record.verdict, record.record.acted) {
        (Some(chose), false) => Value::String(chose.clone()),
        _ => Value::Null,
    };
    let source = (!label.is_null()).then_some("agent");
    of_record(record, label, source)
}

/// A routing example: nothing beside the model says what kind of work a
/// request was, so it waits for a label.
pub fn routing_example(record: &Record) -> Example {
    of_record(record, Value::Null, None)
}

/// A routing example out of a turn's trace header: what a person asked, of
/// which agent, on what kind of channel. Unlabelled. `None` for a header
/// with no request: a workflow step, a background run, or a trace written
/// before headers recorded it.
pub fn trace_example(header: &TraceHeader) -> Option<Example> {
    let message = header.request.clone()?;
    Some(Example {
        input: Input::Routing {
            message,
            agent: Some(header.agent_id.clone()),
            channel: header.channel.clone(),
        },
        label: Value::Null,
        tags: Tags {
            id: Some(format!("trace:{}", header.trace_id)),
            source: None,
            weight: Some(1.0),
            day: Some(header.started_at.format("%Y-%m-%d").to_string()),
            // Not the session: its key names the channel or the person.
            meta: json!({
                "origin": "trace",
                "task_type": header.task_type,
            }),
        },
    })
}

/// The `{agent}/{YYYY-MM-DD}` trace files written on or after `since`'s day,
/// oldest day first, gzipped or not.
fn trace_files(traces: &Path, since: DateTime<Utc>) -> Vec<PathBuf> {
    let entries = |dir: &Path| {
        std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| entry.path())
            .collect::<Vec<_>>()
    };
    let day_of = |dir: &Path| {
        let name = dir.file_name()?.to_str()?;
        chrono::NaiveDate::parse_from_str(name, "%Y-%m-%d").ok()
    };
    let mut days: Vec<(chrono::NaiveDate, PathBuf)> = entries(traces)
        .iter()
        .flat_map(|agent| entries(agent))
        .filter_map(|day| Some((day_of(&day)?, day)))
        .filter(|(day, _)| *day >= since.date_naive())
        .collect();
    days.sort();
    let is_trace = |file: &PathBuf| {
        let name = file.file_name().and_then(|name| name.to_str());
        name.is_some_and(|name| name.ends_with(".jsonl") || name.ends_with(".jsonl.gz"))
    };
    days.iter()
        .flat_map(|(_, day)| {
            let mut files = entries(day);
            files.sort();
            files
        })
        .filter(is_trace)
        .collect()
}

/// The headers in one trace file, read one at a time: a header carries its
/// run's whole system prompt and tools, and a file holds many runs' lines
/// interleaved. Only the header lines are parsed.
fn headers(file: &Path) -> Result<impl Iterator<Item = TraceHeader>> {
    let opened = std::fs::File::open(file).wrap_err_with(|| format!("open {}", file.display()))?;
    let gzipped = file.extension().is_some_and(|ext| ext == "gz");
    let reader: Box<dyn Read> = match gzipped {
        true => Box::new(flate2::read::GzDecoder::new(opened)),
        false => Box::new(opened),
    };
    Ok(BufReader::new(reader)
        .lines()
        .map_while(std::result::Result::ok)
        .filter(|line| line.starts_with(r#"{"type":"header""#))
        .filter_map(|line| serde_json::from_str(&line).ok()))
}

/// The install's stores, as an offline reader opens them.
pub struct Stores {
    telemetry: Arc<dyn TelemetryStore>,
    #[cfg(feature = "postgres")]
    definitions: Option<Arc<workforce_orchestrator::workflow_store::WorkflowStore>>,
    workflows_dir: PathBuf,
    records: Option<Arc<dyn DecisionRecords>>,
    traces_dir: PathBuf,
}

impl Stores {
    /// Open what `config` says the install uses — Postgres or its files —
    /// read-only, running no migration.
    pub fn read_only(config: &WorkforceConfig, workflows_dir: PathBuf) -> Result<Self> {
        let traces_dir = crate::trace_writer::traces_dir(config);
        #[cfg(feature = "postgres")]
        if let Some(pg) = config.postgres.as_ref().filter(|pg| pg.enabled) {
            use workforce_common::telemetry_postgres::PostgresTelemetryStore;
            let telemetry: Arc<dyn TelemetryStore> = match config.telemetry.backend.as_str() {
                "postgres" => PostgresTelemetryStore::read_only(pg)?,
                _ => Self::sqlite(config)?,
            };
            return Ok(Self {
                telemetry,
                definitions: Some(
                    workforce_orchestrator::workflow_store::WorkflowStore::read_only(
                        PostgresTelemetryStore::read_only_pool(pg)?,
                    ),
                ),
                workflows_dir,
                records: Some(Arc::new(
                    crate::decision_store::postgres::PostgresDecisionRecords::read_only(pg)?,
                )),
                traces_dir,
            });
        }
        Ok(Self {
            telemetry: Self::sqlite(config)?,
            #[cfg(feature = "postgres")]
            definitions: None,
            workflows_dir,
            records: None,
            traces_dir,
        })
    }

    fn sqlite(config: &WorkforceConfig) -> Result<Arc<dyn TelemetryStore>> {
        let path = Path::new(&config.hub.data_dir).join("telemetry.db");
        let store = workforce_common::persistence::SqliteStore::open_read_only(&path)
            .wrap_err_with(|| format!("open {} read-only", path.display()))?;
        Ok(Arc::new(store))
    }

    /// The text of the revision whose hash is `hash`, while it is kept.
    async fn revision(&self, hash: &str) -> Result<Option<String>> {
        #[cfg(feature = "postgres")]
        if let Some(store) = &self.definitions {
            return store
                .revision_content(workforce_orchestrator::workflow_store::KIND_WORKFLOW, hash)
                .await;
        }
        let _ = hash;
        Ok(None)
    }

    /// The text of the definition `key` names, as it stands today.
    async fn current(&self, key: &str) -> Result<Option<String>> {
        #[cfg(feature = "postgres")]
        if let Some(store) = &self.definitions {
            let (owner, id) = match key.split_once('/') {
                Some((owner, id)) => (Some(owner), id),
                None => (None, key),
            };
            let kind = workforce_orchestrator::workflow_store::KIND_WORKFLOW;
            if let Some(source) = store.get(id, kind, owner).await? {
                return Ok(Some(source));
            }
        }
        let on_disk = std::fs::read_dir(&self.workflows_dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "toml"))
            .filter_map(|entry| std::fs::read_to_string(entry.path()).ok())
            .find(|source| {
                WorkflowDefinition::from_toml(source).is_ok_and(|def| def.workflow.id == key)
            });
        Ok(on_disk)
    }

    /// The definition a step of the run `key` ran under: the revision it
    /// recorded, or — for a run that recorded none — today's.
    async fn definition(&self, key: &str, hash: Option<&str>) -> Result<Resolved> {
        let parsed = |source: String, exact: bool| match WorkflowDefinition::from_toml(&source) {
            Ok(def) => Ok((Arc::new(def), exact)),
            Err(_) => Err("definition no longer parses"),
        };
        let current = self.current(key).await?;
        Ok(match hash {
            None => match current {
                Some(source) => parsed(source, false),
                None => Err("definition not found"),
            },
            Some(hash) => match self.revision(hash).await? {
                Some(source) => parsed(source, true),
                None => match current.filter(|source| {
                    workforce_orchestrator::workflow_store::content_hash(source) == hash
                }) {
                    Some(source) => parsed(source, true),
                    None => Err("definition revision no longer kept"),
                },
            },
        })
    }

    /// Hand `keep` an example of every record of `usage` since `since`, a
    /// page at a time; whether there is room for more afterwards.
    async fn records_of(
        &self,
        usage: Use,
        since: DateTime<Utc>,
        example: fn(&Record) -> Example,
        tally: &mut Tally,
        keep: &mut impl FnMut(Vec<Example>, &mut Tally) -> Result<bool>,
    ) -> Result<bool> {
        let records = self.records()?;
        let mut after = 0;
        loop {
            let page = records
                .list(usage.as_str(), since, after, RECORDS_PER_PAGE)
                .await?;
            let Some(last) = page.last() else {
                return Ok(true);
            };
            after = last.id;
            if !keep(page.iter().map(example).collect(), tally)? {
                return Ok(false);
            }
        }
    }

    fn records(&self) -> Result<&Arc<dyn DecisionRecords>> {
        self.records.as_ref().ok_or_else(|| {
            eyre::eyre!("decision records are kept in Postgres, and this install has none")
        })
    }

    /// Gather up to `limit` examples for `gather` recorded since `since`,
    /// oldest first, handing each to `emit`.
    pub async fn gather(
        &self,
        gather: Gather,
        since: DateTime<Utc>,
        limit: usize,
        mut emit: impl FnMut(&Example) -> Result<()>,
    ) -> Result<Tally> {
        let mut tally = Tally::default();
        let mut keep = |examples: Vec<Example>, tally: &mut Tally| -> Result<bool> {
            for example in &examples {
                if tally.examples == limit {
                    return Ok(false);
                }
                emit(example)?;
                tally.examples += 1;
            }
            Ok(tally.examples < limit)
        };
        match gather {
            Gather::Branch => {
                let mut known: HashMap<(String, Option<String>), Resolved> = HashMap::new();
                let (mut after, mut after_id) = (since, String::new());
                let mut seen: HashSet<String> = HashSet::new();
                'walk: loop {
                    let page =
                        self.telemetry
                            .workflow_instances_after(after, &after_id, RUNS_PER_PAGE)?;
                    let Some(last) = page.last() else { break };
                    after_id = last.id.clone();
                    after = DateTime::parse_from_rfc3339(&last.updated_at)
                        .wrap_err("a run's update time")?
                        .with_timezone(&Utc);
                    // A run the hub moved while this walk was under way
                    // comes round again behind the cursor: once is enough.
                    let page: Vec<_> = page
                        .into_iter()
                        .filter(|row| seen.insert(row.id.clone()))
                        .collect();
                    let ids: Vec<String> = page.iter().map(|row| row.id.clone()).collect();
                    let mut interventions = match &self.records {
                        Some(records) => records.interventions(&ids).await?,
                        None => BTreeMap::new(),
                    };
                    for row in page {
                        let Ok(instance) = crate::workflow_runtime::instance_of(row) else {
                            tally.leave("run no longer reads");
                            continue;
                        };
                        for record in &instance.step_history {
                            let hash = record.provenance.definition_hash.clone();
                            let key = (instance.workflow_id.clone(), hash);
                            if let Entry::Vacant(unknown) = known.entry(key) {
                                let (workflow, hash) = unknown.key();
                                let resolved = self.definition(workflow, hash.as_deref()).await?;
                                unknown.insert(resolved);
                            }
                        }
                        let definition = |record: &StepRecord| {
                            let hash = record.provenance.definition_hash.clone();
                            known[&(instance.workflow_id.clone(), hash)].clone()
                        };
                        let moved = interventions.remove(&instance.id).unwrap_or_default();
                        let examples =
                            branch_examples(&instance, &moved, since, &definition, &mut tally);
                        if !keep(examples, &mut tally)? {
                            break 'walk;
                        }
                    }
                }
            }
            Gather::Scope => {
                self.records_of(Use::ScopeGate, since, scope_example, &mut tally, &mut keep)
                    .await?;
            }
            Gather::DecideFirst => {
                self.records_of(
                    Use::DecideFirst,
                    since,
                    decide_first_example,
                    &mut tally,
                    &mut keep,
                )
                .await?;
            }
            Gather::Routing => {
                let mut more = self
                    .records_of(Use::Routing, since, routing_example, &mut tally, &mut keep)
                    .await?;
                for file in trace_files(&self.traces_dir, since) {
                    if !more {
                        break;
                    }
                    let examples = headers(&file)?
                        .filter(|header| header.started_at >= since)
                        .filter_map(|header| {
                            let example = trace_example(&header);
                            if example.is_none() {
                                tally.leave("no request on the trace");
                            }
                            example
                        })
                        .collect();
                    more = keep(examples, &mut tally)?;
                }
            }
        }
        Ok(tally)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decide::testing::{config_for, recording_backend};
    use crate::decision_records::testing::{decision, recorded};
    use crate::decision_records::{NewRecord, Subject};
    use std::time::Duration;
    use workforce_common::settings::{BranchFallbackConfig, DecisionConfig};
    use workforce_decide::export::{self, LeftOut};
    use workforce_decide::RequestBudget;
    use workforce_orchestrator::workflow::engine::{
        went_on_with_resumed_outcome, WorkflowExecutor,
    };
    use workforce_orchestrator::workflow::instance::{
        InstanceStatus, StepProvenance, TriggerInfo, TriggerSource,
    };

    const TRIAGE: &str = r#"
[workflow]
id = "triage"
name = "Triage"

[[step]]
id = "classify"
name = "Classify the ticket"
task = """
Read the ticket.
A crash or wrong result: OUTCOME: bug
"""
branches = [{ when = "bug", goto = "fix" }]
goto = "reply"

[[step]]
id = "review"
name = "Review the fix"
task = """
Review it.
Every check passes: OUTCOME: approved
Anything is missing: OUTCOME: changes-requested
"""
outputs = [{ name = "summary" }]
branches = [{ when = "approved", goto = "reply" }, { when = "changes-requested", goto = "fix" }]

[[step]]
id = "gate"
name = "Sign off"
task = "Summarise for sign-off."
pause = "Waiting for sign-off"
branches = [{ when = "approved", goto = "reply" }, { when = "rejected", goto = "fix" }]

[[step]]
id = "fix"
name = "Fix it"
task = "Fix it."

[[step]]
id = "reply"
name = "Reply"
task = "Reply."
exit = true
"#;

    fn def() -> Arc<WorkflowDefinition> {
        Arc::new(WorkflowDefinition::from_toml(TRIAGE).unwrap())
    }

    fn run() -> WorkflowInstance {
        let trigger = TriggerInfo {
            source: TriggerSource::Cli,
            payload: json!({}),
        };
        WorkflowInstance::new("triage", trigger, "classify", None)
    }

    fn epoch() -> DateTime<Utc> {
        DateTime::<Utc>::UNIX_EPOCH
    }

    /// Run `step` to `response` and route it as the engine does.
    fn complete(instance: &mut WorkflowInstance, step: &str, response: &str) {
        instance.current_step_id = Some(step.into());
        instance.record_step(step, None, response, Utc::now(), 1, 0.0);
        WorkflowExecutor::complete_step(&def(), instance, response).unwrap();
    }

    fn gathered(instance: &WorkflowInstance) -> (Vec<Example>, Tally) {
        gathered_with(instance, &[])
    }

    fn gathered_with(instance: &WorkflowInstance, moved: &[Intervention]) -> (Vec<Example>, Tally) {
        let mut tally = Tally::default();
        let definition = |_: &StepRecord| -> Resolved { Ok((def(), true)) };
        let examples = branch_examples(instance, moved, epoch(), &definition, &mut tally);
        (examples, tally)
    }

    /// `intervention` as the hub keeps it once the run has moved.
    fn kept_on(instance: &WorkflowInstance, intervention: Intervention) -> [Intervention; 1] {
        [Intervention {
            to_step: instance.current_step_id.clone(),
            ..intervention
        }]
    }

    /// Resume the run at its step with `outcome` by `actor` and route it
    /// as the hub does, when the step has run: the intervention the hub
    /// keeps.
    fn resumed(instance: &mut WorkflowInstance, actor: Actor, outcome: &str) -> Intervention {
        let resume = instance.intervention(InterventionKind::Resume, actor);
        instance.status = InstanceStatus::Paused;
        WorkflowExecutor::resume(instance, Some(outcome)).unwrap();
        let step = instance.current_step_id.clone().unwrap();
        let response = instance
            .last_record_of(&step)
            .map(|record| record.agent_response.clone());
        let applied = response.is_some_and(|response| {
            let transition = WorkflowExecutor::complete_step(&def(), instance, &response).unwrap();
            went_on_with_resumed_outcome(instance, &step, &transition)
        });
        Intervention {
            outcome: Some(outcome.into()),
            applied: Some(applied),
            to_step: instance.current_step_id.clone(),
            ..resume
        }
    }

    fn person() -> Actor {
        Actor::Person { id: "alice".into() }
    }

    fn response_of(example: &Example) -> &str {
        match &example.input {
            Input::BranchFallback { result, .. } => &result.response,
            other => panic!("a branch input expected, got {other:?}"),
        }
    }

    #[test]
    fn an_agents_own_outcome_is_the_label_and_its_line_is_kept_out_of_the_state() {
        let mut instance = run();
        complete(
            &mut instance,
            "classify",
            "It crashes on save.\nOUTCOME: bug",
        );
        complete(&mut instance, "fix", "Fixed.");
        let (examples, tally) = gathered(&instance);
        assert_eq!(examples.len(), 1, "a step with no branches is no example");
        assert_eq!(tally.left_out, BTreeMap::new());
        let example = &examples[0];
        assert_eq!(example.label, "bug");
        assert_eq!(example.tags.source.as_deref(), Some("agent"));
        assert_eq!(example.tags.weight, Some(1.0));
        assert_eq!(
            example.tags.id.as_deref(),
            Some(format!("branch:{}:0", instance.id).as_str())
        );
        assert_eq!(response_of(example), "It crashes on save.");
        assert_eq!(example.tags.meta["step"], "classify");
        let record = export::record(example, RequestBudget::DEFAULT).unwrap();
        assert!(
            !record.state.to_string().contains("OUTCOME"),
            "the label never rides in the state: {}",
            record.state
        );
    }

    #[test]
    fn a_default_route_is_none_of_these_and_keeps_the_line_it_wrote() {
        let mut instance = run();
        complete(&mut instance, "classify", "Just spam.\nOUTCOME: ignored");
        let (examples, _) = gathered(&instance);
        assert_eq!(examples[0].label, NONE_OF_THESE);
        assert_eq!(examples[0].tags.source.as_deref(), Some("default"));
        assert_eq!(response_of(&examples[0]), "Just spam.\nOUTCOME: ignored");
    }

    #[test]
    fn a_persons_outcome_at_a_gate_counts_for_more_and_an_agents_resume_does_not() {
        for (actor, source, weight) in [
            (person(), "person", PERSON_WEIGHT),
            (Actor::Agent { id: "ops".into() }, "resume", 1.0),
        ] {
            let mut instance = run();
            complete(
                &mut instance,
                "gate",
                "All checks pass; ready for sign-off.",
            );
            let resume = resumed(&mut instance, actor, "rejected");
            let (examples, _) = gathered_with(&instance, &[resume]);
            assert_eq!(examples.len(), 1);
            assert_eq!(examples[0].label, "rejected");
            assert_eq!(examples[0].tags.source.as_deref(), Some(source));
            assert_eq!(examples[0].tags.weight, Some(weight));
        }
    }

    #[test]
    fn a_persons_resume_labels_the_branch_its_outcome_names_when_the_step_went_on_with_it() {
        let mut instance = run();
        complete(&mut instance, "review", "Looks fine.\nOUTCOME: aproved");
        let resume = resumed(&mut instance, person(), "approved — fine");
        assert_eq!(instance.current_step_id.as_deref(), Some("reply"));
        let kept_before = Intervention {
            applied: None,
            ..resume.clone()
        };
        for kept in [resume, kept_before] {
            let (examples, _) = gathered_with(&instance, &[kept]);
            assert_eq!(
                (
                    examples[0].label.clone(),
                    examples[0].tags.source.as_deref(),
                    examples[0].tags.weight
                ),
                (json!("approved"), Some("person"), Some(PERSON_WEIGHT))
            );
        }
    }

    #[test]
    fn a_persons_resume_that_names_no_branch_labels_nothing() {
        let mut halted = run();
        complete(&mut halted, "review", "Looks fine.\nOUTCOME: aproved");
        let resume = resumed(&mut halted, person(), "maybe");
        assert_eq!(resume.applied, Some(false));
        let kept_before = Intervention {
            applied: None,
            ..resume.clone()
        };
        for kept in [resume, kept_before] {
            let (examples, tally) = gathered_with(&halted, &[kept]);
            assert!(examples.is_empty(), "{examples:?}");
            assert_eq!(
                tally.left_out,
                BTreeMap::from([("a resume that named no branch", 1)])
            );
        }
    }

    #[test]
    fn a_persons_resume_labels_the_run_of_the_step_it_resumed_and_no_later_one() {
        let at = |minute| epoch() + chrono::Duration::minutes(minute);
        let mut instance = run();
        complete(&mut instance, "review", "Hm.\nOUTCOME: aproved");
        instance.step_history[0].completed_at = at(1);
        let by_alice = Intervention {
            at: at(2),
            ..resumed(&mut instance, person(), "changes-requested")
        };
        complete(&mut instance, "review", "Hm again.\nOUTCOME: aproved");
        instance.step_history[1].completed_at = at(3);
        let by_ops = Intervention {
            at: at(4),
            ..resumed(
                &mut instance,
                Actor::Agent { id: "ops".into() },
                "changes-requested",
            )
        };
        let (examples, _) = gathered_with(&instance, &[by_alice, by_ops]);
        assert_eq!(
            examples
                .iter()
                .map(|e| (e.label.clone(), e.tags.source.as_deref()))
                .collect::<Vec<_>>(),
            [
                (json!("changes-requested"), Some("person")),
                (json!("changes-requested"), Some("resume"))
            ]
        );
    }

    #[test]
    fn an_outcome_an_earlier_build_set_on_the_step_before_the_one_resumed_labels_nothing() {
        let mut instance = run();
        complete(&mut instance, "review", "Looks fine.\nOUTCOME: approved");
        assert_eq!(instance.current_step_id.as_deref(), Some("reply"));
        let resume = Intervention {
            outcome: Some("changes-requested".into()),
            applied: Some(false),
            ..instance.intervention(InterventionKind::Resume, person())
        };
        let review = instance.step_history.last_mut().unwrap();
        review.outcome = resume.outcome.clone();
        review.provenance.outcome_by = Some(OutcomeBy::Resume);
        let kept_before = Intervention {
            applied: None,
            ..resume.clone()
        };
        for kept in [resume, kept_before] {
            let (examples, tally) = gathered_with(&instance, &kept_on(&instance, kept));
            assert!(examples.is_empty(), "{examples:?}");
            assert_eq!(
                tally.left_out,
                BTreeMap::from([("a resume's outcome given at a later step", 1)])
            );
        }
    }

    #[test]
    fn a_resume_at_a_step_that_has_not_run_leaves_the_label_of_the_step_before() {
        let mut instance = run();
        complete(&mut instance, "review", "Looks fine.\nOUTCOME: approved");
        let at_reply = resumed(&mut instance, person(), "changes-requested");
        assert_eq!(at_reply.applied, Some(false));
        let (examples, _) = gathered_with(&instance, &[at_reply]);
        assert_eq!(
            examples
                .iter()
                .map(|e| (e.label.clone(), e.tags.source.as_deref()))
                .collect::<Vec<_>>(),
            [(json!("approved"), Some("agent"))]
        );

        let mut instance = run();
        complete(&mut instance, "review", "Hm.\nOUTCOME: aproved");
        let by_alice = resumed(&mut instance, person(), "changes-requested");
        instance.record_failed_attempt(
            "fix",
            "boom",
            Utc::now(),
            1,
            0.0,
            StepProvenance::default(),
        );
        let at_fix = resumed(&mut instance, person(), "changes-requested");
        let (examples, tally) = gathered_with(&instance, &[by_alice, at_fix]);
        assert_eq!(
            examples
                .iter()
                .map(|e| (e.label.clone(), e.tags.source.as_deref()))
                .collect::<Vec<_>>(),
            [(json!("changes-requested"), Some("person"))],
            "a person's resume behind a failed attempt still labels its step: {tally:?}"
        );
    }

    #[test]
    fn a_persons_outcome_at_the_gate_they_were_shown_stays_their_label_past_a_later_resume() {
        let mut instance = run();
        complete(
            &mut instance,
            "gate",
            "All checks pass; ready for sign-off.",
        );
        let at_gate = resumed(&mut instance, person(), "rejected");
        assert_eq!(
            (at_gate.applied, instance.current_step_id.as_deref()),
            (Some(true), Some("fix"))
        );
        let at_fix = resumed(&mut instance, person(), "rejected");
        assert_eq!(at_fix.applied, Some(false));
        let kept_before = |resume: &Intervention| Intervention {
            applied: None,
            ..resume.clone()
        };
        for kept in [
            [at_gate.clone(), at_fix.clone()],
            [kept_before(&at_gate), kept_before(&at_fix)],
        ] {
            let (examples, tally) = gathered_with(&instance, &kept);
            assert_eq!(
                examples
                    .iter()
                    .map(|e| (e.label.clone(), e.tags.source.as_deref(), e.tags.weight))
                    .collect::<Vec<_>>(),
                [(json!("rejected"), Some("person"), Some(PERSON_WEIGHT))]
            );
            assert_eq!(tally.left_out, BTreeMap::new());
        }
    }

    #[test]
    fn a_persons_resume_at_a_later_step_is_not_their_label_for_the_gate_before() {
        let mut instance = run();
        complete(
            &mut instance,
            "gate",
            "All checks pass; ready for sign-off.",
        );
        let by_ops = resumed(&mut instance, Actor::Agent { id: "ops".into() }, "rejected");
        let at_fix = Intervention {
            applied: None,
            ..resumed(&mut instance, person(), "rejected")
        };
        let (examples, _) = gathered_with(&instance, &[by_ops, at_fix]);
        assert_eq!(
            examples
                .iter()
                .map(|e| (e.label.clone(), e.tags.source.as_deref()))
                .collect::<Vec<_>>(),
            [(json!("rejected"), Some("resume"))]
        );
    }

    #[test]
    fn a_person_sending_the_run_elsewhere_corrects_the_step_before() {
        let mut instance = run();
        complete(&mut instance, "review", "Looks fine.\nOUTCOME: approved");
        assert_eq!(instance.current_step_id.as_deref(), Some("reply"));
        let goto = instance.intervention(InterventionKind::Goto, person());
        instance.current_step_id = Some("fix".into());
        let (examples, tally) = gathered_with(&instance, &kept_on(&instance, goto));
        assert_eq!(examples.len(), 1);
        assert_eq!(examples[0].label, "changes-requested");
        assert_eq!(examples[0].tags.source.as_deref(), Some("correction"));
        assert_eq!(examples[0].tags.weight, Some(PERSON_WEIGHT));
        assert_eq!(response_of(&examples[0]), "Looks fine.");
        assert_eq!(tally.left_out, BTreeMap::new());

        let mut same = run();
        complete(&mut same, "review", "Looks fine.\nOUTCOME: approved");
        let again = same.intervention(InterventionKind::Goto, person());
        let (examples, _) = gathered_with(&same, &kept_on(&same, again));
        assert_eq!(
            (
                examples[0].label.clone(),
                examples[0].tags.source.as_deref()
            ),
            (json!("approved"), Some("agent")),
            "sent where it was going anyway: no correction"
        );

        let mut by_the_hub = run();
        complete(&mut by_the_hub, "review", "Looks fine.\nOUTCOME: approved");
        let system = by_the_hub.intervention(InterventionKind::Goto, Actor::System);
        by_the_hub.current_step_id = Some("fix".into());
        let moved = kept_on(&by_the_hub, system);
        assert_eq!(gathered_with(&by_the_hub, &moved).0[0].label, "approved");

        let mut to_default = run();
        complete(&mut to_default, "classify", "It crashes.\nOUTCOME: bug");
        let goto = to_default.intervention(InterventionKind::Goto, person());
        to_default.current_step_id = Some("reply".into());
        let (examples, _) = gathered_with(&to_default, &kept_on(&to_default, goto));
        assert_eq!(examples[0].label, NONE_OF_THESE);
        assert_eq!(examples[0].tags.source.as_deref(), Some("correction"));

        let mut nowhere = run();
        complete(&mut nowhere, "review", "Looks fine.\nOUTCOME: approved");
        let goto = nowhere.intervention(InterventionKind::Goto, person());
        nowhere.current_step_id = Some("classify".into());
        let (examples, tally) = gathered_with(&nowhere, &kept_on(&nowhere, goto));
        assert_eq!(examples[0].tags.source.as_deref(), Some("agent"));
        assert_eq!(tally.left_out["sent to a step no one branch leads to"], 1);
    }

    /// A step the decision model settled before its turn onto its default
    /// route keeps the model's pick, which teaches the branch fallback
    /// nothing — and so does the same record once a build from before
    /// `decide_first` has saved it again without saying it was settled.
    #[test]
    fn a_step_settled_first_onto_its_default_route_is_never_its_own_label() {
        let settled = "The decision model settled this step before its turn ran: ignore.";
        let mut instance = run();
        instance.current_step_id = Some("classify".into());
        instance.record_step("classify", None, settled, Utc::now(), 0, 0.0);
        instance.settle_first("classify", "ignore", "kev ignore p=0.93".into());
        WorkflowExecutor::complete_step(&def(), &mut instance, settled).unwrap();
        assert_eq!(instance.current_step_id.as_deref(), Some("reply"));

        let mut resaved = instance.clone();
        resaved.step_history[0].provenance.decided_first = None;
        for run in [&instance, &resaved] {
            let (examples, tally) = gathered(run);
            assert!(examples.is_empty(), "{examples:?}");
            assert_eq!(
                tally.left_out,
                BTreeMap::from([("the decision model's own pick", 1)])
            );
        }
    }

    #[test]
    fn what_teaches_nothing_is_left_out_and_counted_by_reason() {
        let mut instance = run();
        complete(&mut instance, "review", "Fine.");
        instance.preset_outcome("review", "approved", OutcomeBy::DecisionModel);
        instance.step_history[0].outcome = Some("approved".into());
        instance.record_step("review", Some("stuck".into()), "Again.", Utc::now(), 1, 0.0);
        instance.step_history[1].provenance.outcome_by = Some(OutcomeBy::IterationCap);
        instance.record_step("review", None, "Old run, no line.", Utc::now(), 1, 0.0);
        instance.record_step(
            "review",
            None,
            "Old run.\nOUTCOME: approved",
            Utc::now(),
            1,
            0.0,
        );
        instance.record_step("gone", None, "x", Utc::now(), 1, 0.0);
        instance.record_failed_attempt("review", "boom", Utc::now(), 1, 0.0, Default::default());

        let (examples, tally) = gathered(&instance);
        assert_eq!(examples.len(), 1, "the old run's own line still labels it");
        assert_eq!(examples[0].label, "approved");
        assert_eq!(
            tally.left_out,
            BTreeMap::from([
                ("the decision model's own pick", 1),
                ("outcome forced by the iteration cap", 1),
                ("no outcome recorded", 1),
                ("step no longer in the definition", 1),
            ])
        );

        let mut lost = Tally::default();
        let unkept = |_: &StepRecord| -> Resolved { Err("definition revision no longer kept") };
        assert!(branch_examples(&instance, &[], epoch(), &unkept, &mut lost).is_empty());
        assert_eq!(lost.left_out["definition revision no longer kept"], 5);

        let later = Utc::now() + chrono::Duration::hours(1);
        let definition = |_: &StepRecord| -> Resolved { Ok((def(), true)) };
        assert!(
            branch_examples(&instance, &[], later, &definition, &mut Tally::default()).is_empty()
        );
    }

    /// The request the live fallback sends for a step, and the record the
    /// exporter writes for the example gathered from that step's stored
    /// run, are the same bytes — in shadow and when the model's pick routes.
    #[tokio::test]
    async fn a_gathered_example_is_byte_for_byte_the_request_the_live_path_sent() {
        for shadow in [true, false] {
            let (url, seen) = recording_backend("approved", Duration::ZERO).await;
            let chars: workforce_common::settings::DecisionLimits =
                toml::from_str("state_chars = 900").unwrap();
            let config = DecisionConfig {
                branch_fallback: BranchFallbackConfig {
                    shadow,
                    limits: chars,
                    ..Default::default()
                },
                ..config_for(url)
            };
            let decisions = crate::decide::build(&config).unwrap().unwrap();

            let chatter = "I opened another file and it was fine. ".repeat(200);
            let response = format!(
                "{chatter}\nOn balance nothing is missing.\n\
                 OUTPUTS:\nsummary: all checks pass\nEND_OUTPUTS\n\
                 REPORT:\nReady. {chatter}\nEND_REPORT\nSo this can go in.\nOUTCOME: aproved"
            );
            let outputs = json!({"summary": "all checks pass", "files": 12});
            let report = format!("Ready. {chatter}");

            let mut instance = run();
            instance.current_step_id = Some("review".into());
            let mut provenance = StepProvenance::default();
            let settled = decisions
                .settle_branch(
                    &crate::credentials::Caller::new(None, None, None),
                    &Subject::default(),
                    &def(),
                    "review",
                    Produced {
                        response: &response,
                        outputs: Some(&outputs),
                        report: Some(&report),
                    },
                    &mut provenance,
                )
                .await;
            assert_eq!(settled.is_some(), !shadow);
            instance.record_step_with_provenance(
                "review",
                None,
                &response,
                Some(outputs.clone()),
                Some(report.clone()),
                Utc::now(),
                3,
                0.1,
                false,
                provenance,
            );
            if let Some(label) = &settled {
                instance.preset_outcome("review", label, OutcomeBy::DecisionModel);
            }
            WorkflowExecutor::complete_step(&def(), &mut instance, &response).unwrap();
            assert_eq!(
                instance.step_history[0].agent_response, response,
                "the stored response is the agent's, untouched"
            );

            // A person says what it should have been: back to the author.
            let goto = instance.intervention(InterventionKind::Goto, person());
            instance.current_step_id = Some("fix".into());
            instance.status = InstanceStatus::Paused;
            let moved: Vec<Intervention> =
                serde_json::from_value(json!(kept_on(&instance, goto))).unwrap();

            // Through the stored form, as the gatherer reads a run.
            let stored: WorkflowInstance =
                serde_json::from_value(serde_json::to_value(&instance).unwrap()).unwrap();
            let (examples, _) = gathered_with(&stored, &moved);
            assert_eq!(examples.len(), 1, "shadow = {shadow}");
            assert_eq!(examples[0].label, "changes-requested");
            let line = serde_json::to_string(&examples[0]).unwrap();
            let record: Value = serde_json::from_str(
                &export::line(&line, RequestBudget::chars(900))
                    .unwrap()
                    .unwrap(),
            )
            .unwrap();
            let sent = seen.lock()[0].clone();
            assert_eq!(
                (record["state"].to_string(), record["questions"].to_string()),
                (sent["state"].to_string(), sent["questions"].to_string()),
                "shadow = {shadow}"
            );
            assert!(sent["state"]["final_lines"]
                .as_str()
                .unwrap()
                .contains("OUTCOME: aproved"));
        }
    }

    fn kept(usage: &str, input: Input, verdict: Option<&str>) -> Record {
        Record {
            id: 7,
            at: Utc::now(),
            record: NewRecord {
                usage: usage.into(),
                agent_id: recorded().agent_id,
                person: recorded().person,
                channel: recorded().channel,
                instance_id: None,
                step_id: None,
                input,
                request_chars: 1_500,
                backend: "kev".into(),
                model: "fake-1".into(),
                answers: decision(json!({"on_topic": {"type": "noul", "noul": 0.2}}))
                    .response
                    .answers,
                acted: false,
                verdict: verdict.map(str::to_string),
            },
        }
    }

    #[test]
    fn a_scope_record_is_labelled_by_the_classifier_and_never_by_the_model() {
        let input = Input::ScopeGate {
            message: "What's the weather?".into(),
            topic: "bug triage".into(),
        };
        let shadowed = scope_example(&kept("scope_gate", input.clone(), Some("off_topic")));
        assert_eq!(shadowed.input, input);
        assert_eq!(shadowed.label, false);
        assert_eq!(shadowed.tags.source.as_deref(), Some("classifier"));
        assert_eq!(shadowed.tags.id.as_deref(), Some("scope_gate:7"));
        assert_eq!(shadowed.tags.meta["model_answers"]["on_topic"]["noul"], 0.2);
        assert!(
            !serde_json::to_string(&shadowed).unwrap().contains("alice"),
            "who asked is not part of an example"
        );
        assert_eq!(
            scope_example(&kept("scope_gate", input.clone(), Some("on_topic"))).label,
            true
        );

        let decided = scope_example(&kept("scope_gate", input, None));
        assert_eq!(
            (decided.label.clone(), decided.tags.source.clone()),
            (Value::Null, None)
        );
        assert_eq!(
            export::record(&decided, RequestBudget::DEFAULT),
            Err(LeftOut::Unlabelled)
        );
    }

    #[test]
    fn routing_examples_wait_for_a_label_whether_from_a_record_or_a_trace() {
        let asked = Input::Routing {
            message: "How many refunds last week?".into(),
            agent: Some("ops".into()),
            channel: Some("slack".into()),
        };
        let from_record = routing_example(&kept("routing", asked.clone(), None));
        assert_eq!(
            (from_record.input.clone(), from_record.label.clone()),
            (asked.clone(), Value::Null)
        );
        assert_eq!(from_record.tags.meta["origin"], "record");

        let header = |request: Option<&str>| TraceHeader {
            trace_id: "t1".into(),
            agent_id: "ops".into(),
            session_id: "s1".into(),
            workflow_id: None,
            step_id: None,
            task_type: Some("data_analysis".into()),
            channel: Some("slack".into()),
            request: request.map(str::to_string),
            model: "m".into(),
            system_prompt: String::new(),
            tools: Vec::new(),
            started_at: Utc::now(),
        };
        let from_trace = trace_example(&header(Some("How many refunds last week?"))).unwrap();
        assert_eq!(from_trace.input, asked);
        assert_eq!(from_trace.tags.id.as_deref(), Some("trace:t1"));
        assert_eq!(from_trace.tags.meta["task_type"], "data_analysis");
        assert_eq!(trace_example(&header(None)), None);
    }

    #[test]
    fn trace_headers_are_read_from_plain_and_gzipped_files_since_a_day() {
        use crate::trace_writer::JsonlTraceWriter;
        use std::io::Write;
        use workforce_agent::trace::{TraceRun, TraceTags, TraceWriter};

        let traces = tempfile::tempdir().unwrap();
        let header = |id: &str| {
            let tags = TraceTags {
                channel: Some("slack".into()),
                request: Some(format!("request {id}")),
                ..Default::default()
            };
            tags.header(TraceRun {
                trace_id: id.into(),
                agent_id: "ops".into(),
                session_id: "s".into(),
                model: "m".into(),
                system_prompt: String::new(),
                tools: Vec::new(),
                started_at: Utc::now(),
            })
        };
        let day = traces
            .path()
            .join("ops")
            .join(Utc::now().format("%Y-%m-%d").to_string());
        let only_file = |ext: &str| {
            let mut found = std::fs::read_dir(&day).unwrap().flatten().map(|e| e.path());
            found.find(|file| file.extension().is_some_and(|e| e == ext))
        };

        // The real writer, then gzipped as the pruner leaves an old day.
        JsonlTraceWriter::new(traces.path(), "ops")
            .unwrap()
            .write_header(&header("zipped"))
            .unwrap();
        let plain = only_file("jsonl").unwrap();
        let mut gz = flate2::write::GzEncoder::new(
            std::fs::File::create(plain.with_extension("jsonl.gz")).unwrap(),
            flate2::Compression::default(),
        );
        gz.write_all(&std::fs::read(&plain).unwrap()).unwrap();
        gz.finish().unwrap();
        std::fs::remove_file(&plain).unwrap();

        JsonlTraceWriter::new(traces.path(), "ops")
            .unwrap()
            .write_header(&header("plain"))
            .unwrap();
        let old_header = r#"{"type":"header","trace_id":"old","agent_id":"ops","session_id":"","workflow_id":null,"step_id":null,"task_type":null,"model":"m","system_prompt":"","tools":[],"started_at":"2026-01-01T00:00:00Z"}"#;
        let mut live = std::fs::OpenOptions::new()
            .append(true)
            .open(only_file("jsonl").unwrap())
            .unwrap();
        writeln!(
            live,
            "{{\"type\":\"round\",\"round\":1}}\n{old_header}\nnot json"
        )
        .unwrap();

        let stale = traces.path().join("ops").join("2020-01-01");
        std::fs::create_dir_all(&stale).unwrap();
        std::fs::write(stale.join("c.jsonl"), old_header).unwrap();
        std::fs::write(day.join("notes.txt"), old_header).unwrap();

        let since = Utc::now() - chrono::Duration::days(2);
        let files = trace_files(traces.path(), since);
        assert_eq!(files.len(), 2, "{files:?}");
        let read: Vec<TraceHeader> = files
            .iter()
            .flat_map(|file| headers(file).unwrap())
            .collect();
        let mut ids: Vec<&str> = read.iter().map(|header| header.trace_id.as_str()).collect();
        ids.sort_unstable();
        assert_eq!(ids, ["old", "plain", "zipped"]);
        for header in &read {
            let example = trace_example(header);
            match header.trace_id.as_str() {
                "old" => assert_eq!(example, None, "from before requests were kept"),
                id => assert_eq!(
                    example.unwrap().input,
                    Input::Routing {
                        message: format!("request {id}"),
                        agent: Some("ops".into()),
                        channel: Some("slack".into()),
                    }
                ),
            }
        }
    }

    /// On a Postgres install a step's definition is the revision whose hash
    /// its run recorded: the hash the runtime stamps and the one a revision
    /// is stored under are the same.
    #[cfg(feature = "postgres")]
    #[tokio::test(flavor = "multi_thread")]
    async fn a_step_resolves_to_the_revision_its_run_recorded() {
        use workforce_orchestrator::workflow_store::{
            content_hash, EditContext, RunAsWrite, WorkflowStore, KIND_WORKFLOW,
        };
        let ran = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let reached = Arc::clone(&ran);
        workforce_postgres::test_support::with_test_pool(
            WorkflowStore::build_pool,
            |pool| async move {
                let store = WorkflowStore::new(pool).await.unwrap();
                let edited = TRIAGE.replace("Read the ticket.", "Read the ticket twice.");
                for source in [TRIAGE, edited.as_str()] {
                    store
                        .save(
                            "triage",
                            KIND_WORKFLOW,
                            source,
                            None,
                            None,
                            workforce_core::access::Visibility::Global,
                            RunAsWrite::Unchanged,
                            EditContext::new("web", Some("alice")),
                        )
                        .await
                        .unwrap();
                }
                let dir = tempfile::tempdir().unwrap();
                let stores = Stores {
                    telemetry: Arc::new(
                        workforce_common::persistence::SqliteStore::in_memory().unwrap(),
                    ),
                    definitions: Some(store),
                    workflows_dir: dir.path().into(),
                    records: None,
                    traces_dir: dir.path().into(),
                };
                let task = |resolved: Resolved| {
                    let (def, exact) = resolved.unwrap();
                    (def.get_step("classify").unwrap().task.clone(), exact)
                };
                let first = stores
                    .definition("triage", Some(&content_hash(TRIAGE)))
                    .await;
                let (ran_under, exact) = task(first.unwrap());
                assert!(
                    exact && ran_under.contains("Read the ticket.\n"),
                    "{ran_under}"
                );
                let unrecorded = task(stores.definition("triage", None).await.unwrap());
                assert!(!unrecorded.1 && unrecorded.0.contains("twice"));
                let gone = stores
                    .definition("triage", Some(&content_hash("lost")))
                    .await;
                assert_eq!(
                    gone.unwrap().err(),
                    Some("definition revision no longer kept")
                );
                let unknown = stores.definition("nothing", None).await.unwrap();
                assert_eq!(unknown.err(), Some("definition not found"));
                reached.store(true, std::sync::atomic::Ordering::SeqCst);
            },
        )
        .await;
        assert!(
            ran.load(std::sync::atomic::Ordering::SeqCst)
                || std::env::var("WORKFORCE_SKIP_POSTGRES_TESTS").is_ok()
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_files_of_an_install_are_read_without_being_changed() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        let workflows = dir.path().join("workflows");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&workflows).unwrap();
        std::fs::write(workflows.join("triage.toml"), TRIAGE).unwrap();
        let mut config = WorkforceConfig::default();
        config.hub.data_dir = data.display().to_string();
        config.memory.db_path = std::path::Path::new(&config.hub.data_dir)
            .join("memory.db")
            .to_string_lossy()
            .into_owned();

        let mut instance = run();
        complete(
            &mut instance,
            "classify",
            "It crashes on save.\nOUTCOME: bug",
        );
        {
            let store =
                workforce_common::persistence::SqliteStore::open(&data.join("telemetry.db"))
                    .unwrap();
            store
                .save_workflow_instance(&workforce_common::persistence::WorkflowInstanceRow {
                    id: instance.id.clone(),
                    workflow_id: instance.workflow_id.clone(),
                    status: instance.status.to_string(),
                    current_step_id: instance.current_step_id.clone(),
                    step_history: serde_json::to_string(&instance.step_history).unwrap(),
                    variables: instance.variables.to_string(),
                    trigger_json: serde_json::to_string(&instance.trigger).unwrap(),
                    agent_id: None,
                    created_at: instance.created_at.to_rfc3339(),
                    updated_at: instance.updated_at.to_rfc3339(),
                    parent_instance_id: None,
                    channel_id: None,
                    thread_id: None,
                    owner_instance: None,
                    lease_expires_at: None,
                    version: 0,
                })
                .unwrap();
        }
        let before = std::fs::read(data.join("telemetry.db")).unwrap();

        let stores = Stores::read_only(&config, workflows).unwrap();
        let mut lines = Vec::new();
        let tally = stores
            .gather(Gather::Branch, epoch(), 10, |example| {
                lines.push(serde_json::to_string(example)?);
                Ok(())
            })
            .await
            .unwrap();
        assert_eq!((tally.examples, lines.len()), (1, 1));
        let example: Example = serde_json::from_str(&lines[0]).unwrap();
        assert_eq!(example.label, "bug");
        assert_eq!(example.tags.meta["definition"], "current");
        assert_eq!(std::fs::read(data.join("telemetry.db")).unwrap(), before);

        let none = stores
            .gather(Gather::Branch, epoch(), 0, |_| panic!("the limit is 0"))
            .await
            .unwrap();
        assert_eq!(none.examples, 0);
        let err = stores
            .gather(Gather::Scope, epoch(), 10, |_| Ok(()))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Postgres"), "{err}");

        let empty = tempfile::tempdir().unwrap();
        let mut absent = WorkforceConfig::default();
        absent.hub.data_dir = empty.path().display().to_string();
        absent.memory.db_path = std::path::Path::new(&absent.hub.data_dir)
            .join("memory.db")
            .to_string_lossy()
            .into_owned();
        assert!(Stores::read_only(&absent, empty.path().into()).is_err());
        assert!(
            !empty.path().join("telemetry.db").exists(),
            "nothing is created"
        );
    }
}
