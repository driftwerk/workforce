//! Workflow definition — parsed from TOML files.
//!
//! A workflow is a sequence of steps with optional branching, pause points,
//! exit conditions, and handoff triggers.
//!
//! ## Context Variable Type Preservation (DW-431 investigation)
//!
//! Context variables flow through the system as `serde_json::Value`. The
//! initial payload (from CLI, webhook, or API) preserves whatever JSON types
//! the caller provides — integers stay as `Number`, strings as `String`, etc.
//!
//! **A step output is text unless its declaration gives it a `type`.** The
//! `OUTPUTS:` block is free-form text, so an output declared with a name alone
//! is stored as `serde_json::Value::String`:
//!
//! - `pr_number: 42` in an OUTPUTS block becomes `"42"` (a string), not `42`
//! - Subsequent steps accessing `{{steps.impl.pr_number}}` get the string `"42"`
//!
//! An output declared `type = "integer"` (or `number`, `boolean`, `list`,
//! `object`) is read into that JSON type by `package::read`, and a value that
//! does not fit sends the step back. Template resolution renders either form
//! as text.
//!
//! For untyped outputs the defense-in-depth strategy for type correctness is:
//!
//! 1. **Prompt guidance** (engine.rs `prepare_step`) instructs agents to pass
//!    native integers when calling tools, even when context values are strings
//! 2. **Tool description types** explicitly mark numeric parameters as integers
//! 3. **Coercion helpers** (`coerce_to_u64`/`coerce_to_i64` from DW-430) let
//!    tool adapters accept both string and integer representations

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::Path;

use eyre::{Result, WrapErr};
use serde::de::Error as DeError;
use serde::{Deserialize, Deserializer, Serialize};
use workforce_common::settings::DecisionUse;

use super::decision::DecisionSpec;
use super::package::{default_true, is_true};
pub use super::package::{FieldType, OutputField, VerifySpec};

/// A complete workflow definition parsed from TOML.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowDefinition {
    pub workflow: WorkflowMeta,
    #[serde(default, rename = "step")]
    pub steps: Vec<StepDefinition>,
    /// Webhook triggers that can auto-start this workflow.
    #[serde(default, rename = "trigger")]
    pub triggers: Vec<TriggerDefinition>,
    /// Typed variables — context inputs with a default and an optional type. A
    /// `secret` variable's value is a `[[vault:<scope>:<key>]]` marker that
    /// resolves at the skill sink without its cleartext reaching the model.
    #[serde(default, rename = "variables")]
    pub variables: Vec<VariableDef>,
}

/// A declared workflow variable: typed context with a default value.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VariableDef {
    pub id: String,
    #[serde(default, rename = "type")]
    pub kind: VariableKind,
    pub value: String,
    /// For `secret` variables only: hosts the resolved secret may egress to,
    /// enforced at the browser sink. Empty = no host restriction.
    #[serde(default)]
    pub allowed_hosts: Vec<String>,
}

/// The type of a workflow variable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum VariableKind {
    /// A plain string substituted literally into the task.
    #[default]
    String,
    /// A vault-backed secret substituted as a marker, never cleartext.
    Secret,
}

impl VariableDef {
    pub fn is_secret(&self) -> bool {
        self.kind == VariableKind::Secret
    }
}

/// Top-level workflow metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowMeta {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default = "default_version")]
    pub version: String,
    /// Default agent for all steps (overridable per step).
    #[serde(default)]
    pub agent: Option<String>,
    /// Declared inputs for validation and documentation.
    #[serde(default)]
    pub context: HashMap<String, ContextField>,
    /// How step output reaches the initiating channel.
    #[serde(default)]
    pub delivery: DeliveryMode,
    /// How many instances may be active (running or paused) at once. A
    /// schedule slot that fires at the cap is skipped, so a workflow whose runs
    /// each take an hour needs more than the default of one to keep up with
    /// its cadence; a webhook or HTTP-trigger start at the cap is queued and
    /// promoted, oldest first, when a sibling finishes.
    ///
    /// Best-effort across hub instances: the count is read, then decided on,
    /// with no cross-process lock — so two hubs can admit into the same free
    /// slot. (The schedule path has always had this property.) Within one hub
    /// it is exact. The execution lease still prevents the same instance from
    /// running twice anywhere.
    #[serde(default = "default_max_concurrent")]
    pub max_concurrent: u32,
    /// A Jinja template rendered into every halt message, against the
    /// instance variables plus `step` and `error`, so a human reading the
    /// channel sees what the run was about (the ticket, the PR) rather than
    /// only that a step failed. Rendered leniently: a step that never ran
    /// reads as empty.
    #[serde(default)]
    pub failure_report: Option<String>,
    /// The WASM skills a run needs, by name, each with the semver
    /// requirement its installed version must meet (`">=0.10.0"`). A run
    /// whose skills do not meet them fails before its first step.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub requires: BTreeMap<String, String>,
    /// Hold the run instead of failing it when a step's last coding engine
    /// refuses because the subscription's usage window is spent. The instance
    /// pauses on that step, and the maintenance sweep resumes it — re-running
    /// the step — once the window clears.
    ///
    /// Off by default, because it is only the right behaviour for a workflow
    /// whose work can wait. A run that would rather hand over to a metered
    /// engine, or fail fast so a person looks at it, keeps today's behaviour
    /// untouched.
    ///
    /// This is a spent *window*, not a transient 429: a rate limit clears in
    /// minutes and the engine chain already handles it, so it still fails the
    /// run rather than parking it for hours.
    ///
    /// Paused instances count against `max_concurrent`, so a workflow at the
    /// default cap of one admits no new scheduled run while a held one waits.
    #[serde(default)]
    pub pause_on_usage_cap: bool,
    /// Whether people other than a run's starter may steer it — resume,
    /// approve or reject it, send it to a step, skip or retry one, pause or
    /// stop it — when the workflow reaches them (shared with them, their
    /// team's, or everyone's). Its starter and administrators always may. On
    /// by default; switching it off binds runs already under way.
    #[serde(default = "default_true")]
    pub steerable_by_others: bool,
}

fn default_max_concurrent() -> u32 {
    1
}

impl WorkflowMeta {
    /// Each requirement in `requires` that the installed skills do not meet,
    /// said as `requires <skill> <requirement>; installed <version>` or
    /// `…; not installed`. `installed` gives a skill's installed version.
    pub fn unmet_requirements<'a>(
        &self,
        installed: impl Fn(&str) -> Option<&'a str>,
    ) -> Vec<String> {
        self.requires
            .iter()
            .filter_map(|(skill, requirement)| {
                let wanted = format!("requires {skill} {requirement}");
                let Ok(accepted) = semver::VersionReq::parse(requirement) else {
                    return Some(format!("{wanted}, which is not a version requirement"));
                };
                match installed(skill).map(|version| (version, semver::Version::parse(version))) {
                    None => Some(format!("{wanted}; not installed")),
                    Some((_, Ok(version))) if accepted.matches(&version) => None,
                    Some((version, Ok(_))) => Some(format!("{wanted}; installed {version}")),
                    Some((version, Err(_))) => Some(format!(
                        "{wanted}; installed {version}, which is not a semver version"
                    )),
                }
            })
            .collect()
    }
}

/// How a workflow's steps speak to the channel that started it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryMode {
    /// Post the plan banner, per-step progress, and every step's full
    /// response. The right mode for interactively started runs.
    #[default]
    Full,
    /// A step speaks only through its `report` block; steps without one are
    /// silent, and the banner and progress statuses are suppressed. A run
    /// produces exactly the messages its author declared — the right mode
    /// for scheduled runs that end in a channel report.
    Reports,
}

/// Who writes a step's `report`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ReportRender {
    /// The model writes it: the resolved `report` text is guidance for a
    /// REPORT:/END_REPORT block, and that block is what posts.
    #[default]
    Agent,
    /// The engine renders `report` as a Jinja template over the instance
    /// variables once the step's outputs exist and posts the result verbatim.
    /// No model is involved, so it is legal on tool steps and on steps with no
    /// task at all.
    Template,
}

/// What a step does when it runs, decided by its shape alone.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum StepKind<'a> {
    /// A model turn driven by `task`.
    Agent,
    /// One direct WASM skill tool call, `task` being its input.
    Tool(&'a str),
    /// One question to the decision model, `task` being the state.
    Decide(&'a DecideSpec),
    /// One run of another workflow, `task` being its trigger payload.
    Workflow(&'a WorkflowCall),
    /// No turn and no tool — the step only renders its template report.
    Report,
}

/// A declared workflow input field.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextField {
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub description: String,
}

/// A single step in a workflow.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepDefinition {
    pub id: String,
    pub name: String,
    /// Natural language instruction for the agent, or a tool step's JSON
    /// input. Empty on a step that only renders its template report.
    #[serde(default)]
    pub task: String,
    /// Conditional branches — agent picks an outcome label, engine routes.
    #[serde(default)]
    pub branches: Vec<Branch>,
    /// Unconditional jump — always go to this step instead of falling through.
    #[serde(default)]
    pub goto: Option<String>,
    /// Pause the workflow here (value is the reason, e.g. "awaiting-review").
    #[serde(default)]
    pub pause: Option<String>,
    /// Terminal step — workflow ends here.
    #[serde(default)]
    pub exit: bool,
    /// Exit step leaves a [`super::package::HandoffPackage`] of this type on
    /// the run: the step's own package, under the `_handoff` variable.
    #[serde(default)]
    pub handoff: Option<String>,
    /// Inject SKILL.md content as additional context.
    #[serde(default)]
    pub skill: Option<String>,
    /// Override which agent executes this step.
    #[serde(default)]
    pub agent: Option<String>,
    /// Declared outputs — the engine extracts structured data from the agent's
    /// response and uses it (instead of raw prose) in subsequent steps' context.
    /// Beside them every recorded step exposes `_engine` and `_model` from its
    /// provenance when the engine that served it reports them; read `_model`
    /// as `{{?steps.<id>._model}}`, since not every engine names one.
    #[serde(default)]
    pub outputs: Vec<OutputField>,
    /// The steps whose packages this step is handed, by id. When set, the
    /// step's prompt carries the run's inputs and the latest package of each
    /// named step, and nothing else of what came before: no other step's
    /// outputs and nobody's prose. Unset, it is shown every earlier step.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub receives: Option<Vec<String>>,
    /// Have a verifier check this step's output against its request, its
    /// output schema and any criteria before the run moves on:
    /// `verify = true`, or `verify = { criteria = ["…"], corrections = 2 }`.
    /// A rejected output sends the step back with the verdicts, a bounded
    /// number of times. Unset, the step's agent decides (`[handoff] verify`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verify: Option<VerifySpec>,
    /// Maximum number of times this step may run for a single workflow
    /// instance before the engine force-overrides the outcome to
    /// `iteration_cap_outcome` (or escalates via the first branch whose
    /// `when` is "stuck" if no override is set). Intended for review→
    /// implement loops that would otherwise spin indefinitely when the
    /// reviewer can't converge.
    #[serde(default)]
    pub max_iterations: Option<u32>,
    /// Outcome label to force when `max_iterations` is reached. If unset,
    /// the engine looks for a branch whose `when = "stuck"` and routes
    /// there; failing that, the step falls through normally.
    #[serde(default)]
    pub iteration_cap_outcome: Option<String>,
    /// Optional per-step round budget. When set, the engine enforces this limit
    /// instead of the global max_rounds for this step.
    #[serde(default)]
    pub max_rounds: Option<u32>,
    /// Optional per-step model override. When set, the engine uses this model
    /// for the step instead of the agent's default model resolution.
    #[serde(default)]
    pub model: Option<String>,
    /// Rounds between automatic context eviction checks. When set, the engine
    /// evicts stale tool outputs every N rounds to reduce token usage.
    /// Default: None (disabled).
    #[serde(default)]
    pub context_eviction_interval: Option<u32>,
    /// Number of recent tool outputs to preserve per tool type during eviction.
    /// Default: 3.
    #[serde(default = "default_keep_last")]
    pub context_eviction_keep_last: u32,
    /// Optional human-facing report. Under `report_render = "agent"` the engine
    /// resolves its `{{variable}}` references and instructs the model to emit a
    /// REPORT:/END_REPORT block following it; under `"template"` the engine
    /// renders it as a Jinja template after the step completes. On pause steps
    /// the report is delivered instead of the full response.
    #[serde(default)]
    pub report: Option<String>,
    /// Who produces `report`: the model (default) or the engine.
    #[serde(default)]
    pub report_render: ReportRender,
    /// Skills to pre-load before this step runs (e.g. `["linear", "github"]`).
    /// Ensures required tools are available regardless of which model runs the step.
    #[serde(default)]
    pub skills: Vec<String>,
    /// Preferred tools for this step (dot-notation, e.g. `["code.git", "linear.issue_get"]`).
    /// Injected into the task prompt as guidance — the agent is steered toward these
    /// but not restricted from using others (unless `exclusive_tools` is set).
    #[serde(default)]
    pub preferred_tools: Vec<String>,
    /// When true, only load preferred_tools + skill tools + system essentials.
    /// The agent physically cannot call tools outside this set.
    /// Use this for models that struggle with large tool sets (e.g. Gemini).
    #[serde(default)]
    pub exclusive_tools: bool,
    /// Execution engine for this step. Default `"default"` uses the normal agent
    /// loop. Set to `"claude-code"` to delegate execution to Claude Code CLI,
    /// which runs its own autonomous agent loop with built-in coding tools.
    #[serde(default = "default_engine")]
    pub engine: String,
    /// Effort level for a coding engine (`low`, `medium`, `high`, `max`;
    /// `codex` also takes `xhigh`). Only used when `engine` is a coding engine
    /// or `"claude-pilot"`.
    #[serde(default)]
    pub effort: Option<String>,
    /// Wall clock for a coding-engine step, across its retries. Default: 30
    /// minutes. The only cap a `codex` step has: Codex counts no turns.
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    /// The Claude Code tools a `claude-code` step may use, replacing the default
    /// file-and-shell set (`Read`, `Edit`, `Write`, `Bash`, `Glob`, `Grep`).
    /// Hub tools served over MCP are always added. A `codex` step governs its
    /// tools by sandbox mode instead and ignores this.
    #[serde(default)]
    pub allowed_tools: Vec<String>,
    /// This step's own engine fallback chain (`engine[:model]` entries),
    /// replacing the install's `[coding_session.engine] fallback`.
    #[serde(default)]
    pub fallback: Vec<String>,
    /// Phases for multi-stage step execution. Each phase is a focused sub-task
    /// with its own tool restrictions and turn budget. Phases execute
    /// sequentially against the same session, preserving context.
    /// Supported by claude-pilot and by non-pilot engines via the agent runtime.
    #[serde(default)]
    pub phases: Vec<PhaseDefinition>,
    /// Optional persona override. When set, replaces the identity block
    /// (soul/identity/user/agents) in the system prompt with this string.
    /// Tool descriptions, skills, safety guardrails, and dynamic context
    /// are preserved. Supports `{{variable}}` template substitution.
    /// Currently honored only by the default engine.
    #[serde(default)]
    pub persona: Option<String>,
    /// Execute a WASM skill tool directly instead of running an agent turn:
    /// `tool = "skill-name.tool_name"`. `task` becomes the tool's JSON input
    /// after `{{variable}}` resolution (a non-JSON task is wrapped as
    /// `{"input": …}`). The result's top-level fields become the step's
    /// outputs and an `outcome` string field routes branches. Deterministic —
    /// no LLM in the loop. Mutually exclusive with `pause`, `phases`, and a
    /// non-default `engine`. Its `report` is delivered only when
    /// `report_render = "template"`.
    #[serde(default)]
    pub tool: Option<String>,
    /// Ask the install's decision model (Kev or Jev) instead of running an
    /// agent turn or a tool: `decide = { question = "…", kind = "choice" }`.
    /// `task` is the state the question is asked of, after `{{variable}}`
    /// resolution (JSON is passed as structure, prose as text). The answer's
    /// fields — `label`, `probability`, `probabilities`, plus `score` and
    /// `level` for a score — become the step's outputs, and the winning option
    /// becomes its outcome when it is a declared branch label at or above the
    /// probability floor. No LLM turn, a few hundred milliseconds. Mutually
    /// exclusive with `tool`, `pause`, `phases` and a non-default `engine`;
    /// its `report` is delivered only when `report_render = "template"`.
    #[serde(default)]
    pub decide: Option<DecideSpec>,
    /// Ask the install's decision model before this agent step's turn, and
    /// skip the turn when the answer is sure enough:
    /// `decide_first = { state = "…", options = { … }, floors = { … },
    /// outputs = { … } }`. Below the floor, or whenever the model cannot be
    /// asked, the turn runs as it would without the block.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decide_first: Option<DecideFirst>,
    /// Run another workflow as this step: `workflow = "ticket-triage"`, or
    /// `workflow = { id = "…", wait = false }` to start it and move on.
    /// `task` is the child's trigger payload as a JSON object (after
    /// `{{variable}}` resolution; empty means none). A waited child's final
    /// status — `completed`, `failed`, `cancelled` or `exited` — is the
    /// step's outcome, every output its steps produced is an output here, and
    /// `failed`/`cancelled` halt this run unless a branch claims them. Same
    /// exclusions as `decide`.
    #[serde(default)]
    pub workflow: Option<WorkflowCall>,
    /// Ask named people to choose what happens next, with one button per
    /// choice, when the step pauses ([`DecisionSpec`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision: Option<DecisionSpec>,
    /// Keys the author wrote that no field above claims. Captured rather than
    /// discarded so a typo is reported instead of silently disabling the
    /// feature it was meant to enable.
    #[serde(flatten, default, skip_serializing_if = "HashMap::is_empty")]
    pub unknown: HashMap<String, toml::Value>,
}

impl StepDefinition {
    /// Whether `task` goes to something other than a model turn: a tool, the
    /// decision model or another workflow.
    fn delegates_task(&self) -> bool {
        self.tool.is_some() || self.decide.is_some() || self.workflow.is_some()
    }

    pub fn kind(&self) -> StepKind<'_> {
        match (&self.tool, &self.decide, &self.workflow) {
            (Some(tool), _, _) => StepKind::Tool(tool),
            (None, Some(spec), _) => StepKind::Decide(spec),
            (None, None, Some(call)) => StepKind::Workflow(call),
            (None, None, None) if self.task.trim().is_empty() => StepKind::Report,
            (None, None, None) => StepKind::Agent,
        }
    }

    /// The `report` the engine renders itself.
    pub fn template_report(&self) -> Option<&str> {
        self.report
            .as_deref()
            .filter(|_| self.report_render == ReportRender::Template)
    }

    /// The `report` a model is asked to write — including on a tool step,
    /// where no model ever will (`authoring_issues` calls that out).
    pub fn prompted_report(&self) -> Option<&str> {
        self.report
            .as_deref()
            .filter(|_| self.report_render == ReportRender::Agent)
    }

    /// The report the engine will actually deliver for this step: `None` when
    /// there is none, or when a model-written report sits on a tool step.
    pub fn delivered_report(&self) -> Option<(ReportRender, &str)> {
        let text = self.report.as_deref()?;
        match self.report_render {
            ReportRender::Template => Some((ReportRender::Template, text)),
            ReportRender::Agent if !self.delegates_task() => Some((ReportRender::Agent, text)),
            ReportRender::Agent => None,
        }
    }
}

/// A single phase in a multi-stage step.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PhaseDefinition {
    pub name: String,
    pub task: String,
    #[serde(default = "default_phase_turns")]
    pub max_turns: u32,
    #[serde(default)]
    pub allowed_tools: Vec<String>,
    /// Whether this phase needs MCP tools (default false).
    #[serde(default)]
    pub mcp: bool,
}

fn default_phase_turns() -> u32 {
    5
}

