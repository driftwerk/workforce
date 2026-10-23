//! Decision steps: a pause that named people settle by pressing one of the
//! step's buttons. The TOML types, their validation and the run state's pure
//! transitions live here; the hub does every read, write and post around them.

use std::collections::{BTreeMap, HashSet};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize};
use uuid::Uuid;

use super::definition::{StepDefinition, StepKind, WorkflowDefinition};
use super::engine::WorkflowExecutor;
use super::instance::{InstanceStatus, OutcomeBy, WorkflowInstance};
use super::package::{default_true, is_true};
use super::report;

/// The run variable holding the step's decision.
pub const DECISION_KEY: &str = "_decision";

const MAX_BUTTONS: usize = 25;
const MAX_LABEL: usize = 75;
const MAX_OUTCOME: usize = 64;
const MAX_CONFIRM: usize = 300;
const MAX_TITLE: usize = 150;
const MAX_CONTEXT: usize = 1_900;
const MIN_EXPIRY_SECS: u64 = 60;
const MAX_EXPIRY_SECS: u64 = 30 * 86_400;

/// `[step.decision]`: who decides, where they are asked, and the buttons.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DecisionSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
    /// `"dm"`, or a literal Slack conversation id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,
    /// Templates over the definition's own variables, each rendering to
    /// identity ids, emails, `team:<name>` and `<@U…>` mentions.
    #[serde(deserialize_with = "one_or_many")]
    pub deciders: Vec<String>,
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub allow_self_decision: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_after: Option<String>,
    /// The outcome of the button expiry presses.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_expiry: Option<String>,
    pub buttons: Vec<DecisionButton>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DecisionButton {
    pub label: String,
    pub outcome: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub style: Option<ButtonStyle>,
    /// What the closed card says was done; the label when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub done: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confirm: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goto: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end: Option<RunEnd>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow: Option<ButtonWorkflow>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub set: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ButtonStyle {
    Primary,
    Danger,
}

impl ButtonStyle {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Primary => "primary",
            Self::Danger => "danger",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RunEnd {
    Complete,
    Cancel,
}

/// A workflow a button starts, fire-and-forget, before its route.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(from = "ButtonWorkflowRepr")]
pub struct ButtonWorkflow {
    pub id: String,
    pub wait: bool,
    pub payload: BTreeMap<String, String>,
}

/// `workflow = "id"` or `workflow = { id = "…", payload = { … } }`.
#[derive(Deserialize)]
#[serde(untagged)]
enum ButtonWorkflowRepr {
    Id(String),
    Full(ButtonWorkflowFull),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ButtonWorkflowFull {
    id: String,
    #[serde(default)]
    wait: bool,
    #[serde(default)]
    payload: BTreeMap<String, String>,
}

impl From<ButtonWorkflowRepr> for ButtonWorkflow {
    fn from(repr: ButtonWorkflowRepr) -> Self {
        match repr {
            ButtonWorkflowRepr::Id(id) => Self {
                id,
                wait: false,
                payload: BTreeMap::new(),
            },
            ButtonWorkflowRepr::Full(ButtonWorkflowFull { id, wait, payload }) => {
                Self { id, wait, payload }
            }
        }
    }
}

fn one_or_many<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<String>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        One(String),
        Many(Vec<String>),
    }
    Ok(match OneOrMany::deserialize(deserializer)? {
        OneOrMany::One(one) => vec![one],
        OneOrMany::Many(many) => many,
    })
}

/// The decision a run holds at `variables["_decision"]`. No
/// `deny_unknown_fields`: a newer build's fields are carried through a
/// rolling deploy.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunDecision {
    pub gate: Uuid,
    pub step_id: String,
    /// The index of the step record this decision belongs to.
    pub record: usize,
    pub title: String,
    #[serde(default)]
    pub context: Option<String>,
    #[serde(default)]
    pub body: Option<String>,
    pub choices: Vec<Choice>,
    pub deciders: Vec<Decider>,
    pub allow_self_decision: bool,
    pub target: CardTarget,
    /// The agent whose bot posts the cards.
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub cards: Vec<Card>,
    #[serde(default)]
    pub posted: bool,
    pub opened_at: DateTime<Utc>,
    #[serde(default)]
    pub expires_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub on_expiry: Option<String>,
    #[serde(default)]
    pub closed: Option<Closed>,
    #[serde(default)]
    pub cards_closed: bool,
}

/// A button as it was when the step paused, its route resolved.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Choice {
    pub label: String,
    pub outcome: String,
    #[serde(default)]
    pub style: Option<ButtonStyle>,
    #[serde(default)]
    pub done: Option<String>,
    #[serde(default)]
    pub confirm: Option<String>,
    pub next: Next,
    #[serde(default)]
    pub start: Option<ChildStart>,
    #[serde(default)]
    pub set: BTreeMap<String, String>,
}

impl Choice {
    /// What the closed card says was done.
    pub fn done(&self) -> &str {
        self.done.as_deref().unwrap_or(&self.label)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "step", rename_all = "snake_case")]
pub enum Next {
    Step(String),
    Complete,
    Cancel,
}

impl From<RunEnd> for Next {
    fn from(end: RunEnd) -> Self {
        match end {
            RunEnd::Complete => Self::Complete,
            RunEnd::Cancel => Self::Cancel,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChildStart {
    pub workflow: String,
    pub payload: serde_json::Map<String, serde_json::Value>,
}

/// Someone who may decide, with every verified attribute known when the
/// step paused.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Decider {
    #[serde(default)]
    pub identity_id: Option<String>,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub slack_user_id: Option<String>,
    pub display: String,
}

impl Decider {
    /// Whether `person` is this decider by any attribute both know.
    pub fn is(&self, person: &Person) -> bool {
        let same = |a: &Option<String>, b: &Option<String>| a.is_some() && a == b;
        self.identity_id.as_deref() == Some(person.id.as_str())
            || same(&self.slack_user_id, &person.slack_user_id)
            || matches!((&self.email, &person.email), (Some(a), Some(b)) if a.eq_ignore_ascii_case(b))
    }

    /// The decider as a card names them: a mention when Slack knows them.
    pub fn mention(&self) -> String {
        self.slack_user_id
            .as_deref()
            .map_or_else(|| self.display.clone(), |id| format!("<@{id}>"))
    }
}

/// Where the cards go.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CardTarget {
    /// The run's own thread, or top-level in its channel when it has none.
    Thread {
        channel: String,
        thread: Option<String>,
    },
    Channel {
        channel: String,
    },
    DirectMessages,
}

/// One posted card.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Card {
    pub channel: String,
    pub ts: String,
    /// The decider a direct-message card went to.
    #[serde(default)]
    pub decider: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Closed {
    Decided {
        outcome: String,
        by: DecidedBy,
        at: DateTime<Utc>,
        followed: bool,
        #[serde(default)]
        child: Option<String>,
    },
    Stopped {
        by: Option<String>,
        at: DateTime<Utc>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DecidedBy {
    Person {
        id: String,
        display: String,
        #[serde(default)]
        slack_user_id: Option<String>,
    },
    Expiry,
}

/// A person asking to decide, with their verified attributes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Person {
    pub id: String,
    pub email: Option<String>,
    pub slack_user_id: Option<String>,
}

/// Why a person may not decide.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    NotDecider,
    /// They started the run and the decision is not theirs to make.
    OwnDecision,
}

/// Where following a decision left the run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Followed {
    Step(String),
    Completed,
    Cancelled(String),
}

/// Settling asked of a decision that is not open.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the decision is not open")]
pub struct NotOpen;