/// What a decision step asks the decision model. The step's `task` is the
/// state, and the model reads short states: about 1,500 characters of text,
/// beyond which pasted logs and code are collapsed and the rest is cut to
/// its opening.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DecideSpec {
    /// The question, phrased so the state alone answers it.
    pub question: String,
    #[serde(default)]
    pub kind: DecideKind,
    /// For `choice`: the options and what each means (empty means the step's
    /// branch labels, undescribed). For `noul`: what `yes` and `no` mean, as
    /// the criteria the model judges by.
    #[serde(default)]
    pub options: BTreeMap<String, String>,
    /// For `score`: the ordered levels, lowest first (two to ten).
    #[serde(default)]
    pub levels: Vec<String>,
    /// The winning option's probability must reach this to set an outcome.
    /// Defaults to the install's `[decision.branch_fallback] min_probability`.
    #[serde(default)]
    pub min_probability: Option<f64>,
}

/// What an agent step asks the decision model before its turn runs. The
/// question, its options and the floor read as they do on a decision step
/// ([`DecideSpec`]); the state is the block's own, since the step's `task`
/// is instructions for an agent, not something to decide about.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DecideFirst {
    /// The state the question is asked of, after `{{variable}}` resolution:
    /// the few fields that decide the outcome (JSON is passed as structure,
    /// prose as text), within the same 1,500 characters a decision step has.
    pub state: String,
    /// How far this step goes: `on` (the default) settles, `shadow` asks
    /// and records while the turn always runs, `off` never asks. The
    /// install's and the agent's `[decision.uses.decide_first] mode` cap it.
    #[serde(default = "DecideFirst::on")]
    pub mode: DecisionUse,
    /// The question, when the stock one will not do.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub question: Option<String>,
    /// What each option means. The step's branch labels are always options,
    /// described by the step's `task` unless described here; any other
    /// name is an option that settles onto the step's `goto`.
    #[serde(default)]
    pub options: BTreeMap<String, String>,
    /// Whether the question also offers `none-of-these`, the answer that
    /// never settles. `false` only where the options already cover every
    /// route: the branches, and exactly one option for the default route.
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub escape: bool,
    /// What an option's probability must reach to settle the step. Defaults
    /// to the install's `[decision.uses.decide_first] min_probability`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_probability: Option<f64>,
    /// An option's own floor in place of `min_probability`, or `"never"`
    /// for an option only the agent's turn may choose.
    #[serde(default)]
    pub floors: BTreeMap<String, Floor>,
    /// The step's declared outputs when the model settles it, each a
    /// template over the run's variables and the answer:
    /// `{{decision.label}}`, `{{decision.probability}}`,
    /// `{{decision.backend}}` and `{{decision.model}}`.
    #[serde(default)]
    pub outputs: BTreeMap<String, String>,
}

/// The option the decision model chooses when none of a step's outcomes
/// clearly applies. It never settles a step, so no option may take its name.
pub const NO_OUTCOME_OPTION: &str = "none-of-these";

/// What a `decide_first` output template reads of the answer, under
/// `decision.`.
pub const DECISION_FIELDS: [&str; 4] = ["label", "probability", "backend", "model"];

/// The root a `decide_first` output template reads the answer under.
pub const DECISION_ROOT: &str = "decision";

impl DecideFirst {
    fn on() -> DecisionUse {
        DecisionUse::On
    }

    /// The block's question as a decision step would ask it.
    pub fn decide(&self) -> DecideSpec {
        DecideSpec {
            question: self.question.clone().unwrap_or_default(),
            kind: DecideKind::Choice,
            options: self.options.clone(),
            levels: Vec::new(),
            min_probability: self.min_probability,
        }
    }

    /// Every option the question offers `step`: its branch labels, then
    /// whatever else the block describes or sets a floor for.
    fn options_of<'a>(&'a self, step: &'a StepDefinition) -> Vec<&'a str> {
        let mut options: Vec<&str> = step.branches.iter().map(|b| b.when.as_str()).collect();
        for option in self.options.keys() {
            if !options.contains(&option.as_str()) {
                options.push(option);
            }
        }
        options
    }

    /// The options the question offers `step` that nothing describes.
    fn unmeant(&self, step: &StepDefinition) -> Vec<String> {
        let branches: Vec<String> = step.branches.iter().map(|b| b.when.clone()).collect();
        let questions = workforce_decide::first::questions(
            self.question.as_deref(),
            &step.task,
            &branches,
            &self.options,
            self.escape,
        );
        workforce_decide::first::unmeant(&questions)
            .into_iter()
            .map(str::to_string)
            .collect()
    }

    /// Whether the model may settle the step on `option`.
    fn may_settle(&self, option: &str) -> bool {
        self.floors.get(option) != Some(&Floor::Never)
    }
}

/// An option's own probability floor: `0.95`, or `"never"`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "FloorRepr", into = "FloorRepr")]
pub enum Floor {
    At(f64),
    Never,
}

#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum FloorRepr {
    At(f64),
    Word(String),
}

impl TryFrom<FloorRepr> for Floor {
    type Error = String;

    fn try_from(repr: FloorRepr) -> Result<Self, String> {
        match repr {
            FloorRepr::At(p) if p > 0.0 && p <= 1.0 => Ok(Floor::At(p)),
            FloorRepr::At(p) => Err(format!(
                "a floor must be more than 0 and at most 1, got {p}"
            )),
            FloorRepr::Word(word) if word == "never" => Ok(Floor::Never),
            FloorRepr::Word(word) => Err(format!(
                "a floor is a probability or \"never\", got \"{word}\""
            )),
        }
    }
}

impl From<Floor> for FloorRepr {
    fn from(floor: Floor) -> Self {
        match floor {
            Floor::At(p) => FloorRepr::At(p),
            Floor::Never => FloorRepr::Word("never".to_string()),
        }
    }
}

/// The only options a yes/no decision describes.
pub const NOUL_OPTIONS: [&str; 2] = ["yes", "no"];

/// The shape of a decision step's answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DecideKind {
    /// One option out of a set; branches carry the option names.
    #[default]
    Choice,
    /// Yes or no; branches are `yes` and `no`.
    Noul,
    /// A level on an ordered scale; branches carry the level names.
    Score,
}

/// Another workflow run as a step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "WorkflowCallRepr")]
pub struct WorkflowCall {
    /// The workflow's id, resolved in the calling workflow's owner namespace
    /// first, then among system workflows; never another member's.
    pub id: String,
    /// Block until the child finishes (the default), or start it and move on
    /// with `started` as the outcome and its instance id as an output.
    pub wait: bool,
}

/// `workflow = "id"` or `workflow = { id = "…", wait = … }`.
#[derive(Deserialize)]
#[serde(untagged)]
enum WorkflowCallRepr {
    Id(String),
    Full {
        id: String,
        #[serde(default = "default_true")]
        wait: bool,
    },
}

impl From<WorkflowCallRepr> for WorkflowCall {
    fn from(repr: WorkflowCallRepr) -> Self {
        match repr {
            WorkflowCallRepr::Id(id) => Self { id, wait: true },
            WorkflowCallRepr::Full { id, wait } => Self { id, wait },
        }
    }
}

/// The outcomes a waited child run can hand its calling step: its final
/// instance status.
pub const CHILD_RUN_OUTCOMES: [&str; 4] = ["completed", "failed", "cancelled", "exited"];
/// The outcome of a `wait = false` workflow step, and the only outputs it has.
pub const CHILD_STARTED_OUTCOME: &str = "started";
pub const CHILD_STARTED_OUTPUTS: [&str; 3] = ["instance_id", "workflow_id", "status"];

/// A conditional branch from a step.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Branch {
    /// Semantic outcome label the agent selects.
    pub when: String,
    /// Target step ID to jump to.
    pub goto: String,
}

/// A trigger that can auto-start this workflow.
///
/// - `Webhook` fires when a webhook event matches `source` + `event_type` and
///   all `match_rules` pass against the payload. A `secret` authenticates the
///   inbound payload (HMAC signature) and is mandatory (enforced at load).
/// - `Schedule` fires on a cron expression, registered with the hub scheduler
///   at workflow load time. `run_as` sets `CURRENT_USER_ID` during execution
///   so per-user tool paths (e.g. `notes write_daily`) resolve correctly.
/// - `Http` fires when a caller `POST`s to one of the trigger's public paths
///   and authenticates per `auth`. The request body becomes the workflow
///   payload; `response` selects streamed (SSE) or fire-and-forget delivery.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TriggerDefinition {
    Webhook {
        /// Webhook source to match (e.g. "github", "linear").
        source: String,
        /// Event type to match (e.g. "Issue.update", "pull_request").
        event_type: String,
        /// Dot-path conditions that must all match against the payload
        /// (AND logic). Keys are dot-separated JSON paths, values are expected
        /// string values. If a path traverses an array, any matching element
        /// is sufficient.
        #[serde(default, rename = "match")]
        match_rules: HashMap<String, String>,
        /// HMAC secret used to verify the inbound payload signature before this
        /// workflow starts. Carries a `[[vault:<key>]]` marker resolved against
        /// the `workflows/` vault namespace by the hub receiver. Required for
        /// every webhook trigger — `validate` rejects a definition without one.
        #[serde(default)]
        secret: Option<String>,
    },
    Schedule {
        /// Cron expression (UTC unless `tz` is set).
        cron: String,
        /// IANA timezone name (e.g. "Europe/Berlin"). Defaults to UTC.
        #[serde(default)]
        tz: Option<String>,
        /// Identity to run the workflow as. Sets `CURRENT_USER_ID` for the
        /// workflow execution so per-user tool paths resolve correctly.
        #[serde(default)]
        run_as: Option<String>,
        /// Arbitrary payload merged into `TriggerInfo.payload` when the
        /// workflow starts.
        #[serde(default)]
        payload: serde_json::Value,
    },
    Http {
        /// Endpoint id, unique within the definition's owner. The hub mints
        /// a permanent public path per `(definition, owner, id)` and reports
        /// it as `trigger_urls`; `POST /triggers/{id}` (system) and
        /// `POST /triggers/{owner}/{id}` (member) keep answering as well.
        id: String,
        /// Optional dot-path conditions ANDed against the request body, reusing
        /// the webhook matcher. Absent = any body for this id fires the workflow.
        #[serde(default, rename = "match")]
        match_rules: HashMap<String, String>,
        /// Required caller authentication, verified before any workflow work.
        auth: TriggerAuth,
        /// How the response is delivered. Defaults to streaming (SSE).
        #[serde(default)]
        response: HttpResponseMode,
    },
}

/// Caller authentication for an HTTP trigger, verified at the edge before any
/// workflow work begins.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum TriggerAuth {
    /// Shared secret presented as `Authorization: Bearer <token>`, compared in
    /// constant time against the vault-resolved value. The default mode.
    Bearer {
        /// `[[vault:<key>]]` marker resolved against the `workflows/` namespace.
        token: String,
    },
    /// HMAC-SHA256 over the raw request body, presented as
    /// `X-Signature-256: sha256=<hex>`.
    Hmac {
        /// `[[vault:<key>]]` marker resolved against the `workflows/` namespace.
        secret: String,
    },
    /// JWT verified against the issuer's JWKS; identity-aware.
    Oidc(OidcTriggerAuth),
}

/// OIDC verification parameters for an HTTP trigger. Authorizes by matching a
/// configurable token claim against an allowlist and, optionally, runs the
/// workflow as the workforce identity the verified token maps to.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OidcTriggerAuth {
    /// OIDC issuer; `{issuer}/.well-known/openid-configuration` → `jwks_uri`
    /// supplies the verification keys.
    pub issuer: String,
    /// Required `aud` claim — rejects tokens minted for other audiences.
    pub audience: String,
    /// Token claim used as the caller principal. Defaults to `sub`.
    #[serde(default = "default_principal_claim")]
    pub principal_claim: String,
    /// Principal values permitted to invoke this trigger. Must be non-empty.
    pub allowed_principals: Vec<String>,
    /// When true, run the workflow as the workforce identity the verified token
    /// maps to (when it carries a known, verified email). Defaults to false.
    #[serde(default)]
    pub run_as_caller: bool,
}

fn default_principal_claim() -> String {
    "sub".to_string()
}

/// How an HTTP trigger delivers its response to the caller.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HttpResponseMode {
    /// Stream workflow progress back as Server-Sent Events, ending with the
    /// final result. The default.
    #[default]
    Streaming,
    /// Return `202 Accepted` with the instance id and run the workflow detached.
    FireForget,
}

/// Intermediate shape used to deserialize `TriggerDefinition` with a
/// backwards-compatible default: triggers written before schedule support
/// omit `type` and expect webhook semantics.
#[derive(Deserialize)]
struct TriggerRaw {
    #[serde(default, rename = "type")]
    kind: Option<String>,
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    event_type: Option<String>,
    #[serde(default, rename = "match")]
    match_rules: HashMap<String, String>,
    #[serde(default)]
    secret: Option<String>,
    #[serde(default)]
    cron: Option<String>,
    #[serde(default)]
    tz: Option<String>,
    #[serde(default)]
    run_as: Option<String>,
    #[serde(default)]
    payload: serde_json::Value,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    auth: Option<AuthRaw>,
    #[serde(default)]
    response: Option<HttpResponseMode>,
}

/// Untyped `[trigger.auth]` table, resolved into a `TriggerAuth` with a
/// bearer default and mode-specific required-field checks.
#[derive(Deserialize)]
struct AuthRaw {
    #[serde(default)]
    mode: Option<String>,
    #[serde(default)]
    token: Option<String>,
    #[serde(default)]
    secret: Option<String>,
    #[serde(default)]
    issuer: Option<String>,
    #[serde(default)]
    audience: Option<String>,
    #[serde(default)]
    principal_claim: Option<String>,
    #[serde(default)]
    allowed_principals: Vec<String>,
    #[serde(default)]
    run_as_caller: bool,
}

fn build_trigger_auth(raw: AuthRaw) -> Result<TriggerAuth, String> {
    match raw.mode.as_deref() {
        None | Some("bearer") => raw
            .token
            .map(|token| TriggerAuth::Bearer { token })
            .ok_or_else(|| "auth mode 'bearer' requires a token".to_string()),
        Some("hmac") => raw
            .secret
            .map(|secret| TriggerAuth::Hmac { secret })
            .ok_or_else(|| "auth mode 'hmac' requires a secret".to_string()),
        Some("oidc") => {
            let issuer = raw
                .issuer
                .ok_or_else(|| "auth mode 'oidc' requires an issuer".to_string())?;
            let audience = raw
                .audience
                .ok_or_else(|| "auth mode 'oidc' requires an audience".to_string())?;
            if raw.allowed_principals.is_empty() {
                return Err("auth mode 'oidc' requires a non-empty allowed_principals".to_string());
            }
            Ok(TriggerAuth::Oidc(OidcTriggerAuth {
                issuer,
                audience,
                principal_claim: raw.principal_claim.unwrap_or_else(default_principal_claim),
                allowed_principals: raw.allowed_principals,
                run_as_caller: raw.run_as_caller,
            }))
        }
        Some(other) => Err(format!("unknown auth mode '{other}'")),
    }
}

impl<'de> Deserialize<'de> for TriggerDefinition {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = TriggerRaw::deserialize(deserializer)?;
        match raw.kind.as_deref() {
            Some("schedule") => {
                let cron = raw.cron.ok_or_else(|| D::Error::missing_field("cron"))?;
                Ok(TriggerDefinition::Schedule {
                    cron,
                    tz: raw.tz,
                    run_as: raw.run_as,
                    payload: raw.payload,
                })
            }
            Some("http") => {
                let id = raw.id.ok_or_else(|| D::Error::missing_field("id"))?;
                let auth_raw = raw.auth.ok_or_else(|| D::Error::missing_field("auth"))?;
                let auth = build_trigger_auth(auth_raw).map_err(D::Error::custom)?;
                Ok(TriggerDefinition::Http {
                    id,
                    match_rules: raw.match_rules,
                    auth,
                    response: raw.response.unwrap_or_default(),
                })
            }
            Some("webhook") | None => {
                let source = raw
                    .source
                    .ok_or_else(|| D::Error::missing_field("source"))?;
                let event_type = raw
                    .event_type
                    .ok_or_else(|| D::Error::missing_field("event_type"))?;
                Ok(TriggerDefinition::Webhook {
                    source,
                    event_type,
                    match_rules: raw.match_rules,
                    secret: raw.secret,
                })
            }
            Some(other) => Err(D::Error::unknown_variant(
                other,
                &["webhook", "schedule", "http"],
            )),
        }
    }
}

fn default_version() -> String {
    "1.0".to_string()
}

fn default_keep_last() -> u32 {
    3
}

/// Execution engines a step may name. A misspelling used to parse clean and
/// silently fall back to the default.
const STEP_ENGINES: &[&str] = &["default", "claude-code", "codex", "claude-pilot"];

/// The engines that delegate a whole step to a coding CLI in a sandbox and
/// take the coding-step fields (`timeout_secs`, `allowed_tools`, `fallback`).
pub const CODING_ENGINES: &[&str] = &["claude-code", "codex"];

/// Whether `engine` runs the step on a coding engine rather than the agent
/// loop.
pub fn is_coding_engine(engine: &str) -> bool {
    CODING_ENGINES.contains(&engine)
}

fn default_engine() -> String {
    "default".to_string()
}

/// An HTTP trigger id must be a non-empty URL-path-safe slug so it can address
/// its owner-derived public path unambiguously. Excluding `.` is load-bearing
/// beyond tidiness: it is what makes a minted path (`t.{hex}`) provably
/// distinct from every owner-derived one, so one index resolves both with no
/// possibility of either shadowing the other.
pub fn is_valid_http_trigger_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Names the workflow infrastructure owns: an author cannot declare them, and
/// the engine supplies them only when the run happens to have one — a channel
/// for a chat-started run, a worktree once a step names its repo, a ticket when
/// one resolves. Referencing them is therefore always allowed and never
/// guaranteed, so they validate clean and render empty when absent.
pub const RESERVED_PAYLOAD_KEYS: &[&str] = &[
    "_channel_id",
    "_thread_id",
    "_worktree_path",
    "_cancellation_reason",
    "_failure_reason",
    "ticket_data",
];

/// Every `{{…}}`-resolved field of a step, in one place so the reference check
/// and any future consumer cannot drift apart. A template-rendered report is
/// Jinja and is scanned through `report::references` instead.
fn step_templates(step: &StepDefinition) -> impl Iterator<Item = &str> {
    std::iter::once(step.task.as_str())
        .chain(step.prompted_report())
        .chain(step.persona.as_deref())
        .chain(step.phases.iter().map(|phase| phase.task.as_str()))
        .chain(step.decide_first.iter().map(|first| first.state.as_str()))
}

/// The run variables a step's `decide_first` output templates read: every
/// reference but the answer's own `decision.` fields.
fn settled_output_references(step: &StepDefinition) -> impl Iterator<Item = (bool, &str)> {
    step.decide_first
        .iter()
        .flat_map(|first| first.outputs.values())
        .flat_map(|template| super::template::references(template))
        .filter(|(_, key)| super::report::root(key) != DECISION_ROOT)
}

/// Every variable a step's human-facing sinks (`report`, `persona`) read,
/// scanned the way each text is rendered, so a secret can be refused from them.
fn sink_references(step: &StepDefinition) -> impl Iterator<Item = (&'static str, String)> + '_ {
    let rendered = step
        .template_report()
        .into_iter()
        .map(|template| ("report", template))
        .chain(decision_templates(step))
        .flat_map(|(label, template)| {
            super::report::references(template)
                .into_iter()
                .map(move |key| (label, key))
        });
    scanned("report", step.prompted_report())
        .chain(scanned("persona", step.persona.as_deref()))
        .chain(rendered)
}

/// A decision's Jinja fields, rendered against the run's variables when its
/// step pauses.
fn decision_templates(step: &StepDefinition) -> impl Iterator<Item = (&'static str, &str)> {
    step.decision
        .iter()
        .flat_map(|spec| spec.templates())
        .map(|(_, template)| ("decision", template))
}

fn scanned<'a>(
    label: &'static str,
    text: Option<&'a str>,
) -> impl Iterator<Item = (&'static str, String)> + 'a {
    text.into_iter()
        .flat_map(super::template::references)
        .map(move |(_, key)| (label, key.to_string()))
}

/// Where a variable reference sits, which decides what the authoring check may
/// assume about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReferenceSite {
    /// A `{{…}}` field resolved before the step runs: the producer must be a
    /// different step that runs on every path here, or be marked optional.
    Prompt,
    /// A Jinja report rendered after the step completes: the step's own outputs
    /// are visible, and a skipped producer is the template's to guard with
    /// `{% if steps.<id> %}` — an unguarded read fails the step at runtime with
    /// the line and the unresolved path, so only unknown steps and unknown
    /// outputs are static defects.
    Template,
}