/// What the hub resolves before a decision can open.
pub struct Opening<'a> {
    pub deciders: Vec<Decider>,
    pub target: CardTarget,
    pub body: Option<String>,
    pub agent: Option<String>,
    pub starter: Option<&'a str>,
    pub now: DateTime<Utc>,
}

impl DecisionSpec {
    /// Every structural rule a decision keeps; each error names the step.
    pub fn validate(&self, step: &StepDefinition, def: &WorkflowDefinition) -> eyre::Result<()> {
        self.check(step, def)
            .map_err(|why| eyre::eyre!("Step '{}' {why}", step.id))
    }

    fn check(&self, step: &StepDefinition, def: &WorkflowDefinition) -> Result<(), String> {
        check_step(step)?;
        self.check_buttons()?;
        self.buttons
            .iter()
            .try_for_each(|button| button.check(step, def))?;
        self.check_deciders(def)?;
        self.templates().try_for_each(|(field, template)| {
            report::check(template).map_err(|e| format!("decision {field} does not compile: {e}"))
        })?;
        self.check_channel()?;
        self.check_expiry()
    }

    fn check_buttons(&self) -> Result<(), String> {
        if !(1..=MAX_BUTTONS).contains(&self.buttons.len()) {
            return Err(format!(
                "decision has {} buttons; it needs 1 to {MAX_BUTTONS}",
                self.buttons.len()
            ));
        }
        self.buttons
            .iter()
            .try_for_each(DecisionButton::check_own)?;
        let duplicate = |forms: Vec<String>, what: &str| -> Result<(), String> {
            let mut seen = HashSet::new();
            forms
                .into_iter()
                .find(|form| !seen.insert(form.clone()))
                .map_or(Ok(()), |form| {
                    Err(format!("decision has two buttons with the {what} '{form}'"))
                })
        };
        duplicate(
            self.buttons
                .iter()
                .map(|b| b.label.trim().to_lowercase())
                .collect(),
            "label",
        )?;
        duplicate(
            self.buttons.iter().map(|b| b.outcome.clone()).collect(),
            "outcome",
        )?;
        let typed: Vec<(String, usize)> = self
            .buttons
            .iter()
            .enumerate()
            .flat_map(|(at, b)| {
                [
                    (b.label.trim().to_lowercase(), at),
                    (b.outcome.to_lowercase(), at),
                ]
            })
            .collect();
        if let Some((form, _)) = typed.iter().find(|(form, at)| {
            typed
                .iter()
                .any(|(other, other_at)| other == form && other_at != at)
        }) {
            return Err(format!(
                "decision buttons can't be told apart when typed: '{form}' names two of them"
            ));
        }
        if self
            .buttons
            .iter()
            .filter(|b| b.style == Some(ButtonStyle::Primary))
            .count()
            > 1
        {
            return Err("decision has more than one primary button".into());
        }
        Ok(())
    }

    fn check_deciders(&self, def: &WorkflowDefinition) -> Result<(), String> {
        if self.deciders.is_empty() {
            return Err("decision names no deciders".into());
        }
        self.deciders.iter().try_for_each(|entry| {
            if entry.trim().is_empty() {
                return Err("decision has a blank decider".to_string());
            }
            report::check(entry).map_err(|e| format!("decider '{entry}' does not compile: {e}"))?;
            report::references(entry)
                .into_iter()
                .map(|key| decider_reference_issue(&key, def))
                .find_map(Result::err)
                .map_or(Ok(()), |why| Err(format!("decider '{entry}' {why}")))
        })
    }

    fn check_channel(&self) -> Result<(), String> {
        match self.channel.as_deref() {
            None | Some("dm") => Ok(()),
            Some(channel) if is_conversation_id(channel) => Ok(()),
            Some(channel) => Err(format!(
                "decision channel '{channel}' is not \"dm\" or a Slack channel id (C… or G…)"
            )),
        }
    }

    fn check_expiry(&self) -> Result<(), String> {
        match (&self.expires_after, &self.on_expiry) {
            (None, None) => Ok(()),
            (Some(_), None) | (None, Some(_)) => {
                Err("decision sets one of expires_after and on_expiry; they go together".into())
            }
            (Some(after), Some(outcome)) => {
                if !self.buttons.iter().any(|b| &b.outcome == outcome) {
                    return Err(format!(
                        "decision on_expiry '{outcome}' is not the outcome of any button"
                    ));
                }
                let secs = workforce_common::temporal::parse_duration(after)
                    .map_err(|e| format!("decision expires_after '{after}' {e}"))?
                    .as_secs();
                (MIN_EXPIRY_SECS..=MAX_EXPIRY_SECS)
                    .contains(&secs)
                    .then_some(())
                    .ok_or_else(|| {
                        format!("decision expires_after '{after}' must lie between 1m and 30d")
                    })
            }
        }
    }

    /// Every Jinja field rendered against the run's variables, by name.
    pub fn templates(&self) -> impl Iterator<Item = (&'static str, &str)> {
        self.title
            .as_deref()
            .map(|t| ("title", t))
            .into_iter()
            .chain(self.context.as_deref().map(|c| ("context", c)))
            .chain(
                self.buttons
                    .iter()
                    .filter_map(|b| b.workflow.as_ref())
                    .flat_map(|w| w.payload.values())
                    .map(|v| ("workflow payload", v.as_str())),
            )
    }

    /// The decider tokens, rendered against the definition's own non-secret
    /// variables and nothing else, split, trimmed and deduplicated in order.
    pub fn decider_tokens(&self, def: &WorkflowDefinition) -> Result<Vec<String>, String> {
        let declared: serde_json::Map<String, serde_json::Value> = def
            .variables
            .iter()
            .filter(|v| !v.is_secret())
            .map(|v| (v.id.clone(), serde_json::Value::String(v.value.clone())))
            .collect();
        let declared = serde_json::Value::Object(declared);
        let rendered = self
            .deciders
            .iter()
            .map(|entry| {
                report::render(entry, &declared)
                    .map_err(|e| format!("decider '{entry}' does not render: {e}"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let mut seen = HashSet::new();
        Ok(rendered
            .iter()
            .flat_map(|text| text.split([',', '\n']))
            .map(str::trim)
            .filter(|token| !token.is_empty())
            .filter(|token| seen.insert(token.to_string()))
            .map(str::to_string)
            .collect())
    }

    /// Notices a save carries: a branch no button takes.
    fn unreached_branches<'a>(
        &'a self,
        step: &'a StepDefinition,
    ) -> impl Iterator<Item = String> + 'a {
        step.branches
            .iter()
            .filter(|branch| !self.buttons.iter().any(|b| b.outcome == branch.when))
            .map(move |branch| {
                format!(
                    "step '{}' branch '{}' is taken by no decision button, so no press reaches it",
                    step.id, branch.when
                )
            })
    }
}

/// Every notice the decisions of `def` carry.
pub(super) fn notices(def: &WorkflowDefinition) -> Vec<String> {
    let scheduled_single = def.workflow.max_concurrent == 1
        && def
            .triggers
            .iter()
            .any(|t| matches!(t, super::definition::TriggerDefinition::Schedule { .. }));
    def.steps
        .iter()
        .filter_map(|step| step.decision.as_ref().map(|spec| (step, spec)))
        .flat_map(|(step, spec)| {
            let unbounded = (scheduled_single && spec.expires_after.is_none()).then(|| {
                format!(
                    "step '{}' waits on a decision with no expires_after in a scheduled workflow \
                     with max_concurrent = 1: while it waits, every later scheduled run is skipped",
                    step.id
                )
            });
            spec.unreached_branches(step)
                .chain(unbounded)
                .collect::<Vec<_>>()
        })
        .collect()
}

fn check_step(step: &StepDefinition) -> Result<(), String> {
    if step.pause.is_none() {
        return Err(
            "declares a decision but no `pause`; a decision holds the run for people".into(),
        );
    }
    if !matches!(step.kind(), StepKind::Agent | StepKind::Report) {
        return Err(
            "declares a decision but runs as a tool, a decision or another workflow".into(),
        );
    }
    if step.delivered_report().is_none() {
        return Err(
            "declares a decision but no report the engine delivers; the report is the card's body"
                .into(),
        );
    }
    if step.goto.is_some() {
        return Err("declares a decision and a `goto`; a decision routes by its buttons".into());
    }
    if step.exit {
        return Err("is an exit step and cannot declare a decision".into());
    }
    if step.max_iterations.is_some() || step.iteration_cap_outcome.is_some() {
        return Err(
            "declares a decision with max_iterations or iteration_cap_outcome, which a decision \
             never reaches"
                .into(),
        );
    }
    Ok(())
}

fn decider_reference_issue(key: &str, def: &WorkflowDefinition) -> Result<(), String> {
    let root = report::root(key);
    let declared = def.variables.iter().find(|v| v.id == root);
    match declared {
        Some(variable) if variable.is_secret() => Err(format!(
            "reads the secret variable '{root}'; deciders cannot be secret"
        )),
        Some(_) => Ok(()),
        None if root == "steps" => Err(format!(
            "reads the step output '{key}'; deciders may read only [[variables]]"
        )),
        None if root.starts_with('_') => Err(format!(
            "reads the engine variable '{root}'; deciders may read only [[variables]]"
        )),
        None if def.workflow.context.contains_key(root) => Err(format!(
            "reads the context field '{root}'; deciders may read only [[variables]], so a \
             payload cannot choose who decides"
        )),
        None => Err(format!(
            "reads '{root}', which no [[variables]] entry declares"
        )),
    }
}

fn is_conversation_id(channel: &str) -> bool {
    channel.starts_with(['C', 'G'])
        && channel.len() >= 9
        && channel
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
}

fn is_outcome_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '-' || c == '_'
}

fn is_set_key(key: &str) -> bool {
    !key.is_empty() && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

impl DecisionButton {
    fn check_own(&self) -> Result<(), String> {
        let label = self.label.trim();
        let chars = label.chars().count();
        if !(1..=MAX_LABEL).contains(&chars) {
            return Err(format!(
                "decision button '{label}' needs a label of 1 to {MAX_LABEL} characters"
            ));
        }
        if self.outcome.is_empty()
            || self.outcome.len() > MAX_OUTCOME
            || !self.outcome.chars().all(is_outcome_char)
        {
            return Err(format!(
                "decision button '{label}' outcome '{}' must be 1 to {MAX_OUTCOME} of [A-Za-z0-9_-]",
                self.outcome
            ));
        }
        if self
            .done
            .as_deref()
            .is_some_and(|done| !(1..=MAX_LABEL).contains(&done.trim().chars().count()))
        {
            return Err(format!(
                "decision button '{label}' done needs 1 to {MAX_LABEL} characters"
            ));
        }
        if self
            .confirm
            .as_deref()
            .is_some_and(|confirm| !(1..=MAX_CONFIRM).contains(&confirm.trim().chars().count()))
        {
            return Err(format!(
                "decision button '{label}' confirm needs 1 to {MAX_CONFIRM} characters"
            ));
        }
        Ok(())
    }

    fn check(&self, step: &StepDefinition, def: &WorkflowDefinition) -> Result<(), String> {
        let label = self.label.trim();
        let next = self.route(step)?;
        if let Next::Step(target) = &next {
            if def.get_step(target).is_none() {
                return Err(format!(
                    "decision button '{label}' goes to unknown step '{target}'"
                ));
            }
        }
        if let Some(workflow) = &self.workflow {
            let id = workflow.id.trim();
            let refused = match () {
                () if id.is_empty() => Some("names no workflow to start".to_string()),
                () if id.contains('/') => Some(format!(
                    "starts workflow '{id}': give the bare id, never another member's"
                )),
                () if id == def.workflow.id => Some(format!("starts its own workflow '{id}'")),
                () if workflow.wait => Some(format!(
                    "sets wait = true on workflow '{id}': a button starts a workflow and moves \
                     on; to wait for it, `goto` a step that runs `workflow = {{ id = \"{id}\" }}`"
                )),
                () if next == Next::Cancel => Some(format!(
                    "starts workflow '{id}' and ends the run cancelled: cancelling a run stops \
                     the runs it started"
                )),
                () => None,
            };
            if let Some(why) = refused {
                return Err(format!("decision button '{label}' {why}"));
            }
        }
        let secret = |key: &str| def.variables.iter().any(|v| v.id == key && v.is_secret());
        self.set.iter().try_for_each(|(key, value)| {
            if !is_set_key(key) || key.starts_with('_') || key == "steps" || secret(key) {
                return Err(format!(
                    "decision button '{label}' sets '{key}': set keys match [A-Za-z0-9_], do not \
                     start with '_', and name neither `steps` nor a secret variable"
                ));
            }
            if value.contains("{{") || value.contains("{%") {
                return Err(format!(
                    "decision button '{label}' sets '{key}' to a template: set values are literal"
                ));
            }
            Ok(())
        })
    }

    fn routes(&self, step: &StepDefinition) -> Vec<Next> {
        step.branches
            .iter()
            .filter(|branch| branch.when == self.outcome)
            .map(|branch| Next::Step(branch.goto.clone()))
            .chain(self.goto.clone().map(Next::Step))
            .chain(self.end.map(Next::from))
            .collect()
    }

    /// The button's one route: a branch whose `when` is its outcome, its
    /// `goto`, or its `end`.
    pub fn route(&self, step: &StepDefinition) -> Result<Next, String> {
        match self.routes(step).as_slice() {
            [next] => Ok(next.clone()),
            found => {
                let found = found
                    .iter()
                    .map(|next| match next {
                        Next::Step(target) => format!("step '{target}'"),
                        Next::Complete => "end = \"complete\"".to_string(),
                        Next::Cancel => "end = \"cancel\"".to_string(),
                    })
                    .collect::<Vec<_>>();
                let found = match found.is_empty() {
                    true => "none".to_string(),
                    false => found.join(", "),
                };
                Err(format!(
                    "decision button '{}' needs exactly one route (a branch on '{}', `goto` or \
                     `end`); it has {found}",
                    self.label.trim(),
                    self.outcome
                ))
            }
        }
    }
}

fn truncated(text: &str, max: usize) -> String {
    match text.chars().count() > max {
        true => text.chars().take(max - 1).chain(['…']).collect(),
        false => text.to_string(),
    }
}

impl RunDecision {
    /// The decision a step that just paused opens, everything a press needs
    /// frozen as it stands now.
    pub fn open(
        spec: &DecisionSpec,
        step: &StepDefinition,
        instance: &WorkflowInstance,
        opening: Opening<'_>,
    ) -> Result<Self, String> {
        let render = |template: &str| report::render(template, &instance.variables);
        let record = instance
            .step_history
            .len()
            .checked_sub(1)
            .filter(|_| instance.last_record_of(&step.id).is_some())
            .ok_or_else(|| format!("step '{}' has no completed run to decide on", step.id))?;
        let title = spec
            .title
            .as_deref()
            .map(|title| render(title).map_err(|e| format!("decision title: {e}")))
            .transpose()?
            .filter(|title| !title.trim().is_empty())
            .unwrap_or_else(|| step.name.clone());
        let context = spec
            .context
            .as_deref()
            .map(|context| render(context).map_err(|e| format!("decision context: {e}")))
            .transpose()?;
        let choices = spec
            .buttons
            .iter()
            .map(|button| {
                let start = button
                    .workflow
                    .as_ref()
                    .map(|workflow| {
                        workflow
                            .payload
                            .iter()
                            .map(|(key, template)| {
                                render(template)
                                    .map(|value| (key.clone(), serde_json::Value::String(value)))
                                    .map_err(|e| format!("workflow payload '{key}': {e}"))
                            })
                            .collect::<Result<_, _>>()
                            .map(|payload| ChildStart {
                                workflow: workflow.id.trim().to_string(),
                                payload,
                            })
                    })
                    .transpose()?;
                Ok(Choice {
                    label: button.label.trim().to_string(),
                    outcome: button.outcome.clone(),
                    style: button.style,
                    done: button.done.clone(),
                    confirm: button.confirm.clone(),
                    next: button.route(step)?,
                    start,
                    set: button.set.clone(),
                })
            })
            .collect::<Result<Vec<_>, String>>()?;
        let expires_at = spec
            .expires_after
            .as_deref()
            .map(|after| {
                workforce_common::temporal::parse_duration(after)
                    .ok()
                    .and_then(|d| chrono::Duration::from_std(d).ok())
                    .map(|d| opening.now + d)
                    .ok_or_else(|| format!("decision expires_after '{after}' does not parse"))
            })
            .transpose()?;
        let decision = Self {
            gate: Uuid::new_v4(),
            step_id: step.id.clone(),
            record,
            title: truncated(title.trim(), MAX_TITLE),
            context: context
                .map(|c| truncated(c.trim(), MAX_CONTEXT))
                .filter(|c| !c.is_empty()),
            body: opening.body.filter(|b| !b.trim().is_empty()),
            choices,
            deciders: opening.deciders,
            allow_self_decision: spec.allow_self_decision,
            target: opening.target,
            agent: opening.agent,
            cards: Vec::new(),
            posted: false,
            opened_at: opening.now,
            expires_at,
            on_expiry: spec.on_expiry.clone(),
            closed: None,
            cards_closed: false,
        };
        match decision.waiting_on(opening.starter).is_empty() {
            true if decision.deciders.is_empty() => {
                Err("the decision's deciders resolved to nobody".into())
            }
            true => Err(
                "the only decider is the person who started this run, and allow_self_decision is \
                 false"
                    .into(),
            ),
            false => Ok(decision),
        }
    }

    /// The title, context and body passed through `clean`.
    pub fn cleaned(self, clean: impl Fn(&str) -> String) -> Self {
        Self {
            title: clean(&self.title),
            context: self.context.as_deref().map(&clean),
            body: self.body.as_deref().map(&clean),
            ..self
        }
    }

    pub fn read(variables: &serde_json::Value) -> Option<Self> {
        variables
            .get(DECISION_KEY)
            .and_then(|value| serde_json::from_value(value.clone()).ok())
    }

    /// Keep this decision in `variables`; a run started with no payload has
    /// none yet, and gets them.
    pub fn write(&self, variables: &mut serde_json::Value) {
        if variables.is_null() {
            *variables = serde_json::Value::Object(serde_json::Map::new());
        }
        if let (Some(vars), Ok(value)) = (variables.as_object_mut(), serde_json::to_value(self)) {
            vars.insert(DECISION_KEY.to_string(), value);
        }
    }

    pub fn remove(variables: &mut serde_json::Value) {
        if let Some(vars) = variables.as_object_mut() {
            vars.remove(DECISION_KEY);
        }
    }

    /// Whether this decision belongs to the run's current occurrence of its
    /// step: its record is the run's last, a completed run of that step.
    fn belongs_to(&self, instance: &WorkflowInstance) -> bool {
        instance.current_step_id.as_deref() == Some(self.step_id.as_str())
            && instance.step_history.len().checked_sub(1) == Some(self.record)
            && instance.last_record_of(&self.step_id).is_some()
    }

    /// The choice `asked` names: its outcome, else a label ignoring case,
    /// else an outcome ignoring case.
    pub fn choice(&self, asked: &str) -> Option<&Choice> {
        let asked = asked.trim();
        let folded = asked.to_lowercase();
        self.choices
            .iter()
            .find(|c| c.outcome == asked)
            .or_else(|| {
                self.choices
                    .iter()
                    .find(|c| c.label.to_lowercase() == folded)
            })
            .or_else(|| {
                self.choices
                    .iter()
                    .find(|c| c.outcome.to_lowercase() == folded)
            })
    }

    /// The deciders who may decide: all of them, less the starter when the
    /// decision is not theirs to make.
    pub fn waiting_on(&self, starter: Option<&str>) -> Vec<&Decider> {
        self.deciders
            .iter()
            .filter(|d| {
                self.allow_self_decision || starter.is_none() || d.identity_id.as_deref() != starter
            })
            .collect()
    }

    /// Why `person` may not decide, if they may not.
    pub fn refusal(&self, person: Option<&Person>, starter: Option<&str>) -> Option<Refusal> {
        match person {
            Some(person) if self.deciders.iter().any(|d| d.is(person)) => {
                (!self.allow_self_decision && Some(person.id.as_str()) == starter)
                    .then_some(Refusal::OwnDecision)
            }
            _ => Some(Refusal::NotDecider),
        }
    }

    /// The choice expiry presses, once the decision is past its deadline.
    pub fn expired(&self, now: DateTime<Utc>) -> Option<&Choice> {
        self.expires_at
            .filter(|at| *at <= now)
            .and(self.on_expiry.as_deref())
            .and_then(|outcome| self.choice(outcome))
    }

    /// How the decision closed, in a few words.
    pub fn summary(&self) -> Option<String> {
        match self.closed.as_ref()? {
            Closed::Decided { outcome, by, .. } => {
                let done = self.choice(outcome).map_or(outcome.as_str(), Choice::done);
                Some(match by {
                    DecidedBy::Person { display, .. } => format!("{done} by {display}"),
                    DecidedBy::Expiry => format!("{done} (expired)"),
                })
            }
            Closed::Stopped { by: Some(by), .. } => {
                Some(format!("stopped by {by}; nothing was decided"))
            }
            Closed::Stopped { by: None, .. } => Some("stopped; nothing was decided".to_string()),
        }
    }
}

/// The decision a run waits on: paused at its step's current occurrence,
/// not yet closed.
pub fn open_in(instance: &WorkflowInstance) -> Option<RunDecision> {
    RunDecision::read(&instance.variables).filter(|d| {
        instance.status == InstanceStatus::Paused && d.closed.is_none() && d.belongs_to(instance)
    })
}

/// The decision a run has settled but not yet carried out.
pub fn awaiting_follow(instance: &WorkflowInstance) -> Option<RunDecision> {
    RunDecision::read(&instance.variables).filter(|d| {
        matches!(
            d.closed,
            Some(Closed::Decided {
                followed: false,
                ..
            })
        ) && !instance.is_terminal()
            && d.belongs_to(instance)
    })
}

/// The decision whose posted cards still show it open.
pub fn cards_to_close(instance: &WorkflowInstance) -> Option<RunDecision> {
    RunDecision::read(&instance.variables)
        .filter(|d| d.closed.is_some() && !d.cards_closed && d.posted)
}

/// Settle the open decision with `choice`: record it as a resume of the step,
/// write the button's literals and any feedback, and run the run.
pub fn settle(
    instance: &mut WorkflowInstance,
    choice: &Choice,
    by: DecidedBy,
    feedback: Option<&str>,
    now: DateTime<Utc>,
) -> Result<(), NotOpen> {
    let decision = open_in(instance).ok_or(NotOpen)?;
    if let Some(vars) = instance.variables.as_object_mut() {
        vars.extend(
            choice
                .set
                .iter()
                .map(|(key, value)| (key.clone(), serde_json::Value::String(value.clone()))),
        );
        if let Some(feedback) = feedback {
            vars.insert(
                "_human_feedback".to_string(),
                serde_json::Value::String(feedback.to_string()),
            );
        }
    }
    instance.preset_outcome(&decision.step_id, &choice.outcome, OutcomeBy::Resume);
    instance.status = InstanceStatus::Running;
    instance.updated_at = now;
    RunDecision {
        closed: Some(Closed::Decided {
            outcome: choice.outcome.clone(),
            by,
            at: now,
            followed: false,
            child: None,
        }),
        ..decision
    }
    .write(&mut instance.variables);
    Ok(())
}

/// Carry out the settled decision: move to its frozen step, or end the run.
pub fn follow(
    instance: &mut WorkflowInstance,
    def: &WorkflowDefinition,
    now: DateTime<Utc>,
) -> Result<Followed, String> {
    let (decision, outcome, by) = awaiting_follow(instance)
        .and_then(|d| match d.closed.clone() {
            Some(Closed::Decided { outcome, by, .. }) => Some((d, outcome, by)),
            _ => None,
        })
        .ok_or_else(|| "no decision awaits carrying out".to_string())?;
    let choice = decision
        .choice(&outcome)
        .ok_or_else(|| format!("the decision chose '{outcome}', which it never offered"))?;
    let followed = match &choice.next {
        Next::Step(target) => {
            if def.get_step(target).is_none() {
                return Err(format!(
                    "the decision chose step '{target}', which the workflow no longer has"
                ));
            }
            instance.current_step_id = Some(target.clone());
            instance.status = InstanceStatus::Running;
            Followed::Step(target.clone())
        }
        Next::Complete => {
            instance.status = InstanceStatus::Completed;
            instance.current_step_id = None;
            Followed::Completed
        }
        Next::Cancel => {
            let reason = match by {
                DecidedBy::Person { display, .. } => format!(
                    "Ended at '{}': {} by {display}",
                    decision.step_id, choice.label
                ),
                DecidedBy::Expiry => {
                    format!("Expired at '{}': {}", decision.step_id, choice.label)
                }
            };
            WorkflowExecutor::cancel(instance, &reason);
            Followed::Cancelled(reason)
        }
    };
    instance.updated_at = now;
    amend_decided(instance, |followed, _| *followed = true);
    Ok(followed)
}

/// Note the child run the settled decision started.
pub fn record_child(instance: &mut WorkflowInstance, child_id: &str) {
    amend_decided(instance, |_, child| *child = Some(child_id.to_string()));
}

/// Change whether the settled decision was carried out, and the child it
/// started, with `change`.
fn amend_decided(
    instance: &mut WorkflowInstance,
    change: impl FnOnce(&mut bool, &mut Option<String>),
) {
    if let Some(mut decision) = RunDecision::read(&instance.variables) {
        if let Some(Closed::Decided {
            followed, child, ..
        }) = decision.closed.as_mut()
        {
            change(followed, child);
            decision.write(&mut instance.variables);
        }
    }
}

/// Close the open decision because the run was stopped. Whether it was open.
pub fn stop(instance: &mut WorkflowInstance, by: Option<String>, now: DateTime<Utc>) -> bool {
    RunDecision::read(&instance.variables)
        .filter(|d| d.closed.is_none())
        .map(|decision| {
            RunDecision {
                closed: Some(Closed::Stopped { by, at: now }),
                ..decision
            }
            .write(&mut instance.variables)
        })
        .is_some()
}

/// Where a decision's cards go: `channel` as the step names it, else the
/// run's own thread, else each decider directly.
pub fn target(
    channel: Option<&str>,
    run_channel: Option<&str>,
    run_thread: Option<&str>,
) -> CardTarget {
    let thread = |channel: &str| CardTarget::Thread {
        channel: channel.to_string(),
        thread: run_thread.map(str::to_string),
    };
    match (channel, run_channel) {
        (Some("dm"), _) | (None, None) => CardTarget::DirectMessages,
        (Some(named), Some(run)) if named == run => thread(run),
        (Some(named), _) => CardTarget::Channel {
            channel: named.to_string(),
        },
        (None, Some(run)) => thread(run),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflow::instance::{TriggerInfo, TriggerSource};

    const DIGEST: &str = r#"
[workflow]
id = "daily-digest-email"
name = "Daily digest email"

[[variables]]
id = "approvers"
value = "approver.one@example.com, team:reviewers"

[[step]]
id = "draft"
name = "Draft the digest"
task = "Draft today's digest."
outputs = [{ name = "recipient" }, { name = "subject" }]

[[step]]
id = "review"
name = "Review the digest"
report_render = "template"
report = "To: {{ steps.draft.recipient }}"
pause = "awaiting-approval"
branches = [{ when = "send", goto = "send_email" }]

[step.decision]
title = "Send today's digest to {{ steps.draft.recipient }}?"
context = "Drafted from yesterday's figures"
channel = "C0123456789"
deciders = ["{{ approvers }}", "<@U0123ABCD>"]
allow_self_decision = false
expires_after = "20h"
on_expiry = "cancel"

[[step.decision.buttons]]
label = "Send"
outcome = "send"
style = "primary"
done = "Sent"
confirm = "Send this email now?"

[[step.decision.buttons]]
label = "Redraft"
outcome = "redraft"
goto = "draft"

[[step.decision.buttons]]
label = "Cancel"
outcome = "cancel"
style = "danger"
done = "Cancelled"
end = "cancel"

[[step.decision.buttons]]
label = "Escalate"
outcome = "escalate"
workflow = { id = "digest-escalation", payload = { subject = "{{ steps.draft.subject }}" } }
set = { escalated = "yes" }
end = "complete"

[[step]]
id = "send_email"
name = "Send"
task = "{}"
tool = "mailer.send"
exit = true
"#;

    fn digest() -> WorkflowDefinition {
        WorkflowDefinition::from_toml(DIGEST).expect("the digest parses")
    }

    fn now() -> DateTime<Utc> {
        "2026-10-16T09:00:00Z".parse().unwrap()
    }

    fn decider(id: &str) -> Decider {
        Decider {
            identity_id: Some(id.into()),
            email: None,
            slack_user_id: None,
            display: id.to_uppercase(),
        }
    }

    fn person(id: &str) -> Person {
        Person {
            id: id.into(),
            email: None,
            slack_user_id: None,
        }
    }

    /// A run of the digest paused at its review, the draft's outputs in.
    fn paused_at_review(def: &WorkflowDefinition) -> WorkflowInstance {
        let mut instance = WorkflowExecutor::start(
            def,
            TriggerInfo {
                source: TriggerSource::Api,
                payload: serde_json::json!({}),
            },
            None,
        )
        .unwrap();
        instance.record_step_with_outputs(
            "draft",
            None,
            "drafted",
            Some(serde_json::json!({ "recipient": "all@example.com", "subject": "Today", "blank": "" })),
            None,
            now(),
            1,
            0.0,
            false,
        );
        instance.current_step_id = Some("review".into());
        instance.record_step("review", None, "", now(), 1, 0.0);
        instance.status = InstanceStatus::Paused;
        instance
    }

    fn opened(def: &WorkflowDefinition, instance: &mut WorkflowInstance) -> RunDecision {
        let step = def.get_step("review").unwrap();
        let decision = RunDecision::open(
            step.decision.as_ref().unwrap(),
            step,
            instance,
            Opening {
                deciders: vec![decider("alice"), decider("bob")],
                target: CardTarget::DirectMessages,
                body: Some("To: all@example.com".into()),
                agent: None,
                starter: Some("alice"),
                now: now(),
            },
        )
        .unwrap();
        decision.write(&mut instance.variables);
        decision
    }

    #[test]
    fn a_decision_step_parses_and_round_trips_through_toml() {
        let def = digest();
        let spec = def.get_step("review").unwrap().decision.clone().unwrap();
        assert_eq!(spec.deciders, ["{{ approvers }}", "<@U0123ABCD>"]);
        assert!(!spec.allow_self_decision);
        assert_eq!(spec.buttons[0].style, Some(ButtonStyle::Primary));
        assert_eq!(spec.buttons[2].end, Some(RunEnd::Cancel));
        assert_eq!(
            spec.buttons[3].workflow.as_ref().unwrap().payload["subject"],
            "{{ steps.draft.subject }}"
        );

        let written = toml::to_string(&def).unwrap();
        let again = WorkflowDefinition::from_toml(&written).unwrap();
        assert_eq!(again.get_step("review").unwrap().decision, Some(spec));
    }

    #[test]
    fn a_bare_workflow_button_starts_and_moves_on() {
        let button: DecisionButton = toml::from_str(
            "label = \"Escalate\"\noutcome = \"escalate\"\nworkflow = \"digest-escalation\"\nend = \"complete\"\n",
        )
        .unwrap();
        assert_eq!(
            button.workflow,
            Some(ButtonWorkflow {
                id: "digest-escalation".into(),
                wait: false,
                payload: BTreeMap::new(),
            })
        );
    }

    /// The digest with `edit` applied to its text.
    fn edited(edit: impl Fn(&str) -> String) -> eyre::Result<WorkflowDefinition> {
        WorkflowDefinition::from_toml(&edit(DIGEST))
    }

    #[test]
    fn a_decision_refuses_what_its_rules_forbid() {
        type Edit = Box<dyn Fn(&str) -> String>;
        let cases: Vec<(&str, Edit, &str)> = vec![
            (
                "without a pause",
                Box::new(|t| t.replace("pause = \"awaiting-approval\"\n", "")),
                "no `pause`",
            ),
            (
                "without a delivered report",
                Box::new(|t| {
                    t.replace("report_render = \"template\"\nreport = \"To: {{ steps.draft.recipient }}\"\n", "task = \"Check it.\"\n")
                }),
                "no report the engine delivers",
            ),
            (
                "with a step goto",
                Box::new(|t| {
                    t.replace(
                        "pause = \"awaiting-approval\"\n",
                        "pause = \"awaiting-approval\"\ngoto = \"draft\"\n",
                    )
                }),
                "a decision routes by its buttons",
            ),
            (
                "with max_iterations",
                Box::new(|t| {
                    t.replace(
                        "pause = \"awaiting-approval\"\n",
                        "pause = \"awaiting-approval\"\nmax_iterations = 3\n",
                    )
                }),
                "max_iterations",
            ),
            (
                "with no buttons",
                Box::new(|t| {
                    t.split("[[step.decision.buttons]]")
                        .next()
                        .unwrap()
                        .to_string()
                        + "buttons = []\n"
                        + &t[t.find("[[step]]\nid = \"send_email\"").unwrap()..]
                }),
                "needs 1 to 25",
            ),
            (
                "with a duplicate label",
                Box::new(|t| t.replace("label = \"Redraft\"", "label = \"send\"")),
                "two buttons with the label 'send'",
            ),
            (
                "with a long label",
                Box::new(|t| {
                    t.replace(
                        "label = \"Redraft\"",
                        &format!("label = \"{}\"", "x".repeat(76)),
                    )
                }),
                "1 to 75 characters",
            ),
            (
                "with a bad outcome",
                Box::new(|t| t.replace("outcome = \"redraft\"", "outcome = \"re draft\"")),
                "[A-Za-z0-9_-]",
            ),
            (
                "with a duplicate outcome",
                Box::new(|t| t.replace("outcome = \"redraft\"", "outcome = \"cancel\"")),
                "outcome 'cancel'",
            ),
            (
                "with typed forms that collide",
                Box::new(|t| {
                    t.replace("label = \"Redraft\"", "label = \"Escalate now\"")
                        .replace("label = \"Escalate\"", "label = \"Redraft\"")
                }),
                "can't be told apart",
            ),
            (
                "with two primaries",
                Box::new(|t| t.replace("style = \"danger\"", "style = \"primary\"")),
                "more than one primary",
            ),
            (
                "with a long done",
                Box::new(|t| {
                    t.replace("done = \"Sent\"", &format!("done = \"{}\"", "x".repeat(76)))
                }),
                "done needs",
            ),
            (
                "with a blank done",
                Box::new(|t| t.replace("done = \"Sent\"", "done = \" \"")),
                "done needs",
            ),
            (
                "with an empty confirm",
                Box::new(|t| t.replace("confirm = \"Send this email now?\"", "confirm = \" \"")),
                "confirm needs",
            ),
            (
                "with no route",
                Box::new(|t| t.replace("goto = \"draft\"\n", "")),
                "it has none",
            ),
            (
                "with two routes",
                Box::new(|t| t.replace("done = \"Sent\"", "done = \"Sent\"\nend = \"complete\"")),
                "it has step 'send_email', end = \"complete\"",
            ),
            (
                "with a goto to nowhere",
                Box::new(|t| t.replace("goto = \"draft\"", "goto = \"nowhere\"")),
                "unknown step 'nowhere'",
            ),
            (
                "with a waited workflow",
                Box::new(|t| {
                    t.replace(
                        "workflow = { id = \"digest-escalation\",",
                        "workflow = { id = \"digest-escalation\", wait = true,",
                    )
                }),
                "to wait for it",
            ),
            (
                "with a workflow and a cancel",
                Box::new(|t| {
                    t.replace(
                        "set = { escalated = \"yes\" }\nend = \"complete\"",
                        "end = \"cancel\"",
                    )
                }),
                "cancelling a run stops",
            ),
            (
                "with its own workflow",
                Box::new(|t| {
                    t.replace("id = \"digest-escalation\"", "id = \"daily-digest-email\"")
                }),
                "its own workflow",
            ),
            (
                "with another member's workflow",
                Box::new(|t| {
                    t.replace("id = \"digest-escalation\"", "id = \"someone/escalation\"")
                }),
                "bare id",
            ),
            (
                "with a payload that does not compile",
                Box::new(|t| {
                    t.replace(
                        "subject = \"{{ steps.draft.subject }}\"",
                        "subject = \"{{ \"",
                    )
                }),
                "does not compile",
            ),
            (
                "with an engine set key",
                Box::new(|t| t.replace("escalated = \"yes\"", "_run_as = \"yes\"")),
                "sets '_run_as'",
            ),
            (
                "with a set template",
                Box::new(|t| t.replace("escalated = \"yes\"", "escalated = \"{{ approvers }}\"")),
                "set values are literal",
            ),
            (
                "with no deciders",
                Box::new(|t| {
                    t.replace(
                        "deciders = [\"{{ approvers }}\", \"<@U0123ABCD>\"]",
                        "deciders = []",
                    )
                }),
                "names no deciders",
            ),
            (
                "with a blank decider",
                Box::new(|t| t.replace("\"<@U0123ABCD>\"", "\" \"")),
                "blank decider",
            ),
            (
                "with a decider from a step",
                Box::new(|t| t.replace("\"<@U0123ABCD>\"", "\"{{ steps.draft.recipient }}\"")),
                "step output",
            ),
            (
                "with an undeclared decider",
                Box::new(|t| t.replace("\"<@U0123ABCD>\"", "\"{{ reviewer }}\"")),
                "no [[variables]] entry declares",
            ),
            (
                "with an engine decider",
                Box::new(|t| t.replace("\"<@U0123ABCD>\"", "\"{{ _run_as }}\"")),
                "engine variable",
            ),
            (
                "with a bad channel",
                Box::new(|t| t.replace("channel = \"C0123456789\"", "channel = \"#general\"")),
                "not \"dm\" or a Slack channel id",
            ),
            (
                "with an expiry alone",
                Box::new(|t| t.replace("on_expiry = \"cancel\"\n", "")),
                "they go together",
            ),
            (
                "with an expiry no button has",
                Box::new(|t| t.replace("on_expiry = \"cancel\"", "on_expiry = \"ignore\"")),
                "not the outcome of any button",
            ),
            (
                "with too short an expiry",
                Box::new(|t| t.replace("expires_after = \"20h\"", "expires_after = \"30s\"")),
                "between 1m and 30d",
            ),
            (
                "with an expiry that does not parse",
                Box::new(|t| t.replace("expires_after = \"20h\"", "expires_after = \"soon\"")),
                "expires_after 'soon'",
            ),
            (
                "with an unknown decision key",
                Box::new(|t| {
                    t.replace(
                        "allow_self_decision = false",
                        "allow_self_decisions = false",
                    )
                }),
                "allow_self_decisions",
            ),
            (
                "with an unknown button key",
                Box::new(|t| t.replace("done = \"Sent\"", "dnoe = \"Sent\"")),
                "dnoe",
            ),
            (
                "with an unknown workflow key",
                Box::new(|t| {
                    t.replace(
                        "workflow = { id = \"digest-escalation\",",
                        "workflow = { id = \"digest-escalation\", waits = false,",
                    )
                }),
                "did not match any variant",
            ),
        ];
        for (case, edit, said) in cases {
            let refused = edited(edit).expect_err(case);
            let text = format!("{refused:#}");
            assert!(text.contains(said), "{case}: {text}");
        }
    }

    #[test]
    fn a_decision_reading_a_context_field_or_a_secret_is_refused() {
        let context = edited(|t| {
            t.replace("name = \"Daily digest email\"\n", "name = \"Daily digest email\"\n[workflow.context]\nreviewer = { required = false }\n")
                .replace("\"<@U0123ABCD>\"", "\"{{ reviewer }}\"")
        })
        .unwrap_err();
        assert!(format!("{context:#}").contains("context field 'reviewer'"));
        let secret = edited(|t| {
            t.replace("[[step]]\nid = \"draft\"", "[[variables]]\nid = \"key\"\ntype = \"secret\"\nvalue = \"[[vault:agent:key]]\"\n\n[[step]]\nid = \"draft\"")
                .replace("\"<@U0123ABCD>\"", "\"{{ key }}\"")
        })
        .unwrap_err();
        assert!(format!("{secret:#}").contains("secret variable 'key'"));
    }

    #[test]
    fn deciders_render_from_the_definitions_variables_never_the_payload() {
        let def = digest();
        let spec = def.get_step("review").unwrap().decision.clone().unwrap();
        assert_eq!(
            spec.decider_tokens(&def).unwrap(),
            ["approver.one@example.com", "team:reviewers", "<@U0123ABCD>"]
        );
        let mut instance = paused_at_review(&def);
        instance.variables["approvers"] = serde_json::json!("intruder@example.com");
        assert!(!spec
            .decider_tokens(&def)
            .unwrap()
            .contains(&"intruder@example.com".to_string()));
    }

    #[test]
    fn opening_freezes_each_choices_next_step() {
        let def = digest();
        let mut instance = paused_at_review(&def);
        let decision = opened(&def, &mut instance);
        let nexts: Vec<&Next> = decision.choices.iter().map(|c| &c.next).collect();
        assert_eq!(
            nexts,
            [
                &Next::Step("send_email".into()),
                &Next::Step("draft".into()),
                &Next::Cancel,
                &Next::Complete
            ]
        );
        assert_eq!(decision.title, "Send today's digest to all@example.com?");
        assert_eq!(decision.record, 1);
        assert_eq!(
            decision.expires_at,
            Some(now() + chrono::Duration::hours(20))
        );
        assert_eq!(
            decision.choices[3].start,
            Some(ChildStart {
                workflow: "digest-escalation".into(),
                payload: serde_json::json!({ "subject": "Today" })
                    .as_object()
                    .unwrap()
                    .clone(),
            })
        );
        assert_eq!(open_in(&instance), Some(decision));
    }

    #[test]
    fn a_choice_is_found_by_outcome_then_label_ignoring_case() {
        let def = digest();
        let decision = opened(&def, &mut paused_at_review(&def));
        let found = |asked| decision.choice(asked).map(|c| c.outcome.as_str());
        assert_eq!(found("send"), Some("send"));
        assert_eq!(found(" REDRAFT "), Some("redraft"));
        assert_eq!(found("Cancel"), Some("cancel"));
        assert_eq!(found("ESCALATE"), Some("escalate"));
        assert_eq!(found("approve"), None);
    }

    #[test]
    fn a_choice_label_is_found_ignoring_case_beyond_ascii() {
        let def = digest();
        let mut decision = opened(&def, &mut paused_at_review(&def));
        decision.choices[0].label = "Approuvé".into();
        assert_eq!(
            decision.choice("APPROUVÉ").map(|c| c.outcome.as_str()),
            Some("send")
        );
    }

    #[test]
    fn a_title_that_renders_blank_is_the_steps_name() {
        let def = WorkflowDefinition::from_toml(&DIGEST.replace(
            "title = \"Send today's digest to {{ steps.draft.recipient }}?\"",
            "title = \"{{ steps.draft.blank }}\"",
        ))
        .unwrap();
        let decision = opened(&def, &mut paused_at_review(&def));
        assert_eq!(decision.title, def.get_step("review").unwrap().name);
    }

    #[test]
    fn a_decider_matches_by_identity_slack_or_email() {
        let named = Decider {
            identity_id: Some("id-1".into()),
            email: Some("Alice@Example.com".into()),
            slack_user_id: Some("U1".into()),
            display: "Alice".into(),
        };
        let by = |id: &str, email: Option<&str>, slack: Option<&str>| Person {
            id: id.into(),
            email: email.map(Into::into),
            slack_user_id: slack.map(Into::into),
        };
        assert!(named.is(&by("id-1", None, None)));
        assert!(named.is(&by("merged", None, Some("U1"))));
        assert!(named.is(&by("merged", Some("alice@example.com"), None)));
        assert!(!named.is(&by("other", Some("bob@example.com"), Some("U2"))));
        let unlinked = Decider {
            identity_id: None,
            email: None,
            slack_user_id: None,
            display: "nobody".into(),
        };
        assert!(!unlinked.is(&by("other", None, None)));
    }

    #[test]
    fn self_decision_is_refused_only_when_disallowed() {
        let def = digest();
        let mut decision = opened(&def, &mut paused_at_review(&def));
        assert_eq!(
            decision.refusal(Some(&person("alice")), Some("alice")),
            Some(Refusal::OwnDecision)
        );
        assert_eq!(decision.refusal(Some(&person("bob")), Some("alice")), None);
        assert_eq!(
            decision.refusal(Some(&person("carol")), Some("alice")),
            Some(Refusal::NotDecider)
        );
        assert_eq!(
            decision.refusal(None, Some("alice")),
            Some(Refusal::NotDecider)
        );
        assert_eq!(
            decision
                .waiting_on(Some("alice"))
                .iter()
                .map(|d| d.display.as_str())
                .collect::<Vec<_>>(),
            ["BOB"]
        );
        decision.allow_self_decision = true;
        assert_eq!(
            decision.refusal(Some(&person("alice")), Some("alice")),
            None
        );
    }

    #[test]
    fn opening_with_only_the_starter_and_self_decision_off_fails() {
        let def = digest();
        let instance = paused_at_review(&def);
        let step = def.get_step("review").unwrap();
        let opening = |deciders| Opening {
            deciders,
            target: CardTarget::DirectMessages,
            body: None,
            agent: None,
            starter: Some("alice"),
            now: now(),
        };
        let only_starter = RunDecision::open(
            step.decision.as_ref().unwrap(),
            step,
            &instance,
            opening(vec![decider("alice")]),
        )
        .unwrap_err();
        assert!(
            only_starter.contains("allow_self_decision is false"),
            "{only_starter}"
        );
        let nobody = RunDecision::open(
            step.decision.as_ref().unwrap(),
            step,
            &instance,
            opening(vec![]),
        )
        .unwrap_err();
        assert!(nobody.contains("resolved to nobody"), "{nobody}");
    }

    #[test]
    fn settling_records_the_outcome_as_a_resume_applies_set_and_runs_the_run() {
        let def = digest();
        let mut instance = paused_at_review(&def);
        let decision = opened(&def, &mut instance);
        let escalate = decision.choice("escalate").unwrap().clone();
        let by = DecidedBy::Person {
            id: "bob".into(),
            display: "Bob".into(),
            slack_user_id: None,
        };

        settle(
            &mut instance,
            &escalate,
            by.clone(),
            Some("looks off"),
            now(),
        )
        .unwrap();

        let record = instance.last_record_of("review").unwrap();
        assert_eq!(record.outcome.as_deref(), Some("escalate"));
        assert_eq!(record.provenance.outcome_by, Some(OutcomeBy::Resume));
        assert_eq!(instance.variables["escalated"], "yes");
        assert_eq!(instance.variables["_human_feedback"], "looks off");
        assert_eq!(instance.status, InstanceStatus::Running);
        let settled = awaiting_follow(&instance).unwrap();
        assert_eq!(
            settled.closed,
            Some(Closed::Decided {
                outcome: "escalate".into(),
                by,
                at: now(),
                followed: false,
                child: None
            })
        );
        assert_eq!(settled.summary().as_deref(), Some("Escalate by Bob"));
    }

    #[test]
    fn settling_a_closed_decision_is_refused() {
        let def = digest();
        let mut instance = paused_at_review(&def);
        let decision = opened(&def, &mut instance);
        let send = decision.choice("send").unwrap().clone();
        settle(&mut instance, &send, DecidedBy::Expiry, None, now()).unwrap();
        instance.status = InstanceStatus::Paused;
        assert_eq!(
            settle(&mut instance, &send, DecidedBy::Expiry, None, now()),
            Err(NotOpen)
        );
    }

    #[test]
    fn a_loop_back_leaves_no_open_decision() {
        let def = digest();
        let mut instance = paused_at_review(&def);
        opened(&def, &mut instance);
        instance.record_step("draft", None, "again", now(), 1, 0.0);
        instance.record_step("review", None, "", now(), 1, 0.0);
        assert_eq!(open_in(&instance), None);
        assert_eq!(awaiting_follow(&instance), None);
    }

    #[test]
    fn follow_moves_to_the_frozen_step_or_ends_the_run() {
        let def = digest();
        let follow_with = |outcome: &str, by: DecidedBy| {
            let mut instance = paused_at_review(&def);
            let decision = opened(&def, &mut instance);
            settle(
                &mut instance,
                decision.choice(outcome).unwrap(),
                by,
                None,
                now(),
            )
            .unwrap();
            let followed = follow(&mut instance, &def, Utc::now()).unwrap();
            (followed, instance)
        };
        let person = DecidedBy::Person {
            id: "bob".into(),
            display: "Bob".into(),
            slack_user_id: None,
        };

        let (to_draft, run) = follow_with("redraft", person.clone());
        assert_eq!(to_draft, Followed::Step("draft".into()));
        assert_eq!(
            (run.current_step_id.as_deref(), &run.status),
            (Some("draft"), &InstanceStatus::Running)
        );
        assert_eq!(awaiting_follow(&run), None);

        let (completed, run) = follow_with("escalate", person.clone());
        assert_eq!(
            (completed, run.status),
            (Followed::Completed, InstanceStatus::Completed)
        );

        let (cancelled, run) = follow_with("cancel", person);
        assert_eq!(
            cancelled,
            Followed::Cancelled("Ended at 'review': Cancel by Bob".into())
        );
        assert_eq!(run.status, InstanceStatus::Cancelled);

        let (expired, _) = follow_with("cancel", DecidedBy::Expiry);
        assert_eq!(
            expired,
            Followed::Cancelled("Expired at 'review': Cancel".into())
        );
    }

    #[test]
    fn follow_refuses_a_step_the_definition_no_longer_has() {
        let def = digest();
        let mut instance = paused_at_review(&def);
        let decision = opened(&def, &mut instance);
        settle(
            &mut instance,
            decision.choice("redraft").unwrap(),
            DecidedBy::Expiry,
            None,
            now(),
        )
        .unwrap();
        let mut without_draft = def.clone();
        without_draft.steps.retain(|step| step.id != "draft");
        assert_eq!(
            follow(&mut instance, &without_draft, Utc::now()),
            Err("the decision chose step 'draft', which the workflow no longer has".into())
        );
        assert!(awaiting_follow(&instance).is_some());
    }

    #[test]
    fn an_expired_decision_presses_its_on_expiry_choice() {
        let def = digest();
        let decision = opened(&def, &mut paused_at_review(&def));
        assert_eq!(decision.expired(now()), None);
        assert_eq!(
            decision
                .expired(now() + chrono::Duration::hours(20))
                .map(|c| c.outcome.as_str()),
            Some("cancel")
        );
    }

    #[test]
    fn the_target_is_the_run_thread_the_named_channel_or_direct_messages() {
        assert_eq!(
            target(None, Some("C1"), Some("1.0")),
            CardTarget::Thread {
                channel: "C1".into(),
                thread: Some("1.0".into())
            }
        );
        assert_eq!(
            target(Some("C1"), Some("C1"), None),
            CardTarget::Thread {
                channel: "C1".into(),
                thread: None
            }
        );
        assert_eq!(
            target(Some("C2"), Some("C1"), Some("1.0")),
            CardTarget::Channel {
                channel: "C2".into()
            }
        );
        assert_eq!(
            target(Some("dm"), Some("C1"), None),
            CardTarget::DirectMessages
        );
        assert_eq!(target(None, None, None), CardTarget::DirectMessages);
        assert_eq!(
            target(Some("C2"), None, None),
            CardTarget::Channel {
                channel: "C2".into()
            }
        );
    }

    #[test]
    fn a_decision_is_kept_by_a_run_started_with_no_payload() {
        let def = digest();
        let decision = opened(&def, &mut paused_at_review(&def));
        let mut variables = serde_json::Value::Null;
        decision.write(&mut variables);
        assert_eq!(RunDecision::read(&variables), Some(decision));
    }

    #[test]
    fn stopping_closes_only_an_open_decision() {
        let def = digest();
        let mut instance = paused_at_review(&def);
        opened(&def, &mut instance);
        assert!(stop(&mut instance, Some("Carol".into()), now()));
        assert!(!stop(&mut instance, None, now()));
        assert_eq!(
            RunDecision::read(&instance.variables)
                .unwrap()
                .summary()
                .as_deref(),
            Some("stopped by Carol; nothing was decided")
        );
    }
}