impl WorkflowDefinition {
    /// Parse a workflow definition from a TOML string.
    pub fn from_toml(toml_str: &str) -> Result<Self> {
        let def: Self = toml::from_str(toml_str).wrap_err("Failed to parse workflow TOML")?;
        def.validate()?;
        Ok(def)
    }

    /// Load a workflow definition from a TOML file.
    pub fn from_file(path: &Path) -> Result<Self> {
        let content = fs::read_to_string(path)
            .wrap_err_with(|| format!("Failed to read {}", path.display()))?;
        Self::from_toml(&content)
    }

    /// Load all workflow definitions from a directory.
    pub fn load_dir(dir: &Path) -> Result<Vec<Self>> {
        if !dir.exists() {
            return Ok(vec![]);
        }
        let mut defs = Vec::new();
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "toml") {
                match Self::from_file(&path) {
                    Ok(def) => defs.push(def),
                    Err(e) => {
                        tracing::warn!(path = %path.display(), error = %e, "Skipping invalid workflow");
                    }
                }
            }
        }
        Ok(defs)
    }

    /// Validate the workflow definition for consistency.
    pub fn validate(&self) -> Result<()> {
        if let Some(template) = &self.workflow.failure_report {
            super::report::check(template).map_err(|e| {
                eyre::eyre!(
                    "Workflow '{}' failure_report does not compile: {e}",
                    self.workflow.id
                )
            })?;
        }
        if self.workflow.max_concurrent == 0 {
            return Err(eyre::eyre!(
                "Workflow '{}' has max_concurrent = 0; a schedule needs at least one slot",
                self.workflow.id
            ));
        }
        for (skill, requirement) in &self.workflow.requires {
            if skill.is_empty()
                || !skill
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
            {
                return Err(eyre::eyre!(
                    "Workflow '{}' requires `{skill}`, which is not a skill name (lowercase letters, digits and hyphens)",
                    self.workflow.id
                ));
            }
            semver::VersionReq::parse(requirement).map_err(|e| {
                eyre::eyre!(
                    "Workflow '{}' requires {skill} `{requirement}`, which is not a semver requirement such as \">=0.10.0\": {e}",
                    self.workflow.id
                )
            })?;
        }
        if self.steps.is_empty() {
            return Err(eyre::eyre!("Workflow '{}' has no steps", self.workflow.id));
        }

        let step_ids: HashMap<&str, usize> = self
            .steps
            .iter()
            .enumerate()
            .map(|(i, s)| (s.id.as_str(), i))
            .collect();

        if step_ids.len() != self.steps.len() {
            return Err(eyre::eyre!(
                "Workflow '{}' has duplicate step IDs",
                self.workflow.id
            ));
        }

        for step in &self.steps {
            for branch in &step.branches {
                if !step_ids.contains_key(branch.goto.as_str()) {
                    return Err(eyre::eyre!(
                        "Step '{}' branches to unknown step '{}'",
                        step.id,
                        branch.goto
                    ));
                }
            }

            if let Some(ref goto) = step.goto {
                if !step_ids.contains_key(goto.as_str()) {
                    return Err(eyre::eyre!(
                        "Step '{}' has goto to unknown step '{}'",
                        step.id,
                        goto
                    ));
                }
            }

            if step.exit && step.goto.is_some() {
                return Err(eyre::eyre!(
                    "Step '{}' is an exit step but also has goto",
                    step.id
                ));
            }

            if !STEP_ENGINES.contains(&step.engine.as_str()) {
                return Err(eyre::eyre!(
                    "Step '{}' has unknown engine '{}' (use {})",
                    step.id,
                    step.engine,
                    STEP_ENGINES.join(", ")
                ));
            }
            Self::validate_coding_engine_fields(step)?;

            if let Some(defect) = step.outputs.iter().find_map(OutputField::defect) {
                return Err(eyre::eyre!("Step '{}' {defect}", step.id));
            }

            self.validate_handover(step)?;

            if step.report_render == ReportRender::Template {
                let Some(template) = step.report.as_deref() else {
                    return Err(eyre::eyre!(
                        "Step '{}' sets report_render = \"template\" but declares no report",
                        step.id
                    ));
                };
                super::report::check(template)
                    .map_err(|e| eyre::eyre!("Step '{}' report template: {e}", step.id))?;
            }

            if let Some(spec) = &step.decision {
                spec.validate(step, self)?;
            }
            if let Some(first) = &step.decide_first {
                Self::validate_decide_first(step, first)?;
            }
            Self::validate_turnless(step)?;
            let delegates = [
                step.tool.is_some(),
                step.decide.is_some(),
                step.workflow.is_some(),
            ]
            .into_iter()
            .filter(|set| *set)
            .count();
            if delegates > 1 {
                return Err(eyre::eyre!(
                    "Step '{}' declares more than one of `tool`, `decide` and `workflow`; a step is one of them",
                    step.id
                ));
            }
            match step.kind() {
                StepKind::Agent => {}
                StepKind::Tool(_) => {
                    if step.pause.is_some() {
                        return Err(eyre::eyre!(
                            "Step '{}' is a tool step and cannot declare a pause point",
                            step.id
                        ));
                    }
                }
                StepKind::Decide(spec) => Self::validate_decide(step, spec)?,
                StepKind::Workflow(call) => {
                    Self::validate_workflow_call(step, call, &self.workflow.id)?
                }
                StepKind::Report => Self::validate_report_only(step)?,
            }
        }

        for trigger in &self.triggers {
            match trigger {
                TriggerDefinition::Webhook { secret, .. } if secret.is_none() => {
                    return Err(eyre::eyre!(
                        "Webhook trigger in workflow '{}' must declare a secret",
                        self.workflow.id
                    ));
                }
                TriggerDefinition::Http { id, .. } if !is_valid_http_trigger_id(id) => {
                    return Err(eyre::eyre!(
                        "HTTP trigger id '{}' in workflow '{}' must be non-empty and match [A-Za-z0-9_-]+",
                        id,
                        self.workflow.id
                    ));
                }
                _ => {}
            }
        }

        self.validate_variables()?;

        Ok(())
    }

    /// `timeout_secs`, `allowed_tools` and `fallback` shape a coding-engine
    /// run and nothing else, so on another engine they would silently do
    /// nothing.
    fn validate_coding_engine_fields(step: &StepDefinition) -> Result<()> {
        if !is_coding_engine(&step.engine) {
            if step.timeout_secs.is_some() {
                return Err(eyre::eyre!(
                    "Step '{}' sets timeout_secs, which only a coding engine ({}) uses",
                    step.id,
                    CODING_ENGINES.join(", ")
                ));
            }
            if !step.allowed_tools.is_empty() {
                return Err(eyre::eyre!(
                    "Step '{}' sets allowed_tools, which only a coding engine ({}) uses",
                    step.id,
                    CODING_ENGINES.join(", ")
                ));
            }
            if !step.fallback.is_empty() {
                return Err(eyre::eyre!(
                    "Step '{}' sets fallback, which only a coding engine ({}) uses",
                    step.id,
                    CODING_ENGINES.join(", ")
                ));
            }
        }
        workforce_common::settings::EngineEntry::parse_all(&step.fallback)
            .map_err(|e| eyre::eyre!("Step '{}' fallback: {e}", step.id))?;
        if step.timeout_secs == Some(0) {
            return Err(eyre::eyre!(
                "Step '{}' has timeout_secs = 0; a claude-code step needs some wall clock",
                step.id
            ));
        }
        if let Some(blank) = step.allowed_tools.iter().find(|t| t.trim().is_empty()) {
            return Err(eyre::eyre!(
                "Step '{}' lists a blank allowed tool {blank:?}",
                step.id
            ));
        }
        Ok(())
    }

    /// `receives` names steps that have a package to hand over by the time
    /// this one runs, and `verify` sits on a step a correction can send back.
    fn validate_handover(&self, step: &StepDefinition) -> Result<()> {
        if step.receives.is_some() && !step.phases.is_empty() {
            return Err(eyre::eyre!(
                "Step '{}' sets receives, but a phased step is prompted by its phases alone: \
                 write the values it needs into them as {{{{steps.<id>.<field>}}}}",
                step.id
            ));
        }
        for producer in step.receives.iter().flatten() {
            if *producer == step.id {
                return Err(eyre::eyre!("Step '{}' receives its own package", step.id));
            }
            let Some(index) = self.step_index(producer) else {
                return Err(eyre::eyre!(
                    "Step '{}' receives unknown step '{producer}'",
                    step.id
                ));
            };
            if self.steps[index].outputs.is_empty() {
                return Err(eyre::eyre!(
                    "Step '{}' receives '{producer}', which declares no outputs to hand over",
                    step.id
                ));
            }
            if !self.leads_to(index, &step.id) {
                return Err(eyre::eyre!(
                    "Step '{}' receives '{producer}', which never runs before it",
                    step.id
                ));
            }
        }
        let Some(verify) = &step.verify else {
            return Ok(());
        };
        if step.kind() != StepKind::Agent {
            return Err(eyre::eyre!(
                "Step '{}' sets verify, which only a step with a model turn can honour: a correction would run its tool, decision or workflow again",
                step.id
            ));
        }
        match verify.defect() {
            Some(defect) => Err(eyre::eyre!("Step '{}' {defect}", step.id)),
            None => Ok(()),
        }
    }

    /// What no step without a model turn can carry.
    fn validate_turnless(step: &StepDefinition) -> Result<()> {
        let what = match step.kind() {
            StepKind::Agent => return Ok(()),
            StepKind::Tool(_) => "is a tool step and",
            StepKind::Decide(_) => "is a decision step and",
            StepKind::Workflow(_) => "runs another workflow and",
            StepKind::Report => "renders only a report and",
        };
        if !step.phases.is_empty() {
            return Err(eyre::eyre!(
                "Step '{}' {what} cannot declare phases",
                step.id
            ));
        }
        if step.engine != default_engine() {
            return Err(eyre::eyre!(
                "Step '{}' {what} cannot override the engine",
                step.id
            ));
        }
        Ok(())
    }

    /// A decision step needs a state to ask about, a question, and an answer
    /// shape its branches can route on.
    fn validate_decide(step: &StepDefinition, spec: &DecideSpec) -> Result<()> {
        if step.pause.is_some() {
            return Err(eyre::eyre!(
                "Step '{}' is a decision step and cannot declare a pause point",
                step.id
            ));
        }
        if step.task.trim().is_empty() {
            return Err(eyre::eyre!(
                "Step '{}' is a decision step and needs a `task`: the state the question is asked of",
                step.id
            ));
        }
        if spec.question.trim().is_empty() {
            return Err(eyre::eyre!(
                "Step '{}' decision needs a non-empty question",
                step.id
            ));
        }
        if let Some(p) = spec.min_probability {
            if !(0.0..=1.0).contains(&p) {
                return Err(eyre::eyre!(
                    "Step '{}' decision min_probability must be between 0 and 1",
                    step.id
                ));
            }
        }
        let labels: Vec<&str> = step.branches.iter().map(|b| b.when.as_str()).collect();
        match spec.kind {
            DecideKind::Choice => {
                if !spec.levels.is_empty() {
                    return Err(eyre::eyre!(
                        "Step '{}' is a choice decision and takes no levels",
                        step.id
                    ));
                }
                let options: Vec<&str> = if spec.options.is_empty() {
                    labels.clone()
                } else {
                    spec.options.keys().map(String::as_str).collect()
                };
                if options.len() < 2 {
                    return Err(eyre::eyre!(
                        "Step '{}' choice decision needs at least two options: describe them in \
                         decide.options or declare branches",
                        step.id
                    ));
                }
                if options.len() > 255 {
                    return Err(eyre::eyre!(
                        "Step '{}' choice decision has more than 255 options",
                        step.id
                    ));
                }
                if let Some(stray) = labels.iter().find(|l| !options.contains(l)) {
                    return Err(eyre::eyre!(
                        "Step '{}' branch '{stray}' is not one of the decision's options",
                        step.id
                    ));
                }
            }
            DecideKind::Noul => {
                if !spec.levels.is_empty() {
                    return Err(eyre::eyre!(
                        "Step '{}' is a yes/no decision and takes no levels",
                        step.id
                    ));
                }
                if let Some(stray) = spec
                    .options
                    .keys()
                    .find(|k| !NOUL_OPTIONS.contains(&k.as_str()))
                {
                    return Err(eyre::eyre!(
                        "Step '{}' yes/no decision option '{stray}': describe only 'yes' and 'no'",
                        step.id
                    ));
                }
                if let Some(stray) = labels.iter().find(|l| **l != "yes" && **l != "no") {
                    return Err(eyre::eyre!(
                        "Step '{}' branch '{stray}': a yes/no decision routes on 'yes' or 'no'",
                        step.id
                    ));
                }
            }
            DecideKind::Score => {
                if !spec.options.is_empty() {
                    return Err(eyre::eyre!(
                        "Step '{}' is a score decision and takes levels, not options",
                        step.id
                    ));
                }
                if spec.levels.len() < 2 || spec.levels.len() > 10 {
                    return Err(eyre::eyre!(
                        "Step '{}' score decision needs two to ten levels, lowest first",
                        step.id
                    ));
                }
                if let Some(stray) = labels
                    .iter()
                    .find(|l| !spec.levels.iter().any(|lv| lv == *l))
                {
                    return Err(eyre::eyre!(
                        "Step '{}' branch '{stray}' is not one of the decision's levels",
                        step.id
                    ));
                }
            }
        }
        Ok(())
    }

    /// `decide_first` sits on a step whose turn it can stand in for: an
    /// agent step with outcomes to settle, every option it may settle on
    /// with somewhere to go, and every required output filled.
    fn validate_decide_first(step: &StepDefinition, first: &DecideFirst) -> Result<()> {
        let refused = |why: String| Err(eyre::eyre!("Step '{}' {why}", step.id));
        if step.kind() != StepKind::Agent {
            return refused(
                "sets decide_first, which only an agent step can carry: a tool, decision, workflow or report step has no turn to skip".into(),
            );
        }
        if !step.phases.is_empty() {
            return refused(
                "sets decide_first on a phased step, whose phases one answer cannot stand in for"
                    .into(),
            );
        }
        if step.pause.is_some() || step.exit {
            return refused(
                "sets decide_first on a step that pauses or ends the run, which the decision model cannot settle".into(),
            );
        }
        if step.branches.is_empty() {
            return refused(
                "sets decide_first but declares no branches, so it has no outcome to settle".into(),
            );
        }
        if first.state.trim().is_empty() {
            return refused(
                "decide_first needs a `state`: the few fields the outcome is decided from".into(),
            );
        }
        if first
            .question
            .as_deref()
            .is_some_and(|q| q.trim().is_empty())
        {
            return refused(
                "decide_first has an empty question: leave it out for the stock one".into(),
            );
        }
        if first
            .min_probability
            .is_some_and(|p| !(p > 0.0 && p <= 1.0))
        {
            return refused(
                "decide_first min_probability must be more than 0 and at most 1".into(),
            );
        }
        let options = first.options_of(step);
        if options.contains(&NO_OUTCOME_OPTION) || first.floors.contains_key(NO_OUTCOME_OPTION) {
            return refused(format!(
                "decide_first cannot use '{NO_OUTCOME_OPTION}': it is the answer that always leaves the step to its agent"
            ));
        }
        if options.len() > 254 {
            return refused("decide_first has more than 254 options".into());
        }
        if let Some(stray) = first.floors.keys().find(|o| !options.contains(&o.as_str())) {
            return refused(format!(
                "decide_first sets a floor for '{stray}', which is not one of its options ({})",
                options.join(", ")
            ));
        }
        let is_branch = |option: &str| step.branches.iter().any(|b| b.when == option);
        if !first.escape {
            let default_route: Vec<&str> = options
                .iter()
                .copied()
                .filter(|option| !is_branch(option))
                .collect();
            match default_route[..] {
                [_] => {}
                [] => return refused(
                    "decide_first sets escape = false but no option stands for the step's default route, so the model would have to call a branch for everything else: describe that outcome in options, or keep the escape".into(),
                ),
                _ => return refused(format!(
                    "decide_first sets escape = false with more than one option for the step's default route ({}), so a turn that takes it could not say which it was: keep one, or keep the escape",
                    default_route.join(", ")
                )),
            }
            let low = first
                .min_probability
                .into_iter()
                .chain(first.floors.values().filter_map(|floor| match floor {
                    Floor::At(p) => Some(*p),
                    Floor::Never => None,
                }))
                .find(|p| !workforce_decide::first::breaks_ties(*p));
            if let Some(low) = low {
                return refused(format!(
                    "decide_first sets escape = false with a floor of {low}, which an answer split between two options can reach: without the escape every floor must be above 0.5"
                ));
            }
        }
        if step.goto.is_none() {
            if let Some(stranded) = options
                .iter()
                .find(|option| !is_branch(option) && first.may_settle(option))
            {
                return refused(format!(
                    "decide_first option '{stranded}' is not a branch and the step has no `goto`, so settling on it would leave the run with no route: add a branch or a goto, or set its floor to \"never\""
                ));
            }
        }
        if let Some(stray) = first
            .outputs
            .keys()
            .find(|name| !step.outputs.iter().any(|o| o.name == **name))
        {
            return refused(format!(
                "decide_first fills output '{stray}', which the step does not declare"
            ));
        }
        if let Some(unfilled) = step
            .outputs
            .iter()
            .find(|o| o.required && !first.outputs.contains_key(&o.name))
        {
            return refused(format!(
                "decide_first has no template for required output '{}': a step the decision model settles hands over every required output too",
                unfilled.name
            ));
        }
        let unknown = first
            .outputs
            .values()
            .flat_map(|template| super::template::references(template))
            .filter_map(|(_, key)| key.strip_prefix(DECISION_ROOT)?.strip_prefix('.'))
            .find(|field| !DECISION_FIELDS.contains(field));
        match unknown {
            Some(field) => refused(format!(
                "decide_first output reads '{{{{{DECISION_ROOT}.{field}}}}}', which the answer does not carry (use {})",
                DECISION_FIELDS.join(", ")
            )),
            None => Ok(()),
        }
    }

    /// A workflow step names a workflow other than its own, hands over a JSON
    /// object if anything, and routes only on how the child run ended.
    fn validate_workflow_call(
        step: &StepDefinition,
        call: &WorkflowCall,
        own_id: &str,
    ) -> Result<()> {
        if step.pause.is_some() {
            return Err(eyre::eyre!(
                "Step '{}' runs another workflow and cannot declare a pause point",
                step.id
            ));
        }
        let id = call.id.trim();
        if id.is_empty() {
            return Err(eyre::eyre!("Step '{}' names no workflow to run", step.id));
        }
        if id.contains('/') {
            return Err(eyre::eyre!(
                "Step '{}' names workflow '{id}': give the bare id — a step runs its own \
                 owner's workflows and system ones, never another member's",
                step.id
            ));
        }
        if id == own_id {
            return Err(eyre::eyre!(
                "Step '{}' runs its own workflow '{own_id}': a workflow cannot call itself",
                step.id
            ));
        }
        let task = step.task.trim();
        if !task.is_empty() && !task.starts_with('{') {
            return Err(eyre::eyre!(
                "Step '{}' task must be a JSON object: the payload the workflow '{id}' starts with",
                step.id
            ));
        }
        let allowed: &[&str] = if call.wait {
            &CHILD_RUN_OUTCOMES
        } else {
            if let Some(stray) = step
                .outputs
                .iter()
                .find(|o| !CHILD_STARTED_OUTPUTS.contains(&o.name.as_str()))
            {
                return Err(eyre::eyre!(
                    "Step '{}' output '{}': a workflow step that does not wait has only {}",
                    step.id,
                    stray.name,
                    CHILD_STARTED_OUTPUTS.join(", ")
                ));
            }
            &[CHILD_STARTED_OUTCOME]
        };
        if let Some(stray) = step
            .branches
            .iter()
            .find(|b| !allowed.contains(&b.when.as_str()))
        {
            return Err(eyre::eyre!(
                "Step '{}' branch '{}': a workflow step routes on how the run ended ({})",
                step.id,
                stray.when,
                allowed.join(", ")
            ));
        }
        Ok(())
    }

    /// A step with neither task nor tool exists to render a report: nothing
    /// produces outputs, and only a human (via `pause`) can choose a branch.
    fn validate_report_only(step: &StepDefinition) -> Result<()> {
        match step.delivered_report() {
            Some((ReportRender::Template, _)) => {}
            Some((ReportRender::Agent, _)) => {
                return Err(eyre::eyre!(
                    "Step '{}' has no task and no tool, so no model can write its report — \
                     set report_render = \"template\"",
                    step.id
                ));
            }
            None => {
                return Err(eyre::eyre!(
                    "Step '{}' has no task and no tool — a step without a model turn must \
                     carry a rendered report (report_render = \"template\" with a report)",
                    step.id
                ));
            }
        }
        if !step.outputs.is_empty() {
            return Err(eyre::eyre!(
                "Step '{}' renders only a report and cannot declare outputs",
                step.id
            ));
        }
        if !step.branches.is_empty() && step.pause.is_none() {
            return Err(eyre::eyre!(
                "Step '{}' renders only a report and declares branches without a pause — \
                 nothing can choose an outcome; add `pause` so a human chooses, or route \
                 with `goto`",
                step.id
            ));
        }
        Ok(())
    }

    /// Validate the typed `[[variables]]` block: id shape/uniqueness, that a
    /// secret value is exactly one well-formed scoped vault marker, that a
    /// non-secret value carries no vault ref, that `allowed_hosts` is
    /// secret-only, and that no secret is referenced from a human-facing sink
    /// (`report`/`persona`, the workflow's `failure_report`).
    fn validate_variables(&self) -> Result<()> {
        use workforce_common::vault_scope::{contains_vault_ref, parse_exact_scoped_marker};

        let mut seen = HashMap::new();
        for v in &self.variables {
            if v.id.is_empty() || !v.id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                return Err(eyre::eyre!(
                    "variable id '{}' must be non-empty and match [A-Za-z0-9_]+",
                    v.id
                ));
            }
            if seen.insert(v.id.as_str(), ()).is_some() {
                return Err(eyre::eyre!("duplicate variable id '{}'", v.id));
            }
            if v.is_secret() {
                if parse_exact_scoped_marker(&v.value).is_none() {
                    return Err(eyre::eyre!(
                        "secret variable '{}' value must be a single [[vault:<scope>:<key>]] marker",
                        v.id
                    ));
                }
            } else if contains_vault_ref(&v.value) {
                return Err(eyre::eyre!(
                    "variable '{}' references a vault marker but is not type=secret",
                    v.id
                ));
            } else if !v.allowed_hosts.is_empty() {
                return Err(eyre::eyre!(
                    "allowed_hosts is only valid on secret variables (variable '{}')",
                    v.id
                ));
            }
        }

        let secret_ids: Vec<&str> = self
            .variables
            .iter()
            .filter(|v| v.is_secret())
            .map(|v| v.id.as_str())
            .collect();
        let failure_report = self
            .workflow
            .failure_report
            .as_deref()
            .into_iter()
            .flat_map(super::report::references)
            .map(|key| ("failure_report".to_string(), key));
        let step_sinks = self.steps.iter().flat_map(|step| {
            sink_references(step)
                .map(move |(label, key)| (format!("step '{}' {label}", step.id), key))
        });
        failure_report
            .chain(step_sinks)
            .find(|(_, key)| secret_ids.contains(&super::report::root(key)))
            .map_or(Ok(()), |(site, key)| {
                Err(eyre::eyre!(
                    "secret variable '{}' must not be referenced from {site}",
                    super::report::root(&key)
                ))
            })
    }

    /// Steps reachable in one transition from the step at `index`, mirroring
    /// `WorkflowExecutor::complete_step`: branch targets, plus `goto` when set,
    /// plus the next step in order only when neither claims the transition. A
    /// branches-only step has no fall-through — an unmatched outcome is a
    /// routing error, not a path. A decision step goes where its branches and
    /// its buttons' `goto`s go, and never falls through.
    fn successors(&self, index: usize) -> Vec<&str> {
        let Some(step) = self.steps.get(index).filter(|s| !s.exit) else {
            return Vec::new();
        };
        let branches = step.branches.iter().map(|b| b.goto.as_str());
        if let Some(spec) = &step.decision {
            return branches
                .chain(spec.buttons.iter().filter_map(|b| b.goto.as_deref()))
                .collect();
        }
        match (&step.goto, step.branches.is_empty()) {
            (Some(goto), _) => branches.chain(std::iter::once(goto.as_str())).collect(),
            (None, false) => branches.collect(),
            (None, true) => self
                .steps
                .get(index + 1)
                .map(|next| next.id.as_str())
                .into_iter()
                .collect(),
        }
    }

    /// Step ids reachable from the entry step.
    fn reachable(&self) -> HashSet<&str> {
        self.reached_from(self.steps.first().map(|_| 0).into_iter().collect())
    }

    /// Whether a run that has left the step at `from` can come to `to`.
    fn leads_to(&self, from: usize, to: &str) -> bool {
        let after = self
            .successors(from)
            .into_iter()
            .filter_map(|id| self.step_index(id))
            .collect();
        self.reached_from(after).contains(to)
    }

    /// Step ids a run standing at any of `pending` is at or can come to.
    fn reached_from(&self, mut pending: Vec<usize>) -> HashSet<&str> {
        let mut seen = HashSet::new();
        while let Some(index) = pending.pop() {
            let Some(step) = self.steps.get(index) else {
                continue;
            };
            if !seen.insert(step.id.as_str()) {
                continue;
            }
            pending.extend(
                self.successors(index)
                    .into_iter()
                    .filter_map(|id| self.step_index(id)),
            );
        }
        seen
    }

    /// For each reachable step, the steps that run on *every* path from the
    /// entry to it — itself included. Standard iterative dominator fixpoint.
    fn dominators(&self) -> HashMap<&str, HashSet<&str>> {
        let reachable = self.reachable();
        let Some(entry) = self.steps.first().map(|s| s.id.as_str()) else {
            return HashMap::new();
        };

        let predecessors = self.steps.iter().enumerate().fold(
            HashMap::<&str, Vec<&str>>::new(),
            |mut acc, (index, step)| {
                for target in self.successors(index) {
                    acc.entry(target).or_default().push(step.id.as_str());
                }
                acc
            },
        );

        let mut dominators: HashMap<&str, HashSet<&str>> = reachable
            .iter()
            .map(|&id| {
                let seed = if id == entry {
                    HashSet::from([entry])
                } else {
                    reachable.clone()
                };
                (id, seed)
            })
            .collect();

        let order: Vec<&str> = reachable
            .iter()
            .copied()
            .filter(|&id| id != entry)
            .collect();
        loop {
            let mut changed = false;
            for &id in &order {
                let merged = {
                    let mut from_predecessors = predecessors
                        .get(id)
                        .into_iter()
                        .flatten()
                        .filter_map(|predecessor| dominators.get(predecessor))
                        .cloned()
                        .reduce(|left, right| &left & &right)
                        .unwrap_or_default();
                    from_predecessors.insert(id);
                    from_predecessors
                };
                if dominators.get(id) != Some(&merged) {
                    dominators.insert(id, merged);
                    changed = true;
                }
            }
            if !changed {
                return dominators;
            }
        }
    }

    /// Variables a reference may name without the check complaining: engine-owned
    /// names, everything with a default, and every declared context field — a
    /// `required = false` field renders empty rather than failing, so declaring it
    /// is the author's acknowledgement that it may be absent. Trigger payloads are deliberately excluded: a manual
    /// start carries none, which is precisely how a scheduled-only variable
    /// becomes a hard failure the first time someone runs the workflow by hand.
    fn guaranteed_variables(&self) -> HashSet<&str> {
        self.variables
            .iter()
            .map(|v| v.id.as_str())
            .chain(self.workflow.context.keys().map(String::as_str))
            .chain(RESERVED_PAYLOAD_KEYS.iter().copied())
            .collect()
    }

    /// Everything an author got wrong that is not structurally fatal: a step no
    /// path reaches, a reference that cannot resolve where it is used, and a key
    /// no field claims. Empty means the definition runs as written on every path.
    /// Structural problems (unknown branch targets, duplicate ids, bad enums) are
    /// [`Self::validate`]'s job and are assumed to have passed.
    pub fn authoring_issues(&self) -> Vec<String> {
        let reachable = self.reachable();
        let dominators = self.dominators();
        let guaranteed = self.guaranteed_variables();
        let declared_outputs: HashMap<&str, HashSet<&str>> = self
            .steps
            .iter()
            .map(|step| {
                let names = step.outputs.iter().map(|o| o.name.as_str()).collect();
                (step.id.as_str(), names)
            })
            .collect();

        let unreachable = self
            .steps
            .iter()
            .enumerate()
            .filter(|(_, step)| !reachable.contains(step.id.as_str()))
            .map(
                |(index, step)| match self.orphaning_predecessor(index, &reachable) {
                    Some(blocker) => format!(
                        "step '{}' is unreachable — step '{blocker}' declares branches but no \
                     `goto`, so it has no fall-through to the next step. Add \
                     `goto = \"{}\"` to '{blocker}' for its default path, or branch to it.",
                        step.id, step.id
                    ),
                    None => format!("step '{}' is unreachable", step.id),
                },
            );

        let (dominators, declared_outputs, guaranteed) =
            (&dominators, &declared_outputs, &guaranteed);
        let unresolvable = self
            .steps
            .iter()
            .filter(|step| reachable.contains(step.id.as_str()))
            .flat_map(move |step| {
                let prompted = step_templates(step)
                    .flat_map(super::template::references)
                    .chain(settled_output_references(step))
                    .filter(|(optional, _)| !optional)
                    .map(|(_, key)| (key.to_string(), ReferenceSite::Prompt))
                    .collect::<Vec<_>>();
                let rendered = step
                    .template_report()
                    .into_iter()
                    .chain(decision_templates(step).map(|(_, template)| template))
                    .flat_map(super::report::references)
                    .map(|key| (key, ReferenceSite::Template));
                prompted
                    .into_iter()
                    .chain(rendered)
                    .filter_map(|(key, site)| {
                        self.reference_issue(
                            step,
                            &key,
                            dominators.get(step.id.as_str()),
                            declared_outputs,
                            guaranteed,
                            site,
                        )
                    })
                    .collect::<Vec<_>>()
            });

        let mistyped = self.steps.iter().flat_map(|step| {
            step.unknown.keys().map(move |key| {
                format!(
                    "step '{}' sets '{key}', which is not a step field — it is ignored, \
                     so whatever it was meant to configure is not in effect",
                    step.id
                )
            })
        });

        // A model-written `report` on a delegated step is never produced — no
        // model runs — so it neither posts nor counts toward reports delivery
        // having a voice.
        let dead_reports = self
            .steps
            .iter()
            .filter(|step| step.report.is_some() && step.delivered_report().is_none())
            .map(|step| {
                let runs_as = match step.kind() {
                    StepKind::Decide(_) => "a decision",
                    StepKind::Workflow(_) => "another workflow",
                    _ => "a tool call",
                };
                format!(
                    "step '{}' declares a `report` but runs as {runs_as} — no model ever \
                     produces the report, so it is never posted; set report_render = \
                     \"template\" to have the engine render it from the step's outputs",
                    step.id
                )
            });
        let mute = (self.workflow.delivery == DeliveryMode::Reports
            && self
                .steps
                .iter()
                .all(|step| step.delivered_report().is_none()))
        .then(|| {
            "delivery = \"reports\" but no step declares a report the engine can deliver — \
             every run will finish without posting anything to the channel"
                .to_string()
        });

        // A `decide_first` option the question would offer with no meaning
        // is refused here, at save, and only skipped at run time: a stored
        // definition never fails to load over how its task is worded.
        let unmeant = self.steps.iter().flat_map(|step| {
            step.decide_first
                .as_ref()
                .map(|first| first.unmeant(step))
                .unwrap_or_default()
                .into_iter()
                .map(|option| {
                    format!(
                        "step '{}' decide_first offers '{option}' with nothing saying what it \
                         means — no line of the task names it and decide_first.options does \
                         not describe it — so the model cannot tell it apart: describe it in \
                         decide_first.options",
                        step.id
                    )
                })
        });

        let mut issues: Vec<String> = unreachable
            .chain(unresolvable)
            .chain(mistyped)
            .chain(dead_reports)
            .chain(mute)
            .chain(unmeant)
            .collect();
        issues.sort();
        issues.dedup();
        issues
    }

    /// What a save of this definition warns about its decisions without
    /// refusing it: a branch no button takes, and a decision that never
    /// expires in a scheduled workflow with one slot.
    pub fn decision_notices(&self) -> Vec<String> {
        super::decision::notices(self)
    }

    /// The step immediately before an unreachable one when that step is the
    /// reason: declaring `branches` removes the fall-through to the next step,
    /// which is the single most common way an authored workflow strands its own
    /// happy path.
    fn orphaning_predecessor(&self, index: usize, reachable: &HashSet<&str>) -> Option<&str> {
        let previous = self.steps.get(index.checked_sub(1)?)?;
        (reachable.contains(previous.id.as_str())
            && !previous.branches.is_empty()
            && previous.decision.is_none()
            && previous.goto.is_none()
            && !previous.exit)
            .then_some(previous.id.as_str())
    }

    fn reference_issue(
        &self,
        step: &StepDefinition,
        key: &str,
        dominators: Option<&HashSet<&str>>,
        declared_outputs: &HashMap<&str, HashSet<&str>>,
        guaranteed: &HashSet<&str>,
        site: ReferenceSite,
    ) -> Option<String> {
        let Some(("steps", rest)) = key.split_once('.') else {
            // A bare reference may walk into a structured value
            // (`{{repository.full_name}}`), so only the root has to be declared —
            // the resolver walks the rest. `steps` itself is engine-owned.
            let root = super::report::root(key);
            return (root != "steps" && !guaranteed.contains(root)).then(|| {
                format!(
                    "step '{}' references '{{{{{key}}}}}', which no [[variables]] entry or \
                     [workflow.context] field declares — declare it, or write '{{{{?{key}}}}}' \
                     to render empty when absent",
                    step.id
                )
            });
        };

        let (producer, field) = match rest.split_once('.') {
            Some((producer, path)) => (producer, path.split('.').next()),
            None => (rest, None),
        };

        if !declared_outputs.contains_key(producer) {
            return Some(format!(
                "step '{}' references '{{{{{key}}}}}', but no step '{producer}' exists",
                step.id
            ));
        }
        if producer == step.id.as_str() && site == ReferenceSite::Prompt {
            return Some(format!(
                "step '{}' references its own output '{{{{{key}}}}}', which is only recorded \
                 after the step finishes",
                step.id
            ));
        }
        if site == ReferenceSite::Prompt && !dominators.is_some_and(|d| d.contains(producer)) {
            return Some(format!(
                "step '{}' references '{{{{{key}}}}}', but step '{producer}' does not run on \
                 every path that reaches it — write '{{{{?{key}}}}}' to render empty when it \
                 was skipped",
                step.id
            ));
        }
        field
            .filter(|name| {
                !declared_outputs[producer].contains(name)
                    && !super::instance::PROVENANCE_OUTPUTS.contains(name)
            })
            .map(|name| {
                format!(
                    "step '{}' references '{{{{{key}}}}}', but step '{producer}' declares no \
                     output named '{name}'",
                    step.id
                )
            })
    }

    /// Get a step definition by ID.
    pub fn get_step(&self, id: &str) -> Option<&StepDefinition> {
        self.steps.iter().find(|s| s.id == id)
    }

    /// Get the index of a step by ID.
    pub fn step_index(&self, id: &str) -> Option<usize> {
        self.steps.iter().position(|s| s.id == id)
    }

    /// Get the first step.
    pub fn first_step(&self) -> Option<&StepDefinition> {
        self.steps.first()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STEP: &str = "\n[[step]]\nid = \"s1\"\nname = \"S\"\ntask = \"do it\"\n";

    /// Build a definition from step fragments without going through
    /// `from_toml`'s structural validation, so a graph can be asserted on
    /// directly.
    fn wf(steps: &str) -> WorkflowDefinition {
        WorkflowDefinition::from_toml(&format!("[workflow]\nid = \"wf\"\nname = \"WF\"\n{steps}"))
            .expect("fixture must parse")
    }

    /// Concatenated step fragments, so a fixture reads as a list of steps
    /// rather than a chain of string additions.
    fn wf_of(fragments: &[String]) -> WorkflowDefinition {
        wf(&fragments.concat())
    }

    fn step(id: &str, extra: &str) -> String {
        format!("\n[[step]]\nid = \"{id}\"\nname = \"{id}\"\ntask = \"t\"\n{extra}")
    }

    #[test]
    fn successors_of_exit_step_are_empty() {
        let def = wf(&step("a", "exit = true\n"));
        assert!(def.successors(0).is_empty());
    }

    /// A decision step `review` whose buttons send, redraft at `draft` or
    /// end the run, with `extra` added to its decision.
    fn decision_wf(extra: &str, triggers: &str) -> WorkflowDefinition {
        wf(&format!(
            "max_concurrent = 1\n{triggers}\n\
             [[step]]\nid = \"draft\"\nname = \"Draft\"\ntask = \"draft it\"\n\
             outputs = [{{ name = \"subject\" }}]\n\n\
             [[step]]\nid = \"review\"\nname = \"Review\"\nreport_render = \"template\"\n\
             report = \"{{{{ steps.draft.subject }}}}\"\npause = \"awaiting-approval\"\n\
             branches = [{{ when = \"send\", goto = \"send\" }}, {{ when = \"hold\", goto = \"send\" }}]\n\n\
             [step.decision]\ndeciders = \"approver.one@example.com\"\n{extra}\n\
             [[step.decision.buttons]]\nlabel = \"Send\"\noutcome = \"send\"\n\n\
             [[step.decision.buttons]]\nlabel = \"Redraft\"\noutcome = \"redraft\"\ngoto = \"draft\"\n\n\
             [[step.decision.buttons]]\nlabel = \"Drop\"\noutcome = \"drop\"\nend = \"cancel\"\n\n\
             [[step]]\nid = \"after\"\nname = \"After\"\ntask = \"never\"\n\n\
             [[step]]\nid = \"send\"\nname = \"Send\"\ntask = \"send it\"\nexit = true\n"
        ))
    }

    #[test]
    fn successors_include_button_gotos_and_a_decision_step_never_falls_through() {
        let def = decision_wf("", "");
        let review = def.step_index("review").unwrap();
        assert_eq!(def.successors(review), ["send", "send", "draft"]);
        assert!(def
            .authoring_issues()
            .iter()
            .any(|issue| issue == "step 'after' is unreachable"));
    }

    #[test]
    fn a_button_goto_target_is_reachable() {
        let def = decision_wf("", "");
        let review = def.step_index("review").unwrap();
        assert!(def.leads_to(review, "draft"));
        assert!(def.reachable().contains("draft"));
    }

    #[test]
    fn a_title_reading_an_unknown_output_is_an_authoring_issue() {
        let def = decision_wf("title = \"Send {{ steps.draft.nothing }}?\"", "");
        let issues = def.authoring_issues();
        assert!(
            issues
                .iter()
                .any(|issue| issue.contains("declares no output named 'nothing'")),
            "{issues:?}"
        );
    }

    #[test]
    fn a_branch_no_button_reaches_is_a_notice() {
        let notices = decision_wf("", "").decision_notices();
        assert_eq!(
            notices,
            ["step 'review' branch 'hold' is taken by no decision button, so no press reaches it"]
        );
    }

    #[test]
    fn a_scheduled_single_slot_workflow_without_expiry_gets_a_notice() {
        let schedule = "\n[[trigger]]\ntype = \"schedule\"\ncron = \"0 7 * * *\"\n";
        let unbounded = decision_wf("", schedule).decision_notices();
        assert!(
            unbounded
                .iter()
                .any(|n| n.contains("every later scheduled run is skipped")),
            "{unbounded:?}"
        );
        let bounded = decision_wf("expires_after = \"20h\"\non_expiry = \"drop\"", schedule)
            .decision_notices();
        assert!(
            !bounded.iter().any(|n| n.contains("scheduled run")),
            "{bounded:?}"
        );
    }

    #[test]
    fn pause_on_usage_cap_is_off_unless_a_workflow_asks_for_it() {
        let def = wf(STEP);
        assert!(
            !def.workflow.pause_on_usage_cap,
            "holding a run for hours must be opted into, never inherited"
        );
    }

    #[test]
    fn pause_on_usage_cap_is_read_from_the_workflow_block() {
        let def = WorkflowDefinition::from_toml(&format!(
            "[workflow]\nid = \"wf\"\nname = \"WF\"\npause_on_usage_cap = true\n{STEP}"
        ))
        .expect("fixture must parse");
        assert!(def.workflow.pause_on_usage_cap);
    }

    #[test]
    fn a_definition_saved_before_the_flag_lets_others_steer_its_runs() {
        assert!(wf(STEP).workflow.steerable_by_others);
        let closed = WorkflowDefinition::from_toml(&format!(
            "[workflow]\nid = \"wf\"\nname = \"WF\"\nsteerable_by_others = false\n{STEP}"
        ))
        .expect("fixture must parse");
        assert!(!closed.workflow.steerable_by_others);
        let round_trip = WorkflowDefinition::from_toml(&toml::to_string(&closed).unwrap()).unwrap();
        assert!(
            !round_trip.workflow.steerable_by_others,
            "a save keeps it switched off"
        );
    }

    #[test]
    fn successors_fall_through_in_order() {
        let def = wf_of(&[step("a", ""), step("b", "")]);
        assert_eq!(def.successors(0), vec!["b"]);
        assert!(def.successors(1).is_empty());
    }

    #[test]
    fn successors_prefer_goto_over_fall_through() {
        let def = wf_of(&[step("a", "goto = \"c\"\n"), step("b", ""), step("c", "")]);
        assert_eq!(def.successors(0), vec!["c"]);
    }

    #[test]
    fn successors_of_branches_only_step_exclude_fall_through() {
        let def = wf(
            &(step("a", "[[step.branches]]\nwhen = \"x\"\ngoto = \"c\"\n")
                + &step("b", "")
                + &step("c", "")),
        );
        assert_eq!(def.successors(0), vec!["c"]);
    }

    #[test]
    fn successors_of_branches_with_goto_include_both() {
        let def = wf_of(&[
            step(
                "a",
                "goto = \"b\"\n[[step.branches]]\nwhen = \"x\"\ngoto = \"c\"\n",
            ),
            step("b", ""),
            step("c", ""),
        ]);
        let mut got = def.successors(0);
        got.sort_unstable();
        assert_eq!(got, vec!["b", "c"]);
    }

    #[test]
    fn dominators_over_a_chain_include_every_earlier_step() {
        let def = wf_of(&[step("a", ""), step("b", ""), step("c", "")]);
        let dom = def.dominators();
        assert_eq!(dom["c"], HashSet::from(["a", "b", "c"]));
    }

    #[test]
    fn dominators_over_a_diamond_exclude_the_arms() {
        let def = wf_of(&[
            step(
                "a",
                "[[step.branches]]\nwhen = \"l\"\ngoto = \"left\"\n\
             [[step.branches]]\nwhen = \"r\"\ngoto = \"right\"\n",
            ),
            step("left", "goto = \"join\"\n"),
            step("right", "goto = \"join\"\n"),
            step("join", ""),
        ]);
        let dom = def.dominators();
        assert_eq!(dom["join"], HashSet::from(["a", "join"]));
    }

    #[test]
    fn dominators_terminate_on_a_loop() {
        let def = wf_of(&[
            step("a", ""),
            step(
                "b",
                "goto = \"a\"\n[[step.branches]]\nwhen = \"done\"\ngoto = \"c\"\n",
            ),
            step("c", ""),
        ]);
        let dom = def.dominators();
        assert_eq!(dom["c"], HashSet::from(["a", "b", "c"]));
    }

    #[test]
    fn unreachable_step_is_reported() {
        let def = wf_of(&[step("a", "exit = true\n"), step("orphan", "")]);
        assert!(def
            .authoring_issues()
            .iter()
            .any(|i| i.contains("step 'orphan' is unreachable")));
    }

    #[test]
    fn a_mistyped_step_key_is_reported_rather_than_ignored() {
        let def = wf("\n[[step]]\nid = \"a\"\nname = \"a\"\ntask = \"t\"\nmodle = \"opus\"\n");
        assert!(
            def.authoring_issues()
                .iter()
                .any(|i| i.contains("'modle'") && i.contains("not a step field")),
            "a typo must not silently disable the field it meant: {:?}",
            def.authoring_issues()
        );
    }

    #[test]
    fn a_correctly_spelled_step_key_is_not_reported() {
        let def = wf("\n[[step]]\nid = \"a\"\nname = \"a\"\ntask = \"t\"\nmodel = \"opus\"\n");
        assert_eq!(def.authoring_issues(), Vec::<String>::new());
    }

    #[test]
    fn engine_injected_variables_need_no_declaration() {
        let def = wf(
            "\n[[step]]\nid = \"a\"\nname = \"a\"\n             task = \"in {{_worktree_path}} for {{ticket_data}} on {{_channel_id}}\"\n",
        );
        assert_eq!(def.authoring_issues(), Vec::<String>::new());
    }

    /// The live `pr-review` shape: `prepare` branches only on failure, so the
    /// engine gives it no fall-through and the workflow can never reach `merge`.
    #[test]
    fn branches_without_goto_strand_the_next_step_and_the_error_says_so() {
        let def = wf_of(&[
            step(
                "prepare",
                "[[step.branches]]\nwhen = \"gates-failed\"\ngoto = \"prepare\"\n",
            ),
            step("merge", "exit = true\n"),
        ]);
        let issues = def.authoring_issues();
        let unreachable = issues
            .iter()
            .find(|i| i.contains("'merge' is unreachable"))
            .expect("merge must be reported unreachable");
        assert!(
            unreachable.contains("declares branches but no")
                && unreachable.contains("goto = \"merge\""),
            "the error must name the cause and the fix, got: {unreachable}"
        );
    }

    #[test]
    fn a_goto_on_a_branching_step_keeps_the_next_step_reachable() {
        let def = wf_of(&[
            step(
                "prepare",
                "goto = \"merge\"\n[[step.branches]]\nwhen = \"gates-failed\"\ngoto = \"prepare\"\n",
            ),
            step("merge", "exit = true\n"),
        ]);
        assert_eq!(def.authoring_issues(), Vec::<String>::new());
    }

    #[test]
    fn reference_to_a_dominating_step_output_is_accepted() {
        let def = wf_of(&[
            step("a", "[[step.outputs]]\nname = \"plan\"\n"),
            String::from("\n[[step]]\nid = \"b\"\nname = \"b\"\ntask = \"use {{steps.a.plan}}\"\n"),
        ]);
        assert_eq!(def.authoring_issues(), Vec::<String>::new());
    }

    #[test]
    fn reference_to_a_branch_skipped_step_is_rejected() {
        let def = wf_of(&[step(
            "check",
            "[[step.branches]]\nwhen = \"known\"\ngoto = \"report\"\n\
             [[step.branches]]\nwhen = \"new\"\ngoto = \"classify\"\n",
        ), step("classify", "[[step.outputs]]\nname = \"items\"\n"), String::from("\n[[step]]\nid = \"report\"\nname = \"report\"\ntask = \"{{steps.classify.items}}\"\nexit = true\n")]);
        let issues = def.authoring_issues();
        assert!(
            issues
                .iter()
                .any(|i| i.contains("does not run on every path")),
            "expected a dominance failure, got {issues:?}"
        );
    }

    #[test]
    fn optional_marker_accepts_a_branch_skipped_reference() {
        let def = wf_of(&[step(
            "check",
            "[[step.branches]]\nwhen = \"known\"\ngoto = \"report\"\n\
             [[step.branches]]\nwhen = \"new\"\ngoto = \"classify\"\n",
        ), step("classify", "[[step.outputs]]\nname = \"items\"\n"), String::from("\n[[step]]\nid = \"report\"\nname = \"report\"\ntask = \"{{?steps.classify.items}}\"\nexit = true\n")]);
        assert_eq!(def.authoring_issues(), Vec::<String>::new());
    }

    #[test]
    fn reference_to_an_undeclared_output_is_rejected() {
        let def = wf_of(&[
            step("a", "[[step.outputs]]\nname = \"plan\"\n"),
            String::from("\n[[step]]\nid = \"b\"\nname = \"b\"\ntask = \"{{steps.a.missing}}\"\n"),
        ]);
        assert!(def
            .authoring_issues()
            .iter()
            .any(|i| i.contains("declares no output named 'missing'")));
    }

    #[test]
    fn self_reference_is_rejected() {
        let def = wf("\n[[step]]\nid = \"a\"\nname = \"a\"\ntask = \"{{steps.a.own}}\"\n[[step.outputs]]\nname = \"own\"\n");
        assert!(def
            .authoring_issues()
            .iter()
            .any(|i| i.contains("references its own output")));
    }

    #[test]
    fn undeclared_bare_variable_is_rejected() {
        let def = wf("\n[[step]]\nid = \"a\"\nname = \"a\"\ntask = \"post to {{channel_id}}\"\n");
        assert!(def
            .authoring_issues()
            .iter()
            .any(|i| i.contains("{{channel_id}}")));
    }

    #[test]
    fn declared_context_field_satisfies_a_bare_variable() {
        let def = WorkflowDefinition::from_toml(
            "[workflow]\nid = \"wf\"\nname = \"WF\"\n             [workflow.context.channel_id]\nrequired = false\n             \n[[step]]\nid = \"a\"\nname = \"a\"\ntask = \"post to {{channel_id}}\"\n",
        )
        .unwrap();
        assert_eq!(def.authoring_issues(), Vec::<String>::new());
    }

    #[test]
    fn a_declared_context_field_covers_a_path_into_it() {
        let def = WorkflowDefinition::from_toml(
            "[workflow]\nid = \"wf\"\nname = \"WF\"\n\
             [workflow.context.repository]\nrequired = true\n\
             \n[[step]]\nid = \"a\"\nname = \"a\"\ntask = \"{{repository.full_name}}\"\n",
        )
        .unwrap();
        assert_eq!(def.authoring_issues(), Vec::<String>::new());
    }

    #[test]
    fn an_undeclared_root_is_still_rejected_for_a_path() {
        let def = wf("\n[[step]]\nid = \"a\"\nname = \"a\"\ntask = \"{{nope.field}}\"\n");
        assert!(def
            .authoring_issues()
            .iter()
            .any(|i| i.contains("{{nope.field}}")));
    }

    #[test]
    fn delivery_defaults_to_full_and_parses_reports() {
        assert_eq!(wf(STEP).workflow.delivery, DeliveryMode::Full);
        let def = WorkflowDefinition::from_toml(
            "[workflow]\nid = \"wf\"\nname = \"WF\"\ndelivery = \"reports\"\n\
             \n[[step]]\nid = \"s1\"\nname = \"S\"\ntask = \"do it\"\nreport = \"summarize\"\n",
        )
        .expect("reports delivery must parse");
        assert_eq!(def.workflow.delivery, DeliveryMode::Reports);
        assert_eq!(def.authoring_issues(), Vec::<String>::new());
    }

    #[test]
    fn reports_delivery_without_any_report_step_is_flagged() {
        let def = WorkflowDefinition::from_toml(
            "[workflow]\nid = \"wf\"\nname = \"WF\"\ndelivery = \"reports\"\n\
             \n[[step]]\nid = \"s1\"\nname = \"S\"\ntask = \"do it\"\n",
        )
        .expect("fixture must parse");
        assert!(
            def.authoring_issues()
                .iter()
                .any(|i| i.contains("delivery = \"reports\"")),
            "an all-silent reports workflow must be called out at save time"
        );
    }

    /// A tool step never runs a model, so its `report` is never produced:
    /// it must not silence the reports-mode mute warning, and it earns its
    /// own dead-report warning.
    #[test]
    fn a_report_on_a_tool_step_is_dead_and_does_not_satisfy_reports_delivery() {
        let def = WorkflowDefinition::from_toml(
            "[workflow]\nid = \"wf\"\nname = \"WF\"\ndelivery = \"reports\"\n\
             \n[[step]]\nid = \"s1\"\nname = \"S\"\ntask = \"{}\"\n\
             tool = \"skill.tool\"\nreport = \"summarize\"\n",
        )
        .expect("fixture must parse");
        let issues = def.authoring_issues();
        assert!(
            issues.iter().any(|i| i.contains("runs as a tool call")),
            "dead report on a tool step must be called out: {issues:?}"
        );
        assert!(
            issues.iter().any(|i| i.contains("delivery = \"reports\"")),
            "a tool-step report must not count as the workflow's voice: {issues:?}"
        );
    }

    const REPORTS_WF: &str = "[workflow]\nid = \"wf\"\nname = \"WF\"\ndelivery = \"reports\"\n";

    fn reports_wf(steps: &str) -> Result<WorkflowDefinition> {
        WorkflowDefinition::from_toml(&format!("{REPORTS_WF}{steps}"))
    }

    #[test]
    fn report_render_defaults_to_agent_and_parses_template() {
        let def = wf("\n[[step]]\nid = \"a\"\nname = \"a\"\ntask = \"t\"\nreport = \"r\"\n");
        assert_eq!(def.steps[0].report_render, ReportRender::Agent);
        assert_eq!(
            def.steps[0].delivered_report(),
            Some((ReportRender::Agent, "r"))
        );
        let def = wf(
            "\n[[step]]\nid = \"a\"\nname = \"a\"\ntask = \"t\"\nreport = \"r\"\n\
             report_render = \"template\"\n",
        );
        assert_eq!(
            def.steps[0].delivered_report(),
            Some((ReportRender::Template, "r"))
        );
    }

    #[test]
    fn template_render_without_a_report_is_rejected() {
        let err = reports_wf(
            "\n[[step]]\nid = \"a\"\nname = \"a\"\ntask = \"t\"\nreport_render = \"template\"\n",
        )
        .unwrap_err();
        assert!(err.to_string().contains("declares no report"), "{err}");
    }

    #[test]
    fn template_syntax_error_is_rejected_naming_the_step() {
        let err = reports_wf(
            "\n[[step]]\nid = \"a\"\nname = \"a\"\ntask = \"t\"\nreport_render = \"template\"\n\
             report = \"{% if x %}open\"\n",
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("Step 'a' report template"),
            "{err}"
        );
    }

    #[test]
    fn report_only_step_parses_without_a_task_and_speaks_for_reports_delivery() {
        let def = reports_wf(
            "\n[[step]]\nid = \"say\"\nname = \"Say\"\nexit = true\n\
             report_render = \"template\"\nreport = \"hello\"\n",
        )
        .expect("report-only step must parse");
        assert_eq!(def.steps[0].kind(), StepKind::Report);
        assert_eq!(def.authoring_issues(), Vec::<String>::new());
    }

    #[test]
    fn report_only_step_rejects_what_needs_a_model_turn() {
        let base = "\n[[step]]\nid = \"say\"\nname = \"Say\"\nreport_render = \"template\"\n\
                    report = \"hello\"\n";
        for (extra, needle) in [
            ("[[step.outputs]]\nname = \"x\"\n", "cannot declare outputs"),
            (
                "[[step.phases]]\nname = \"p\"\ntask = \"t\"\n",
                "cannot declare phases",
            ),
            ("engine = \"claude-code\"\n", "cannot override the engine"),
            (
                "[[step.branches]]\nwhen = \"ok\"\ngoto = \"say\"\n",
                "without a pause",
            ),
        ] {
            let err = reports_wf(&format!("{base}{extra}")).unwrap_err();
            assert!(err.to_string().contains(needle), "{extra}: {err}");
        }
    }

    #[test]
    fn report_only_step_may_pause_and_let_a_human_choose_a_branch() {
        reports_wf(
            "\n[[step]]\nid = \"say\"\nname = \"Say\"\npause = \"gate\"\n\
             report_render = \"template\"\nreport = \"hello\"\n\
             [[step.branches]]\nwhen = \"ok\"\ngoto = \"say\"\n",
        )
        .expect("a paused report-only step routes by the human's outcome");
    }

    #[test]
    fn a_step_with_no_task_and_no_tool_needs_a_rendered_report() {
        let err = reports_wf("\n[[step]]\nid = \"say\"\nname = \"Say\"\n").unwrap_err();
        assert!(
            err.to_string().contains("must carry a rendered report"),
            "{err}"
        );
        let err = reports_wf("\n[[step]]\nid = \"say\"\nname = \"Say\"\nreport = \"hello\"\n")
            .unwrap_err();
        assert!(err.to_string().contains("set report_render"), "{err}");
    }

    #[test]
    fn a_template_report_on_a_tool_step_is_delivered_and_may_read_its_own_outputs() {
        let def = reports_wf(
            "\n[[step]]\nid = \"s1\"\nname = \"S\"\ntask = \"{}\"\ntool = \"skill.tool\"\n\
             report_render = \"template\"\nreport = \"{{ steps.s1.count }} done\"\n\
             [[step.outputs]]\nname = \"count\"\n",
        )
        .expect("fixture must parse");
        assert_eq!(def.authoring_issues(), Vec::<String>::new());
    }

    #[test]
    fn a_dead_report_warning_points_at_template_rendering() {
        let def = reports_wf(
            "\n[[step]]\nid = \"s1\"\nname = \"S\"\ntask = \"{}\"\ntool = \"skill.tool\"\n\
             report = \"summarize\"\n",
        )
        .expect("fixture must parse");
        assert!(def
            .authoring_issues()
            .iter()
            .any(|i| i.contains("set report_render = \"template\"")));
    }

    #[test]
    fn a_template_report_referencing_an_unknown_step_is_flagged_once() {
        let def = wf(
            "\n[[step]]\nid = \"a\"\nname = \"a\"\ntask = \"t\"\nreport_render = \"template\"\n\
             report = \"{% for t in steps.ghost.items %}{{ t.k | upper }}{% endfor %}\"\n",
        );
        let issues = def.authoring_issues();
        assert!(
            issues.iter().any(|i| i.contains("no step 'ghost' exists")),
            "{issues:?}"
        );
        assert_eq!(
            issues.len(),
            1,
            "loop variables and filters are not references: {issues:?}"
        );
    }

    #[test]
    fn a_template_report_may_read_a_step_some_path_skips_but_not_an_unknown_output() {
        let skippable = "\n[[step]]\nid = \"a\"\nname = \"a\"\ntask = \"t\"\ngoto = \"join\"\n\
             [[step.branches]]\nwhen = \"detour\"\ngoto = \"b\"\n\
             \n[[step]]\nid = \"b\"\nname = \"b\"\ntask = \"t\"\n[[step.outputs]]\nname = \"x\"\n";
        for report in [
            "{% if steps.b %}{{ steps.b.x }}{% endif %}",
            "{{ steps.b.x }}",
            "{{ steps[\\\"b\\\"].x }}",
        ] {
            let def = wf(&format!(
                "{skippable}\n[[step]]\nid = \"join\"\nname = \"join\"\n\
                 report_render = \"template\"\nreport = \"{report}\"\n"
            ));
            assert_eq!(def.authoring_issues(), Vec::<String>::new(), "{report}");
        }
        let def = wf(&format!(
            "{skippable}\n[[step]]\nid = \"join\"\nname = \"join\"\nreport_render = \"template\"\n\
             report = \"{{{{ steps.b.nope }}}}\"\n"
        ));
        assert!(def
            .authoring_issues()
            .iter()
            .any(|i| i.contains("declares no output named 'nope'")));
    }

    #[test]
    fn provenance_keys_resolve_without_being_declared() {
        let steps = "\n[[step]]\nid = \"review\"\nname = \"r\"\ntask = \"t\"\n\
             [[step.outputs]]\nname = \"verdict\"\n";
        let def = wf(&format!(
            "{steps}\n[[step]]\nid = \"gate\"\nname = \"g\"\n\
             task = \"{{{{steps.review._engine}}}} {{{{?steps.review._model}}}} {{{{?steps.none._engine}}}}\"\n"
        ));
        assert_eq!(def.authoring_issues(), Vec::<String>::new());
        let def = wf(&format!(
            "{steps}\n[[step]]\nid = \"gate\"\nname = \"g\"\ntask = \"{{{{steps.review._nope}}}}\"\n"
        ));
        assert!(def
            .authoring_issues()
            .iter()
            .any(|i| i.contains("declares no output named '_nope'")));
    }

    #[test]
    fn a_report_only_step_referencing_its_own_output_is_flagged() {
        let def = wf(
            "\n[[step]]\nid = \"say\"\nname = \"Say\"\nreport_render = \"template\"\n\
             report = \"{{ steps.say.x }}\"\n",
        );
        assert!(def
            .authoring_issues()
            .iter()
            .any(|i| i.contains("declares no output named 'x'")));
    }

    #[test]
    fn secret_in_template_report_rejected_by_root() {
        for reference in ["{{ token }}", "{{ token.value }}"] {
            let err = WorkflowDefinition::from_toml(&format!(
                "[workflow]\nid = \"wf\"\nname = \"WF\"\n\
                 [[variables]]\nid = \"token\"\ntype = \"secret\"\n\
                 value = \"[[vault:agent:k]]\"\n\
                 \n[[step]]\nid = \"a\"\nname = \"a\"\ntask = \"t\"\nreport_render = \"template\"\n\
                 report = \"{reference}\"\n"
            ))
            .unwrap_err();
            assert!(
                err.to_string()
                    .contains("secret variable 'token' must not be referenced"),
                "{reference}: {err}"
            );
        }
    }

    #[test]
    fn declared_variable_satisfies_a_bare_variable() {
        let def = wf("[[variables]]\nid = \"tone\"\nvalue = \"terse\"\n\
             \n[[step]]\nid = \"a\"\nname = \"a\"\ntask = \"{{tone}}\"\n");
        assert_eq!(def.authoring_issues(), Vec::<String>::new());
    }

    #[test]
    fn trigger_payload_alone_does_not_satisfy_a_bare_variable() {
        let def = WorkflowDefinition::from_toml(
            "[workflow]\nid = \"wf\"\nname = \"WF\"\n             \n[[trigger]]\ntype = \"schedule\"\ncron = \"0 8 * * 1-5\"\n             payload = { channel_id = \"C1\" }\n             \n[[step]]\nid = \"a\"\nname = \"a\"\ntask = \"post to {{channel_id}}\"\n",
        )
        .unwrap();
        assert!(
            !def.authoring_issues().is_empty(),
            "a manual start carries no trigger payload, so this must not be treated as guaranteed"
        );
    }

    /// The live harbor definition after its branch was removed and its
    /// channel hardcoded: a linear chain whose every reference is a declared
    /// output of a dominator. Guards against the analysis rejecting a workflow
    /// that is in fact safe.
    #[test]
    fn interop_triage_linear_shape_is_accepted() {
        let def = wf_of(&[
            step(
                "fetch_and_cluster",
                "goto = \"check_known\"\n[[step.outputs]]\nname = \"clusters\"\n\
             [[step.outputs]]\nname = \"rate_limited\"\n",
            ),
            step(
                "check_known",
                "goto = \"classify\"\n[[step.outputs]]\nname = \"known_results\"\n\
             [[step.outputs]]\nname = \"unknown_clusters\"\n",
            ),
            step(
                "classify",
                "goto = \"ticket\"\n[[step.outputs]]\nname = \"ticket_items\"\n\
             [[step.outputs]]\nname = \"noise_items\"\n\
             [[step.outputs]]\nname = \"uncertain_items\"\n",
            ),
            step(
                "ticket",
                "goto = \"report\"\n[[step.outputs]]\nname = \"ticket_results\"\n",
            ),
            String::from(
                "\n[[step]]\nid = \"report\"\nname = \"report\"\nexit = true\n\
              task = \"\"\"{{steps.check_known.known_results}} {{steps.ticket.ticket_results}} \
              {{steps.classify.uncertain_items}} {{steps.classify.noise_items}} \
              {{steps.fetch_and_cluster.rate_limited}}\"\"\"\n\
              [[step.outputs]]\nname = \"triage_summary\"\n",
            ),
        ]);
        assert_eq!(
            def.authoring_issues(),
            Vec::<String>::new(),
            "the live definition must keep saving"
        );
    }

    #[test]
    fn interop_triage_branch_shape_is_rejected() {
        let def = wf_of(&[step("fetch", "[[step.outputs]]\nname = \"clusters\"\n"), step(
                "check_known",
                "[[step.branches]]\nwhen = \"all_known\"\ngoto = \"report\"\n\
                 [[step.branches]]\nwhen = \"unknown_found\"\ngoto = \"classify\"\n",
            ), step("classify", "[[step.outputs]]\nname = \"uncertain_items\"\n"), step("ticket", "[[step.outputs]]\nname = \"ticket_results\"\n"), String::from("\n[[step]]\nid = \"report\"\nname = \"report\"\n                task = \"{{steps.ticket.ticket_results}} {{steps.classify.uncertain_items}}\"\nexit = true\n")]);
        let issues = def.authoring_issues();
        assert_eq!(
            issues.len(),
            2,
            "both skipped-step references must be reported: {issues:?}"
        );
    }

    fn wf_with(variables: &str) -> Result<WorkflowDefinition> {
        WorkflowDefinition::from_toml(&format!(
            "[workflow]\nid = \"wf\"\nname = \"WF\"\n{variables}{STEP}"
        ))
    }

    #[test]
    fn parses_typed_variables() {
        let def = wf_with(
            "[[variables]]\nid = \"portal_email\"\nvalue = \"a@b.com\"\n\
             [[variables]]\nid = \"portal_pw\"\ntype = \"secret\"\n\
             value = \"[[vault:agent_user:portal_pw]]\"\nallowed_hosts = [\"x.example.com\"]\n",
        )
        .unwrap();
        assert_eq!(def.variables.len(), 2);
        assert!(!def.variables[0].is_secret());
        assert!(def.variables[1].is_secret());
        assert_eq!(def.variables[1].allowed_hosts, vec!["x.example.com"]);
    }

    #[test]
    fn secret_value_must_be_scoped_marker() {
        assert!(
            wf_with("[[variables]]\nid = \"p\"\ntype = \"secret\"\nvalue = \"hunter2\"\n").is_err()
        );
        assert!(wf_with(
            "[[variables]]\nid = \"p\"\ntype = \"secret\"\nvalue = \"[[vault:bogus:k]]\"\n"
        )
        .is_err());
        assert!(wf_with(
            "[[variables]]\nid = \"p\"\ntype = \"secret\"\nvalue = \"[[vault:agent:k]]\"\n"
        )
        .is_ok());
    }

    #[test]
    fn plain_value_may_not_be_a_vault_ref() {
        assert!(
            wf_with("[[variables]]\nid = \"p\"\nvalue = \"[[vault:agent:k]]\"\n").is_err(),
            "a vault ref in a non-secret variable must be rejected"
        );
    }

    #[test]
    fn allowed_hosts_is_secret_only() {
        assert!(
            wf_with("[[variables]]\nid = \"p\"\nvalue = \"v\"\nallowed_hosts = [\"x.com\"]\n")
                .is_err()
        );
    }

    #[test]
    fn duplicate_variable_id_rejected() {
        assert!(wf_with(
            "[[variables]]\nid = \"p\"\nvalue = \"a\"\n[[variables]]\nid = \"p\"\nvalue = \"b\"\n"
        )
        .is_err());
    }

    #[test]
    fn secret_in_report_rejected() {
        let toml = "[workflow]\nid = \"wf\"\nname = \"WF\"\n\
            [[variables]]\nid = \"portal_pw\"\ntype = \"secret\"\nvalue = \"[[vault:agent:k]]\"\n\
            [[step]]\nid = \"s1\"\nname = \"S\"\ntask = \"t\"\nreport = \"pw is {{portal_pw}}\"\n";
        assert!(WorkflowDefinition::from_toml(toml).is_err());
    }

    #[test]
    fn pilot_phase_rejects_unknown_field() {
        // Regression: `branches = [...]` placed inside a [[step.phases]] block
        // (instead of the parent [[step]]) used to be silently ignored, leaving
        // the step with no branches and breaking outcome routing. Now rejected
        // at parse time via #[serde(deny_unknown_fields)].
        let toml = r#"
[workflow]
id = "t"
name = "t"

[[step]]
id = "review"
name = "Review"
task = "orchestrated by phases"
engine = "claude-pilot"

[[step.phases]]
name = "verdict"
task = "decide"
branches = [{ when = "approved", goto = "next" }]
"#;
        let err = format!("{:#}", WorkflowDefinition::from_toml(toml).unwrap_err());
        assert!(
            err.to_lowercase().contains("branches"),
            "error must name the rejected field: {err}"
        );
    }

    const MINIMAL_WORKFLOW: &str = r#"
[workflow]
id = "test"
name = "Test Workflow"

[[step]]
id = "one"
name = "Step One"
task = "Do the first thing."

[[step]]
id = "two"
name = "Step Two"
task = "Do the second thing."
"#;

    const BRANCHING_WORKFLOW: &str = r#"
[workflow]
id = "branching"
name = "Branching Test"

[workflow.context]
ticket_id = { required = true, description = "The ticket" }

[[step]]
id = "investigate"
name = "Investigate"
task = "Look into it."
branches = [
  { when = "already-fixed", goto = "close" },
  { when = "needs-work", goto = "implement" },
]

[[step]]
id = "implement"
name = "Implement"
task = "Write the code."

[[step]]
id = "merge"
name = "Merge"
task = "Merge the PR."

[[step]]
id = "close"
name = "Close"
exit = true
task = "Close the ticket."
"#;

    #[test]
    fn parse_minimal() {
        let def = WorkflowDefinition::from_toml(MINIMAL_WORKFLOW).unwrap();
        assert_eq!(def.workflow.id, "test");
        assert_eq!(def.steps.len(), 2);
        assert_eq!(def.steps[0].id, "one");
        assert_eq!(def.steps[1].task, "Do the second thing.");
    }

    #[test]
    fn parse_branching() {
        let def = WorkflowDefinition::from_toml(BRANCHING_WORKFLOW).unwrap();
        assert_eq!(def.steps[0].branches.len(), 2);
        assert_eq!(def.steps[0].branches[0].when, "already-fixed");
        assert_eq!(def.steps[0].branches[0].goto, "close");
        assert!(def.workflow.context.contains_key("ticket_id"));
        assert!(def.workflow.context["ticket_id"].required);
    }

    #[test]
    fn parse_with_pause_and_goto() {
        let toml = r#"
[workflow]
id = "pause-test"
name = "Pause Test"

[[step]]
id = "work"
name = "Do Work"
task = "Work on it."
pause = "awaiting-review"
branches = [
  { when = "changes-requested", goto = "work" },
]

[[step]]
id = "fixup"
name = "Fix Up"
task = "Apply fixes."
goto = "done"

[[step]]
id = "done"
name = "Done"
exit = true
task = "All done."
"#;
        let def = WorkflowDefinition::from_toml(toml).unwrap();
        assert_eq!(def.steps[0].pause.as_deref(), Some("awaiting-review"));
        assert_eq!(def.steps[1].goto.as_deref(), Some("done"));
        assert!(def.steps[2].exit);
    }

    #[test]
    fn parse_with_skill_and_agent() {
        let toml = r#"
[workflow]
id = "skill-test"
name = "Skill Test"
agent = "developer"

[[step]]
id = "review"
name = "Review"
task = "Review the PR."
skill = "review-pr"
agent = "reviewer"
"#;
        let def = WorkflowDefinition::from_toml(toml).unwrap();
        assert_eq!(def.workflow.agent.as_deref(), Some("developer"));
        assert_eq!(def.steps[0].skill.as_deref(), Some("review-pr"));
        assert_eq!(def.steps[0].agent.as_deref(), Some("reviewer"));
    }

    #[test]
    fn parse_with_handoff() {
        let toml = r#"
[workflow]
id = "handoff-test"
name = "Handoff Test"

[[step]]
id = "escalate"
name = "Escalate"
task = "Escalate to product."
exit = true
handoff = "escalation_to_product"
"#;
        let def = WorkflowDefinition::from_toml(toml).unwrap();
        assert!(def.steps[0].exit);
        assert_eq!(
            def.steps[0].handoff.as_deref(),
            Some("escalation_to_product")
        );
    }

    #[test]
    fn reject_empty_workflow() {
        let toml = r#"
[workflow]
id = "empty"
name = "Empty"
"#;
        assert!(WorkflowDefinition::from_toml(toml).is_err());
    }

    #[test]
    fn reject_duplicate_step_ids() {
        let toml = r#"
[workflow]
id = "dups"
name = "Dups"

[[step]]
id = "same"
name = "First"
task = "Do it."

[[step]]
id = "same"
name = "Second"
task = "Do it again."
"#;
        assert!(WorkflowDefinition::from_toml(toml).is_err());
    }

    #[test]
    fn reject_branch_to_unknown_step() {
        let toml = r#"
[workflow]
id = "bad-branch"
name = "Bad Branch"

[[step]]
id = "start"
name = "Start"
task = "Go."
branches = [
  { when = "done", goto = "nonexistent" },
]
"#;
        assert!(WorkflowDefinition::from_toml(toml).is_err());
    }

    #[test]
    fn reject_goto_to_unknown_step() {
        let toml = r#"
[workflow]
id = "bad-goto"
name = "Bad Goto"

[[step]]
id = "start"
name = "Start"
task = "Go."
goto = "nowhere"
"#;
        assert!(WorkflowDefinition::from_toml(toml).is_err());
    }

    #[test]
    fn reject_unknown_engine_and_name_the_allowed_set() {
        let err = WorkflowDefinition::from_toml(
            "[workflow]\nid = \"wf\"\nname = \"WF\"\n\
             \n[[step]]\nid = \"a\"\nname = \"a\"\ntask = \"t\"\nengine = \"claude_code\"\n",
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("claude_code") && err.contains("claude-code"),
            "{err}"
        );
    }

    #[test]
    fn a_codex_step_takes_the_coding_fields() {
        let def = WorkflowDefinition::from_toml(
            "[workflow]\nid = \"wf\"\nname = \"WF\"\n\
             \n[[step]]\nid = \"a\"\nname = \"a\"\ntask = \"t\"\nengine = \"codex\"\n\
             model = \"astra\"\neffort = \"xhigh\"\ntimeout_secs = 900\n\
             fallback = [\"claude_code:opus\", \"claude_code_bedrock:powerful\"]\n",
        )
        .unwrap();
        assert_eq!(def.steps[0].engine, "codex");
        assert!(is_coding_engine(&def.steps[0].engine) && is_coding_engine("claude-code"));
        assert!(!is_coding_engine("default") && !is_coding_engine("claude-pilot"));
        let err = WorkflowDefinition::from_toml(
            "[workflow]\nid = \"wf\"\nname = \"WF\"\n\
             \n[[step]]\nid = \"a\"\nname = \"a\"\ntask = \"t\"\ntimeout_secs = 5\n",
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("codex") && err.contains("claude-code"),
            "{err}"
        );
    }

    #[test]
    fn reject_unknown_output_validate_rule() {
        let err = WorkflowDefinition::from_toml(
            "[workflow]\nid = \"wf\"\nname = \"WF\"\n\
             \n[[step]]\nid = \"a\"\nname = \"a\"\ntask = \"t\"\n\
             outputs = [{ name = \"n\", validate = \"regex\" }]\n",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("regex") && err.contains("not_empty"), "{err}");
    }

    #[test]
    fn accept_every_documented_engine() {
        for engine in ["default", "claude-code", "claude-pilot"] {
            let toml = format!(
                "[workflow]\nid = \"wf\"\nname = \"WF\"\n\
                 \n[[step]]\nid = \"a\"\nname = \"a\"\ntask = \"t\"\nengine = \"{engine}\"\n"
            );
            assert!(
                WorkflowDefinition::from_toml(&toml).is_ok(),
                "{engine} must parse"
            );
        }
    }

    #[test]
    fn reject_exit_with_goto() {
        let toml = r#"
[workflow]
id = "exit-goto"
name = "Exit Goto"

[[step]]
id = "bad"
name = "Bad"
task = "Can't exit and goto."
exit = true
goto = "other"

[[step]]
id = "other"
name = "Other"
task = "Other."
"#;
        assert!(WorkflowDefinition::from_toml(toml).is_err());
    }

    #[test]
    fn get_step_by_id() {
        let def = WorkflowDefinition::from_toml(MINIMAL_WORKFLOW).unwrap();
        assert_eq!(def.get_step("two").unwrap().name, "Step Two");
        assert!(def.get_step("missing").is_none());
    }

    #[test]
    fn step_index() {
        let def = WorkflowDefinition::from_toml(MINIMAL_WORKFLOW).unwrap();
        assert_eq!(def.step_index("one"), Some(0));
        assert_eq!(def.step_index("two"), Some(1));
        assert_eq!(def.step_index("missing"), None);
    }

    #[test]
    fn default_version_applied() {
        let def = WorkflowDefinition::from_toml(MINIMAL_WORKFLOW).unwrap();
        assert_eq!(def.workflow.version, "1.0");
    }

    #[test]
    fn parse_without_triggers() {
        let def = WorkflowDefinition::from_toml(MINIMAL_WORKFLOW).unwrap();
        assert!(def.triggers.is_empty());
    }

    #[test]
    fn parse_with_triggers() {
        let toml = r#"
[workflow]
id = "triggered"
name = "Triggered Workflow"

[[trigger]]
source = "linear"
event_type = "Issue.update"
secret = "[[vault:linear/wh]]"

[trigger.match]
"data.state.name" = "Todo"

[[trigger]]
source = "github"
event_type = "issues"
secret = "[[vault:github/wh]]"

[trigger.match]
"action" = "labeled"
"label.name" = "bug"

[[step]]
id = "start"
name = "Start"
task = "Do the thing."
"#;
        let def = WorkflowDefinition::from_toml(toml).unwrap();
        assert_eq!(def.triggers.len(), 2);
        match &def.triggers[0] {
            TriggerDefinition::Webhook {
                source,
                event_type,
                match_rules,
                secret,
            } => {
                assert_eq!(source, "linear");
                assert_eq!(event_type, "Issue.update");
                assert_eq!(
                    match_rules.get("data.state.name"),
                    Some(&"Todo".to_string())
                );
                assert_eq!(secret.as_deref(), Some("[[vault:linear/wh]]"));
            }
            _ => panic!("expected Webhook variant"),
        }
        match &def.triggers[1] {
            TriggerDefinition::Webhook {
                source,
                match_rules,
                ..
            } => {
                assert_eq!(source, "github");
                assert_eq!(match_rules.len(), 2);
            }
            _ => panic!("expected Webhook variant"),
        }
    }

    #[test]
    fn parse_trigger_with_secret() {
        let toml = r#"
[workflow]
id = "verified"
name = "Verified Workflow"

[[trigger]]
source = "github"
event_type = "pull_request"
secret = "[[vault:github/webhook1]]"

[trigger.match]
"repository.full_name" = "contoso/monorepo"

[[step]]
id = "start"
name = "Start"
task = "Do the thing."
"#;
        let def = WorkflowDefinition::from_toml(toml).unwrap();
        match &def.triggers[0] {
            TriggerDefinition::Webhook { secret, .. } => {
                assert_eq!(secret.as_deref(), Some("[[vault:github/webhook1]]"));
            }
            _ => panic!("expected Webhook variant"),
        }
    }

    #[test]
    fn parse_with_outputs() {
        let toml = r#"
[workflow]
id = "output-test"
name = "Output Test"

[[step]]
id = "implement"
name = "Implement"
task = "Do the work."
outputs = [
  { name = "pr_number", description = "PR number created" },
  { name = "branch_name", description = "Feature branch" },
]

[[step]]
id = "review"
name = "Review"
task = "Review the work."
"#;
        let def = WorkflowDefinition::from_toml(toml).unwrap();
        assert_eq!(def.steps[0].outputs.len(), 2);
        assert_eq!(def.steps[0].outputs[0].name, "pr_number");
        assert_eq!(def.steps[0].outputs[0].description, "PR number created");
        assert_eq!(def.steps[0].outputs[1].name, "branch_name");
        // Step without outputs should have empty vec
        assert!(def.steps[1].outputs.is_empty());
    }

    #[test]
    fn parse_trigger_without_match_rules() {
        let toml = r#"
[workflow]
id = "catch-all"
name = "Catch All"

[[trigger]]
source = "github"
event_type = "push"
secret = "[[vault:github/push]]"

[[step]]
id = "start"
name = "Start"
task = "Do it."
"#;
        let def = WorkflowDefinition::from_toml(toml).unwrap();
        assert_eq!(def.triggers.len(), 1);
        match &def.triggers[0] {
            TriggerDefinition::Webhook { match_rules, .. } => {
                assert!(match_rules.is_empty());
            }
            _ => panic!("expected Webhook variant"),
        }
    }

    #[test]
    fn parse_with_max_rounds() {
        let toml = r#"
[workflow]
id = "rounds-test"
name = "Rounds Test"

[[step]]
id = "limited"
name = "Limited Step"
task = "Do limited work."
max_rounds = 10

[[step]]
id = "unlimited"
name = "Unlimited Step"
task = "Do unlimited work."
"#;
        let def = WorkflowDefinition::from_toml(toml).unwrap();
        assert_eq!(def.steps[0].max_rounds, Some(10));
        assert_eq!(def.steps[1].max_rounds, None);
    }

    #[test]
    fn parse_with_model() {
        let toml = r#"
[workflow]
id = "model-test"
name = "Model Test"

[[step]]
id = "cheap"
name = "Cheap Step"
task = "Do cheap work."
model = "claude-sonnet-4"

[[step]]
id = "expensive"
name = "Expensive Step"
task = "Do expensive work."
model = "claude-opus-4-6"

[[step]]
id = "default"
name = "Default Step"
task = "Do default work."
"#;
        let def = WorkflowDefinition::from_toml(toml).unwrap();
        assert_eq!(def.steps[0].model.as_deref(), Some("claude-sonnet-4"));
        assert_eq!(def.steps[1].model.as_deref(), Some("claude-opus-4-6"));
        assert_eq!(def.steps[2].model, None);
    }
    #[test]
    fn parse_with_report() {
        let toml = r#"
[workflow]
id = "report-test"
name = "Report Test"

[[step]]
id = "design"
name = "Design"
task = "Design the feature."
pause = "awaiting-review"
report = "Summarise in 3-5 bullet points: what will change, key files, and any risks."

[[step]]
id = "implement"
name = "Implement"
task = "Implement the feature."
"#;
        let def = WorkflowDefinition::from_toml(toml).unwrap();
        assert_eq!(
            def.steps[0].report.as_deref(),
            Some("Summarise in 3-5 bullet points: what will change, key files, and any risks.")
        );
        // Step without report should default to None
        assert_eq!(def.steps[1].report, None);
    }

    #[test]
    fn parse_with_context_eviction() {
        let toml = r#"
[workflow]
id = "eviction-test"
name = "Eviction Test"

[[step]]
id = "long-step"
name = "Long Step"
task = "Do long work."
context_eviction_interval = 10
context_eviction_keep_last = 5

[[step]]
id = "normal-step"
name = "Normal Step"
task = "Do normal work."
"#;
        let def = WorkflowDefinition::from_toml(toml).unwrap();
        assert_eq!(def.steps[0].context_eviction_interval, Some(10));
        assert_eq!(def.steps[0].context_eviction_keep_last, 5);
        // Step without eviction config should default to None / 3
        assert_eq!(def.steps[1].context_eviction_interval, None);
        assert_eq!(def.steps[1].context_eviction_keep_last, 3);
    }

    #[test]
    fn parse_with_engine() {
        let toml = r#"
[workflow]
id = "engine-test"
name = "Engine Test"

[[step]]
id = "plan"
name = "Plan"
task = "Plan the work."

[[step]]
id = "implement"
name = "Implement"
task = "Do the work."
engine = "claude-code"
max_rounds = 100
"#;
        let def = WorkflowDefinition::from_toml(toml).unwrap();
        assert_eq!(def.steps[0].engine, "default");
        assert_eq!(def.steps[0].effort, None);
        assert_eq!(def.steps[1].engine, "claude-code");
        assert_eq!(def.steps[1].max_rounds, Some(100));
        assert_eq!(def.steps[1].effort, None);
    }

    #[test]
    fn parse_with_effort() {
        let toml = r#"
[workflow]
id = "effort-test"
name = "Effort Test"

[[step]]
id = "implement"
name = "Implement"
task = "Do the work."
engine = "claude-code"
model = "sonnet"
effort = "high"
"#;
        let def = WorkflowDefinition::from_toml(toml).unwrap();
        assert_eq!(def.steps[0].engine, "claude-code");
        assert_eq!(def.steps[0].model.as_deref(), Some("sonnet"));
        assert_eq!(def.steps[0].effort.as_deref(), Some("high"));
        assert_eq!(def.steps[0].timeout_secs, None);
        assert!(def.steps[0].allowed_tools.is_empty());
    }

    #[test]
    fn a_claude_code_step_carries_its_wall_clock_and_tool_set() {
        let toml = r#"
[workflow]
id = "cc-fields"
name = "Fields"

[[step]]
id = "implement"
name = "Implement"
task = "Do the work."
engine = "claude-code"
timeout_secs = 5400
allowed_tools = ["Read", "Edit", "Bash", "WebFetch"]
"#;
        let def = WorkflowDefinition::from_toml(toml).unwrap();
        assert_eq!(def.steps[0].timeout_secs, Some(5400));
        assert!(def.steps[0].fallback.is_empty());
        assert_eq!(
            def.steps[0].allowed_tools,
            vec!["Read", "Edit", "Bash", "WebFetch"]
        );
    }

    #[test]
    fn a_failure_report_must_compile_and_read_no_secret() {
        let def = |extra: &str| {
            WorkflowDefinition::from_toml(&format!(
                "[workflow]\nid = \"w\"\nname = \"W\"\n{extra}\n[[step]]\nid = \"s\"\nname = \"S\"\ntask = \"Do.\"\n"
            ))
        };
        assert_eq!(def("").unwrap().workflow.failure_report, None);
        let ok = def("failure_report = \"{{ steps.pick.ticket_id }} at {{ step }}\"").unwrap();
        assert!(ok.workflow.failure_report.is_some());
        let err = def("failure_report = \"{% if %}\"")
            .unwrap_err()
            .to_string();
        assert!(err.contains("failure_report does not compile"), "{err}");
        let err = def("failure_report = \"{{ token.value }}\"\n\
             [[variables]]\nid = \"token\"\ntype = \"secret\"\nvalue = \"[[vault:agent:k]]\"")
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("secret variable 'token' must not be referenced from failure_report"),
            "{err}"
        );
    }

    fn requiring(requires: &str) -> Result<WorkflowDefinition> {
        WorkflowDefinition::from_toml(&format!(
            "[workflow]\nid = \"pr-review\"\nname = \"Review\"\n{requires}\n\n\
             [[step]]\nid = \"run\"\nname = \"R\"\ntask = \"go\"\nexit = true\n"
        ))
    }

    #[test]
    fn requires_names_skills_with_semver_requirements_checked_at_parse() {
        let def = requiring(
            "requires = { \"ticket-picker\" = \">=0.10.0\", \"fieldops-doctor\" = \">=0.1.0, <1.0.0\" }",
        )
        .unwrap();
        assert_eq!(
            def.workflow.requires,
            BTreeMap::from([
                ("fieldops-doctor".to_string(), ">=0.1.0, <1.0.0".to_string()),
                ("ticket-picker".to_string(), ">=0.10.0".to_string()),
            ])
        );
        let json = serde_json::to_value(&def).unwrap();
        assert_eq!(json["workflow"]["requires"]["ticket-picker"], ">=0.10.0");
        assert!(
            serde_json::to_value(requiring("").unwrap()).unwrap()["workflow"]
                .get("requires")
                .is_none()
        );

        let err = requiring("requires = { \"ticket-picker\" = \"the latest\" }")
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("requires ticket-picker `the latest`, which is not a semver requirement"),
            "{err}"
        );
        let err = requiring("requires = { \"Ticket Picker\" = \">=0.10.0\" }")
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("requires `Ticket Picker`, which is not a skill name"),
            "{err}"
        );
    }

    #[test]
    fn unmet_requirements_name_each_skill_missing_older_or_unreadable() {
        let def = requiring(
            "requires = { \"fieldops-doctor\" = \">=0.1.0\", \"slack-helper\" = \">=1.2.0\", \"ticket-picker\" = \">=0.10.0\", \"triage\" = \"^2\" }",
        )
        .unwrap();
        let installed = HashMap::from([
            ("ticket-picker", "0.9.0"),
            ("slack-helper", "1.3.1"),
            ("triage", "two"),
        ]);
        assert_eq!(
            def.workflow
                .unmet_requirements(|skill| installed.get(skill).copied()),
            [
                "requires fieldops-doctor >=0.1.0; not installed",
                "requires ticket-picker >=0.10.0; installed 0.9.0",
                "requires triage ^2; installed two, which is not a semver version",
            ]
        );
        let current = HashMap::from([
            ("ticket-picker", "0.10.0"),
            ("slack-helper", "1.2.0"),
            ("fieldops-doctor", "0.1.3"),
            ("triage", "2.4.0"),
        ]);
        assert!(def
            .workflow
            .unmet_requirements(|skill| current.get(skill).copied())
            .is_empty());
        assert!(requiring("")
            .unwrap()
            .workflow
            .unmet_requirements(|_| None)
            .is_empty());
    }

    #[test]
    fn max_concurrent_defaults_to_one_and_refuses_zero() {
        let def = |extra: &str| {
            WorkflowDefinition::from_toml(&format!(
                "[workflow]\nid = \"w\"\nname = \"W\"\n{extra}\n[[step]]\nid = \"s\"\nname = \"S\"\ntask = \"Do.\"\n"
            ))
        };
        assert_eq!(def("").unwrap().workflow.max_concurrent, 1);
        assert_eq!(
            def("max_concurrent = 4").unwrap().workflow.max_concurrent,
            4
        );
        let err = def("max_concurrent = 0").unwrap_err().to_string();
        assert!(err.contains("max_concurrent = 0"), "{err}");
    }

    /// The cap applies to trigger starts too (queued at the cap), with no
    /// extra authoring rule: a triggered definition carries it unchanged.
    #[test]
    fn max_concurrent_needs_no_extra_validation_for_a_triggered_workflow() {
        let def = WorkflowDefinition::from_toml(
            r#"
[workflow]
id = "w"
name = "W"
max_concurrent = 3

[[trigger]]
source = "github"
event_type = "pull_request"
secret = "[[vault:github/pr]]"

[[step]]
id = "s"
name = "S"
task = "Do."
"#,
        )
        .unwrap();
        assert_eq!(def.workflow.max_concurrent, 3);
        assert_eq!(def.triggers.len(), 1);
    }

    #[test]
    fn claude_code_fields_are_refused_on_other_engines_and_when_empty() {
        let step = |extra: &str| {
            WorkflowDefinition::from_toml(&format!(
                r#"
[workflow]
id = "cc-fields"
name = "Fields"

[[step]]
id = "s"
name = "S"
task = "Do."
{extra}
"#
            ))
        };
        let err = step("timeout_secs = 60").unwrap_err().to_string();
        assert!(
            err.contains("timeout_secs") && err.contains("claude-code"),
            "{err}"
        );
        let err = step("allowed_tools = [\"Read\"]").unwrap_err().to_string();
        assert!(err.contains("allowed_tools"), "{err}");
        let err = step("engine = \"claude-code\"\ntimeout_secs = 0")
            .unwrap_err()
            .to_string();
        assert!(err.contains("timeout_secs = 0"), "{err}");
        let err = step("engine = \"claude-code\"\nallowed_tools = [\" \"]")
            .unwrap_err()
            .to_string();
        assert!(err.contains("blank"), "{err}");
        assert!(step("engine = \"claude-code\"\ntimeout_secs = 60").is_ok());
        let err = step("fallback = [\"claude_code:opus\"]")
            .unwrap_err()
            .to_string();
        assert!(err.contains("fallback"), "{err}");
        let err = step("engine = \"claude-code\"\nfallback = [\"model:\"]")
            .unwrap_err()
            .to_string();
        assert!(err.contains("model:"), "{err}");
        let with_chain =
            step("engine = \"claude-code\"\nfallback = [\"claude_code:opus\"]").unwrap();
        assert_eq!(with_chain.steps[0].fallback, vec!["claude_code:opus"]);
    }

    #[test]
    fn trigger_webhook_explicit_type() {
        let toml = r#"
[workflow]
id = "wh"
name = "Webhook Workflow"

[[step]]
id = "s1"
name = "Step"
task = "Do."

[[trigger]]
type = "webhook"
source = "github"
event_type = "pull_request"
secret = "[[vault:github/pr]]"
match = { "action" = "opened" }
"#;
        let def = WorkflowDefinition::from_toml(toml).unwrap();
        assert_eq!(def.triggers.len(), 1);
        match &def.triggers[0] {
            TriggerDefinition::Webhook {
                source,
                event_type,
                match_rules,
                ..
            } => {
                assert_eq!(source, "github");
                assert_eq!(event_type, "pull_request");
                assert_eq!(match_rules.get("action"), Some(&"opened".to_string()));
            }
            _ => panic!("expected Webhook variant"),
        }
    }

    #[test]
    fn trigger_webhook_without_type_defaults_to_webhook() {
        // Back-compat: workflows authored before schedule support omit `type`.
        let toml = r#"
[workflow]
id = "wh"
name = "Webhook Workflow"

[[step]]
id = "s1"
name = "Step"
task = "Do."

[[trigger]]
source = "linear"
event_type = "Issue.update"
secret = "[[vault:linear/wh]]"
"#;
        let def = WorkflowDefinition::from_toml(toml).unwrap();
        assert!(matches!(def.triggers[0], TriggerDefinition::Webhook { .. }));
    }

    #[test]
    fn trigger_schedule_parses() {
        let toml = r#"
[workflow]
id = "sch"
name = "Scheduled Workflow"

[[step]]
id = "s1"
name = "Step"
task = "Do."

[[trigger]]
type = "schedule"
cron = "0 8 * * *"
tz = "Europe/Berlin"
run_as = "U01KINGS"
"#;
        let def = WorkflowDefinition::from_toml(toml).unwrap();
        match &def.triggers[0] {
            TriggerDefinition::Schedule {
                cron,
                tz,
                run_as,
                payload,
            } => {
                assert_eq!(cron, "0 8 * * *");
                assert_eq!(tz.as_deref(), Some("Europe/Berlin"));
                assert_eq!(run_as.as_deref(), Some("U01KINGS"));
                assert!(payload.is_null());
            }
            _ => panic!("expected Schedule variant"),
        }
    }

    #[test]
    fn trigger_schedule_rejects_missing_cron() {
        let toml = r#"
[workflow]
id = "bad"
name = "Bad Schedule"

[[step]]
id = "s1"
name = "Step"
task = "Do."

[[trigger]]
type = "schedule"
tz = "UTC"
"#;
        let err = format!("{:#}", WorkflowDefinition::from_toml(toml).unwrap_err());
        assert!(
            err.contains("cron"),
            "missing-cron error should name the field: {err}"
        );
    }

    #[test]
    fn trigger_unknown_type_rejected() {
        let toml = r#"
[workflow]
id = "bad"
name = "Bad Type"

[[step]]
id = "s1"
name = "Step"
task = "Do."

[[trigger]]
type = "mystery"
cron = "* * * * *"
"#;
        assert!(WorkflowDefinition::from_toml(toml).is_err());
    }

    #[test]
    fn trigger_mixed_array_preserves_both_variants() {
        let toml = r#"
[workflow]
id = "mix"
name = "Mixed Triggers"

[[step]]
id = "s1"
name = "Step"
task = "Do."

[[trigger]]
type = "webhook"
source = "github"
event_type = "pull_request"
secret = "[[vault:github/pr]]"

[[trigger]]
type = "schedule"
cron = "0 9 * * 1"
"#;
        let def = WorkflowDefinition::from_toml(toml).unwrap();
        assert_eq!(def.triggers.len(), 2);
        assert!(matches!(def.triggers[0], TriggerDefinition::Webhook { .. }));
        assert!(matches!(
            def.triggers[1],
            TriggerDefinition::Schedule { .. }
        ));
    }

    #[test]
    fn webhook_trigger_without_secret_rejected() {
        let toml = r#"
[workflow]
id = "no-secret"
name = "No Secret"

[[step]]
id = "s1"
name = "Step"
task = "Do."

[[trigger]]
source = "github"
event_type = "push"
"#;
        let err = format!("{:#}", WorkflowDefinition::from_toml(toml).unwrap_err());
        assert!(err.contains("secret"), "error should name secret: {err}");
    }

    #[test]
    fn http_trigger_parses_with_defaults() {
        let toml = r#"
[workflow]
id = "ht"
name = "HTTP Workflow"

[[step]]
id = "s1"
name = "Step"
task = "Do."

[[trigger]]
type = "http"
id = "deploy-notifier"
[trigger.auth]
token = "[[vault:deploy/token]]"
"#;
        let def = WorkflowDefinition::from_toml(toml).unwrap();
        match &def.triggers[0] {
            TriggerDefinition::Http {
                id,
                auth,
                response,
                match_rules,
            } => {
                assert_eq!(id, "deploy-notifier");
                assert!(match_rules.is_empty());
                assert_eq!(*response, HttpResponseMode::Streaming);
                assert_eq!(
                    *auth,
                    TriggerAuth::Bearer {
                        token: "[[vault:deploy/token]]".to_string()
                    }
                );
            }
            _ => panic!("expected Http variant"),
        }
    }

    #[test]
    fn http_trigger_hmac_and_fire_forget() {
        let toml = r#"
[workflow]
id = "ht"
name = "HTTP Workflow"

[[step]]
id = "s1"
name = "Step"
task = "Do."

[[trigger]]
type = "http"
id = "sig"
response = "fire_forget"
[trigger.auth]
mode = "hmac"
secret = "[[vault:sig/secret]]"
[trigger.match]
"action" = "deploy"
"#;
        let def = WorkflowDefinition::from_toml(toml).unwrap();
        match &def.triggers[0] {
            TriggerDefinition::Http {
                auth,
                response,
                match_rules,
                ..
            } => {
                assert_eq!(*response, HttpResponseMode::FireForget);
                assert_eq!(match_rules.get("action"), Some(&"deploy".to_string()));
                assert_eq!(
                    *auth,
                    TriggerAuth::Hmac {
                        secret: "[[vault:sig/secret]]".to_string()
                    }
                );
            }
            _ => panic!("expected Http variant"),
        }
    }

    #[test]
    fn http_trigger_oidc_parses() {
        let toml = r#"
[workflow]
id = "ht"
name = "HTTP Workflow"

[[step]]
id = "s1"
name = "Step"
task = "Do."

[[trigger]]
type = "http"
id = "oidc-ep"
[trigger.auth]
mode = "oidc"
issuer = "https://keycloak.example/realms/wf"
audience = "workforce-triggers"
allowed_principals = ["svc-deploy"]
run_as_caller = true
"#;
        let def = WorkflowDefinition::from_toml(toml).unwrap();
        match &def.triggers[0] {
            TriggerDefinition::Http {
                auth: TriggerAuth::Oidc(oidc),
                ..
            } => {
                assert_eq!(oidc.issuer, "https://keycloak.example/realms/wf");
                assert_eq!(oidc.audience, "workforce-triggers");
                assert_eq!(oidc.principal_claim, "sub");
                assert_eq!(oidc.allowed_principals, vec!["svc-deploy".to_string()]);
                assert!(oidc.run_as_caller);
            }
            _ => panic!("expected Http/Oidc variant"),
        }
    }

    #[test]
    fn http_trigger_missing_auth_rejected() {
        let toml = r#"
[workflow]
id = "ht"
name = "HTTP Workflow"

[[step]]
id = "s1"
name = "Step"
task = "Do."

[[trigger]]
type = "http"
id = "no-auth"
"#;
        let err = format!("{:#}", WorkflowDefinition::from_toml(toml).unwrap_err());
        assert!(err.contains("auth"), "error should name auth: {err}");
    }

    #[test]
    fn http_trigger_bearer_requires_token() {
        let toml = r#"
[workflow]
id = "ht"
name = "HTTP Workflow"

[[step]]
id = "s1"
name = "Step"
task = "Do."

[[trigger]]
type = "http"
id = "bad"
[trigger.auth]
mode = "bearer"
"#;
        assert!(WorkflowDefinition::from_toml(toml).is_err());
    }

    #[test]
    fn http_trigger_oidc_requires_allowed_principals() {
        let toml = r#"
[workflow]
id = "ht"
name = "HTTP Workflow"

[[step]]
id = "s1"
name = "Step"
task = "Do."

[[trigger]]
type = "http"
id = "bad"
[trigger.auth]
mode = "oidc"
issuer = "https://issuer"
audience = "aud"
allowed_principals = []
"#;
        assert!(WorkflowDefinition::from_toml(toml).is_err());
    }

    #[test]
    fn http_trigger_invalid_id_rejected() {
        let toml = r#"
[workflow]
id = "ht"
name = "HTTP Workflow"

[[step]]
id = "s1"
name = "Step"
task = "Do."

[[trigger]]
type = "http"
id = "bad id!"
[trigger.auth]
token = "[[vault:t]]"
"#;
        let err = format!("{:#}", WorkflowDefinition::from_toml(toml).unwrap_err());
        assert!(err.contains("id"), "error should name the id: {err}");
    }

    fn decide_step(extra: &str) -> Result<WorkflowDefinition> {
        WorkflowDefinition::from_toml(&format!(
            "[workflow]\nid = \"wf\"\nname = \"WF\"\n\n[[step]]\nid = \"d\"\nname = \"D\"\n\
             task = \"{{{{text}}}}\"\n{extra}\n[[step]]\nid = \"bug\"\nname = \"Bug\"\ntask = \"t\"\n\
             \n[[step]]\nid = \"feature\"\nname = \"Feature\"\ntask = \"t\"\n"
        ))
    }

    #[test]
    fn a_decide_step_parses_and_is_a_decision_kind() {
        let def = decide_step(
            "decide = { question = \"Which?\", options = { bug = \"broken\", feature = \"new\" } }\n\
             branches = [{ when = \"bug\", goto = \"bug\" }, { when = \"feature\", goto = \"feature\" }]\n",
        )
        .unwrap();
        let step = &def.steps[0];
        assert!(matches!(step.kind(), StepKind::Decide(spec) if spec.kind == DecideKind::Choice));
        assert_eq!(step.delivered_report(), None);
        let scored = decide_step(
            "decide = { question = \"How urgent?\", kind = \"score\", levels = [\"low\", \"high\"] }\n\
             branches = [{ when = \"high\", goto = \"bug\" }]\n",
        )
        .unwrap();
        assert!(
            matches!(scored.steps[0].kind(), StepKind::Decide(spec) if spec.kind == DecideKind::Score)
        );
        let judged = decide_step(
            "decide = { question = \"Urgent?\", kind = \"noul\", options = { yes = \"customers are blocked now\", no = \"it can wait\" } }\n\
             branches = [{ when = \"yes\", goto = \"bug\" }]\n",
        )
        .unwrap();
        assert!(
            matches!(judged.steps[0].kind(), StepKind::Decide(spec) if spec.options.len() == 2)
        );
    }

    #[test]
    fn a_decide_step_needs_a_state_options_and_matching_branches() {
        for (extra, expect) in [
            (
                "decide = { question = \"Which?\" }\n",
                "at least two options",
            ),
            (
                "decide = { question = \"\", options = { a = \"\", b = \"\" } }\n",
                "non-empty question",
            ),
            (
                "decide = { question = \"Which?\", options = { a = \"\", b = \"\" } }\n\
                 branches = [{ when = \"bug\", goto = \"bug\" }]\n",
                "not one of the decision's options",
            ),
            (
                "decide = { question = \"Ok?\", kind = \"noul\" }\n\
                 branches = [{ when = \"maybe\", goto = \"bug\" }]\n",
                "routes on 'yes' or 'no'",
            ),
            (
                "decide = { question = \"Q\", kind = \"noul\", options = { maybe = \"\" } }\n",
                "describe only 'yes' and 'no'",
            ),
            (
                "decide = { question = \"Q\", kind = \"score\", levels = [\"only\"] }\n",
                "two to ten levels",
            ),
            (
                "decide = { question = \"Q\", options = { a = \"\", b = \"\" }, min_probability = 1.5 }\n",
                "between 0 and 1",
            ),
            (
                "decide = { question = \"Q\", options = { a = \"\", b = \"\" } }\npause = \"why\"\n",
                "cannot declare a pause point",
            ),
            (
                "decide = { question = \"Q\", options = { a = \"\", b = \"\" } }\ntool = \"s.t\"\n",
                "more than one of",
            ),
        ] {
            let err = format!("{:#}", decide_step(extra).unwrap_err());
            assert!(err.contains(expect), "{extra:?} → {err}");
        }
        let err = format!(
            "{:#}",
            WorkflowDefinition::from_toml(
                "[workflow]\nid = \"wf\"\nname = \"WF\"\n\n[[step]]\nid = \"d\"\nname = \"D\"\n\
                 decide = { question = \"Q\", options = { a = \"\", b = \"\" } }\n"
            )
            .unwrap_err()
        );
        assert!(err.contains("needs a `task`"), "{err}");
    }

    fn workflow_step(call: &str, extra: &str) -> Result<WorkflowDefinition> {
        WorkflowDefinition::from_toml(&format!(
            "[workflow]\nid = \"wf\"\nname = \"WF\"\n\n[[step]]\nid = \"run\"\nname = \"Run\"\n\
             workflow = {call}\n{extra}\n[[step]]\nid = \"after\"\nname = \"After\"\ntask = \"t\"\n"
        ))
    }

    #[test]
    fn a_workflow_step_takes_an_id_or_a_table_and_routes_on_how_the_run_ended() {
        let def = workflow_step(
            "\"child\"",
            "task = '{ \"ticket\": \"{{key}}\" }'\n\
             branches = [{ when = \"failed\", goto = \"after\" }]\n",
        )
        .unwrap();
        let StepKind::Workflow(call) = def.steps[0].kind() else {
            panic!("workflow kind");
        };
        assert_eq!(
            call,
            &WorkflowCall {
                id: "child".into(),
                wait: true
            }
        );
        let def = workflow_step("{ id = \"child\", wait = false }", "").unwrap();
        assert!(matches!(def.steps[0].kind(), StepKind::Workflow(c) if !c.wait));
        assert_eq!(def.steps[0].task, "");
    }

    #[test]
    fn a_workflow_step_cannot_call_itself_take_a_non_object_task_or_route_elsewhere() {
        for (call, extra, expect) in [
            ("\"wf\"", "", "cannot call itself"),
            ("\"alice/child\"", "", "never another member's"),
            (
                "{ id = \"child\", wait = false }",
                "[[step.outputs]]\nname = \"label\"\n",
                "does not wait has only",
            ),
            ("\"\"", "", "names no workflow"),
            (
                "\"child\"",
                "task = \"free text\"\n",
                "must be a JSON object",
            ),
            (
                "\"child\"",
                "branches = [{ when = \"approved\", goto = \"after\" }]\n",
                "routes on how the run ended (completed, failed, cancelled, exited)",
            ),
            (
                "{ id = \"child\", wait = false }",
                "branches = [{ when = \"completed\", goto = \"after\" }]\n",
                "(started)",
            ),
            (
                "\"child\"",
                "pause = \"why\"\n",
                "cannot declare a pause point",
            ),
            (
                "\"child\"",
                "decide = { question = \"Q\" }\n",
                "more than one of",
            ),
        ] {
            let err = format!("{:#}", workflow_step(call, extra).unwrap_err());
            assert!(err.contains(expect), "{call} {extra:?} → {err}");
        }
    }

    /// A ticket triage whose `classify` step carries `block` as its
    /// `decide_first` and `extra` beside it.
    fn triage(block: &str, extra: &str) -> Result<WorkflowDefinition> {
        WorkflowDefinition::from_toml(&format!(
            "[workflow]\nid = \"triage\"\nname = \"Triage\"\n\n\
             [workflow.context.subject]\nrequired = true\n\n\
             [[step]]\nid = \"classify\"\nname = \"Classify\"\n\
             task = \"Read {{{{subject}}}}. A crash: OUTCOME: bug\"\n\
             outputs = [{{ name = \"category\" }}, {{ name = \"note\", required = false }}]\n\
             branches = [{{ when = \"bug\", goto = \"fix\" }}]\n\
             goto = \"done\"\n{extra}{block}\n\
             [[step]]\nid = \"fix\"\nname = \"Fix\"\ntask = \"t\"\n\n\
             [[step]]\nid = \"done\"\nname = \"Done\"\ntask = \"t\"\n"
        ))
    }

    const FIRST: &str = "[step.decide_first]\n\
        state = '{ \"subject\": \"{{subject}}\" }'\n\
        options = { ignore = \"A newsletter or an auto-reply\" }\n\
        min_probability = 0.9\n\
        floors = { bug = \"never\" }\n\
        outputs = { category = \"{{decision.label}}\", note = \"p={{decision.probability}} for {{subject}}\" }\n";

    #[test]
    fn a_decide_first_block_parses_onto_an_agent_step_and_round_trips() {
        let def = triage(FIRST, "").unwrap();
        assert_eq!(def.authoring_issues(), Vec::<String>::new());
        let step = &def.steps[0];
        assert_eq!(step.kind(), StepKind::Agent, "still an agent step");
        let first = step.decide_first.as_ref().unwrap();
        assert_eq!(first.mode, DecisionUse::On);
        assert_eq!(first.min_probability, Some(0.9));
        assert_eq!(first.floors["bug"], Floor::Never);
        assert_eq!(first.options_of(step), ["bug", "ignore"]);
        assert!(first.may_settle("ignore") && !first.may_settle("bug"));
        assert_eq!(first.decide().kind, DecideKind::Choice);

        let shadowed = triage(
            &FIRST.replace(
                "{ bug = \"never\" }",
                "{ bug = \"never\", ignore = 0.95 }\nmode = \"shadow\"",
            ),
            "",
        )
        .unwrap();
        let first = shadowed.steps[0].decide_first.clone().unwrap();
        assert_eq!(
            (first.mode, first.floors["ignore"]),
            (DecisionUse::Shadow, Floor::At(0.95))
        );
        let stored = serde_json::to_value(&first).unwrap();
        assert_eq!(
            stored["floors"],
            serde_json::json!({"bug": "never", "ignore": 0.95})
        );
        assert_eq!(
            serde_json::from_value::<DecideFirst>(stored).unwrap(),
            first
        );
    }

    #[test]
    fn the_escape_is_withheld_only_where_one_option_stands_for_the_default_route() {
        let offered = triage(FIRST, "").unwrap().steps[0]
            .decide_first
            .clone()
            .unwrap();
        assert!(offered.escape, "offered unless the author withholds it");
        assert!(serde_json::to_value(&offered)
            .unwrap()
            .get("escape")
            .is_none());

        let def = triage(&format!("{FIRST}escape = false\n"), "").unwrap();
        assert_eq!(def.authoring_issues(), Vec::<String>::new());
        let withheld = def.steps[0].decide_first.clone().unwrap();
        assert!(!withheld.escape);
        let stored = serde_json::to_value(&withheld).unwrap();
        assert_eq!(stored["escape"], false);
        assert_eq!(
            serde_json::from_value::<DecideFirst>(stored).unwrap(),
            withheld
        );

        let options = |options: &str| {
            let block = FIRST.replace(
                "{ ignore = \"A newsletter or an auto-reply\" }",
                &format!("{{ {options} }}"),
            );
            format!(
                "{:#}",
                triage(&format!("{block}escape = false\n"), "").unwrap_err()
            )
        };
        assert!(
            options("").contains("no option stands for the step's default route"),
            "{}",
            options("")
        );
        let two = options("ignore = \"A newsletter\", later = \"Not now\"");
        assert!(
            two.contains("more than one option for the step's default route (ignore, later)"),
            "{two}"
        );
    }

    #[test]
    fn the_escape_is_withheld_only_where_every_floor_needs_more_than_half_the_answer() {
        let withheld = |floors: &str| {
            triage(
                &format!(
                    "{}escape = false\n",
                    FIRST.replace("floors = { bug = \"never\" }", floors)
                ),
                "",
            )
        };
        assert!(withheld("floors = { bug = \"never\", ignore = 0.51 }").is_ok());
        for (floors, low) in [
            ("floors = { bug = \"never\", ignore = 0.5 }", "0.5"),
            ("floors = { bug = 0.4 }", "0.4"),
        ] {
            let refused = format!("{:#}", withheld(floors).unwrap_err());
            assert!(
                refused.contains(&format!("escape = false with a floor of {low}")),
                "{refused}"
            );
        }
        let low_default = triage(
            &format!(
                "{}escape = false\n",
                FIRST.replace("min_probability = 0.9", "min_probability = 0.5")
            ),
            "",
        );
        assert!(format!("{:#}", low_default.unwrap_err()).contains("a floor of 0.5"));
        assert!(
            triage(
                &FIRST.replace("min_probability = 0.9", "min_probability = 0.5"),
                ""
            )
            .is_ok(),
            "with the escape, a low floor stays the author's call"
        );
    }

    #[test]
    fn a_step_without_the_block_has_none_and_serialises_as_it_did() {
        let def = triage("", "").unwrap();
        assert_eq!(def.steps[0].decide_first, None);
        let stored = serde_json::to_value(&def.steps[0]).unwrap();
        assert!(stored.get("decide_first").is_none());
    }

    #[test]
    fn a_decide_first_block_is_refused_where_it_cannot_stand_in_for_a_turn() {
        let on_step = |step: &str| {
            WorkflowDefinition::from_toml(&format!(
                "[workflow]\nid = \"wf\"\nname = \"WF\"\n\n[[step]]\nid = \"s\"\nname = \"S\"\n{step}\
                 decide_first = {{ state = \"x\" }}\n\n[[step]]\nid = \"a\"\nname = \"A\"\ntask = \"t\"\n"
            ))
        };
        for (step, expect) in [
            (
                "task = \"t\"\n",
                "sets decide_first but declares no branches, so it has no outcome to settle",
            ),
            (
                "task = \"{}\"\ntool = \"s.t\"\nbranches = [{ when = \"a\", goto = \"a\" }]\n",
                "only an agent step can carry",
            ),
            (
                "task = \"t\"\ndecide = { question = \"Q\", options = { a = \"\", b = \"\" } }\n",
                "only an agent step can carry",
            ),
            (
                "report = \"r\"\nreport_render = \"template\"\npause = \"p\"\nbranches = [{ when = \"a\", goto = \"a\" }]\n",
                "only an agent step can carry",
            ),
            (
                "task = \"t\"\nbranches = [{ when = \"a\", goto = \"a\" }]\nphases = [{ name = \"p\", task = \"t\" }]\n",
                "on a phased step",
            ),
            (
                "task = \"t\"\npause = \"gate\"\nbranches = [{ when = \"a\", goto = \"a\" }]\n",
                "pauses or ends the run",
            ),
            (
                "task = \"t\"\nexit = true\nbranches = [{ when = \"a\", goto = \"a\" }]\n",
                "pauses or ends the run",
            ),
        ] {
            let err = format!("{:#}", on_step(step).unwrap_err());
            assert!(err.contains(expect), "{step:?} → {err}");
        }
    }

    #[test]
    fn a_decide_first_block_names_real_options_and_fills_every_required_output() {
        let with = |from: &str, to: &str| triage(&FIRST.replace(from, to), "");
        for (from, to, expect) in [
            (
                "{ bug = \"never\" }",
                "{ urgent = 0.95 }",
                "sets a floor for 'urgent', which is not one of its options (bug, ignore)",
            ),
            (
                "category = \"{{decision.label}}\", ",
                "",
                "has no template for required output 'category'",
            ),
            (
                "note = ",
                "reason = ",
                "fills output 'reason', which the step does not declare",
            ),
            (
                "{{decision.probability}}",
                "{{decision.confidence}}",
                "reads '{{decision.confidence}}', which the answer does not carry",
            ),
            (
                "min_probability = 0.9",
                "min_probability = 1.5",
                "more than 0 and at most 1",
            ),
            (
                "{ bug = \"never\" }",
                "{ bug = 0 }",
                "a floor must be more than 0",
            ),
            (
                "{ bug = \"never\" }",
                "{ bug = \"sometimes\" }",
                "a probability or \"never\"",
            ),
            (
                "state = '{ \"subject\": \"{{subject}}\" }'",
                "state = \" \"",
                "needs a `state`",
            ),
            (
                "{ ignore = \"A newsletter or an auto-reply\" }",
                "{ none-of-these = \"x\" }",
                "cannot use 'none-of-these'",
            ),
            ("min_probability = 0.9", "question = \"\"", "empty question"),
            (
                "min_probability = 0.9",
                "min_probabilty = 0.9",
                "unknown field",
            ),
        ] {
            let err = format!("{:#}", with(from, to).unwrap_err());
            assert!(err.contains(expect), "{to:?} → {err}");
        }

        let no_default = |floors: &str| {
            WorkflowDefinition::from_toml(&format!(
                "[workflow]\nid = \"wf\"\nname = \"WF\"\n\n[[step]]\nid = \"s\"\nname = \"S\"\ntask = \"t\"\n\
                 branches = [{{ when = \"a\", goto = \"a\" }}]\n\
                 decide_first = {{ state = \"x\", options = {{ ignore = \"spam\" }}{floors} }}\n\n\
                 [[step]]\nid = \"a\"\nname = \"A\"\ntask = \"t\"\n"
            ))
        };
        let err = format!("{:#}", no_default("").unwrap_err());
        assert!(
            err.contains("option 'ignore' is not a branch and the step has no `goto`"),
            "{err}"
        );
        no_default(", floors = { ignore = \"never\" }").unwrap();
    }

    #[test]
    fn a_decide_first_option_nothing_describes_is_refused_at_save_and_still_loads() {
        let with = |options: &str| {
            WorkflowDefinition::from_toml(&format!(
                "[workflow]\nid = \"wf\"\nname = \"WF\"\n\n[[step]]\nid = \"s\"\nname = \"S\"\n\
                 task = \"A crash: OUTCOME: bug\"\ngoto = \"a\"\n\
                 branches = [{{ when = \"bug\", goto = \"a\" }}, {{ when = \"refund\", goto = \"a\" }}]\n\
                 decide_first = {{ state = \"x\", options = {{ {options} }} }}\n\n\
                 [[step]]\nid = \"a\"\nname = \"A\"\ntask = \"t\"\n"
            ))
            .expect("loads whatever its options say")
            .authoring_issues()
        };
        assert_eq!(
            with(""),
            ["step 's' decide_first offers 'refund' with nothing saying what it means — no line \
              of the task names it and decide_first.options does not describe it — so the model \
              cannot tell it apart: describe it in decide_first.options"]
        );
        assert_eq!(
            with("refund = \"A request for money back\""),
            Vec::<String>::new()
        );
        let blank = with("refund = \"A request for money back\", later = \" \"");
        assert_eq!(blank.len(), 1, "{blank:?}");
        assert!(
            blank[0].contains("'later'"),
            "an option described as nothing"
        );
    }

    #[test]
    fn a_decide_first_template_that_cannot_resolve_is_an_authoring_issue() {
        let unknown_state = triage(&FIRST.replace("{{subject}}\" }'", "{{sender}}\" }'"), "")
            .unwrap()
            .authoring_issues();
        assert_eq!(unknown_state.len(), 1, "{unknown_state:?}");
        assert!(unknown_state[0].contains("references '{{sender}}'"));

        let unknown_output = triage(
            &FIRST.replace("for {{subject}}", "for {{steps.fix.pr}}"),
            "",
        )
        .unwrap()
        .authoring_issues();
        assert_eq!(unknown_output.len(), 1, "{unknown_output:?}");
        assert!(unknown_output[0].contains("{{steps.fix.pr}}"));
    }
}
