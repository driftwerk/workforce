//! The hub's System One decision backends and what they are used for: the
//! workflow branch fallback (a step that ended with no route of its own), decision
//! steps (a step that is nothing but a question), an agent step's
//! `decide_first` (its outcome, asked before its turn), the `decide.ask`
//! tool and the pre-agent scope gate.
//!
//! Configured once at boot from `[decision]` and held process-wide, like the
//! privacy route. Every use is off until `[decision]` is enabled, and each
//! has its own switch under `[decision.uses]`.
//!
//! Each use states what it decides about as a `workforce_decide::Input`
//! and sends the request that input builds within the use's `state_chars`
//! (the whole request: state and questions): the one builder the offline
//! exporter runs too, so a model is trained on what it is asked. An input
//! with nothing to decide about is never sent. The branch fallback,
//! `decide_first`, the scope gate and routing keep what they asked and what
//! came back
//! ([`crate::decision_records`]), which is what that training reads.
//! Each call is asked within its use's `budget_ms`; a backend that is busy
//! or out of time is an error like any other, never an answer: the use
//! takes its non-decision path, and a decision step, which has none, asks
//! again.
//!
//! Backend keys are not part of that boot-time setup: every request reads
//! `decision/<backend>/api_key` from the layered vault for its [`Caller`]
//! (the config's `api_key` is the last resort), so people bring their own
//! key, a rotation applies to the next request, and a backend the caller has
//! no key for is skipped for that request.

use eyre::{eyre, Result, WrapErr};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tracing::{debug, info, warn};
use url::Url;
use workforce_common::settings::{
    AgentDecisionConfig, DecideFirstUse, DecisionBackend, DecisionConfig, DecisionLimits,
    DecisionUse, RoutingUse, ScopeGateUse,
};
use workforce_decide::branch::{self, BranchOption, StepResult, QUESTION_ID};
use workforce_decide::first::{self, Floors};
use workforce_decide::routing::{self, Route, UseCase};
use workforce_decide::scope::{self, ScopeVerdict};
use workforce_decide::{
    Answer, Backend, Call, DecideError, Decider, DeciderPolicy, Decision, Input, NoulCriteria,
    Question, Request, RequestBudget, SystemOneClient, Use,
};

use crate::credentials::Caller;
use crate::decision_records::{Pending, Recorder, Subject};
use crate::scope_gate::{ScopeDecider, ScopeOutcome};
use crate::skill_loader::CredScope;
use workforce_common::settings::VaultSource;
use workforce_orchestrator::workflow::definition::{
    DecideFirst, DecideKind, DecideSpec, Floor, StepDefinition, WorkflowDefinition,
};
use workforce_orchestrator::workflow::engine::{branch_outcome, parse_outcome, FirstAsk, StepMode};
use workforce_orchestrator::workflow::instance::{
    DecidedFirst, OutcomeBy, StepProvenance, StepRecord, WorkflowInstance, DECISION_ENGINE,
};

/// The install's decision setup: the backends and which uses are on.
pub struct Decisions {
    decider: Decider,
    keys: BTreeMap<String, BackendKey>,
    /// Floor for the branch fallback, and for a decision step that sets none.
    min_probability: f64,
    branch_fallback: Option<BranchFallback>,
    decision_steps: bool,
    tool: bool,
    scope_gate: ScopeGateUse,
    routing: RoutingUse,
    decide_first: DecideFirstUse,
    limits: BTreeMap<Use, Limits>,
}

struct BranchFallback {
    shadow: bool,
}

/// What one use may spend on a decision: time on each backend it is put
/// to, queueing included, and text in its request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Limits {
    budget: Duration,
    request: RequestBudget,
}

impl Limits {
    /// `usage`'s defaults, overridden by what its config table sets.
    fn of(usage: Use, configured: DecisionLimits) -> (Use, Self) {
        let limits = Self {
            budget: configured
                .budget_ms
                .map_or(usage.default_budget(), |ms| Duration::from_millis(ms.get())),
            request: configured
                .state_chars
                .map_or(RequestBudget::DEFAULT, |chars| {
                    RequestBudget::chars(chars.get())
                }),
        };
        (usage, limits)
    }
}

/// What an agent step produced, as the workflow recorded it.
#[derive(Clone, Copy)]
pub struct Produced<'a> {
    pub response: &'a str,
    /// The declared outputs parsed from the response.
    pub outputs: Option<&'a Value>,
    pub report: Option<&'a str>,
}

/// What the branch fallback decides about for `step`: what the step
/// produced, its task as the definition writes it, and its branches, each
/// with the name of the step it routes to. Whoever gathers training
/// examples from recorded runs builds them with this.
pub fn branch_input(
    def: &WorkflowDefinition,
    step: &StepDefinition,
    produced: &Produced<'_>,
) -> Input {
    let text = |value: &Value| {
        value
            .as_str()
            .map_or_else(|| value.to_string(), str::to_string)
    };
    Input::BranchFallback {
        result: StepResult {
            step: step.name.clone(),
            outputs: produced
                .outputs
                .and_then(Value::as_object)
                .into_iter()
                .flatten()
                .map(|(name, value)| (name.clone(), text(value)))
                .collect(),
            report: produced.report.map(str::to_string),
            response: produced.response.to_string(),
        },
        task: step.task.clone(),
        branches: step
            .branches
            .iter()
            .map(|b| {
                let leads_to = def.get_step(&b.goto).map(|target| target.name.clone());
                BranchOption::new(b.when.clone(), leads_to)
            })
            .collect(),
    }
}

/// Where one backend's key comes from: its vault credential, read for the
/// caller, then the install config's `api_key`.
struct BackendKey {
    path: String,
    scope: CredScope,
    configured: Option<String>,
    /// Whether a caller with no key is kept away from the backend.
    required: bool,
}

/// Where a caller's key for a backend comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeySource {
    Vault(VaultSource),
    InstallConfig,
    Missing,
}

/// Whether a caller's decisions can be put to one backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendAccess {
    pub name: String,
    pub primary: bool,
    pub requires_key: bool,
    pub key: KeySource,
    /// The vault path a key for it is stored at.
    pub path: String,
}

impl BackendAccess {
    pub fn available(&self) -> bool {
        !self.requires_key || self.key != KeySource::Missing
    }
}

/// The vault path of the key for the backend named `backend`.
fn key_path(backend: &str) -> String {
    format!("decision/{backend}/api_key")
}

impl BackendKey {
    /// `backend`'s key at `url`. A caller with no key is kept away when the
    /// config says so, else from a backend people bring their own key to and
    /// from any public address: only a backend on a private network is asked
    /// without one.
    fn of(backend: &DecisionBackend, url: &Url) -> Self {
        let path = key_path(&backend.name);
        let scope = crate::credentials::catalogued(&path).map_or(CredScope::Shared, |c| c.scope);
        Self {
            required: backend.requires_key.unwrap_or_else(|| {
                scope.allows_per_user() || !crate::credentials::is_internal(url)
            }),
            path,
            scope,
            configured: backend
                .api_key
                .as_deref()
                .map(str::trim)
                .filter(|k| !k.is_empty())
                .map(str::to_string),
        }
    }

    fn resolve(&self, caller: &Caller) -> Option<String> {
        caller
            .resolve(&self.path, self.scope)
            .or_else(|| self.configured.clone())
    }

    fn source(&self, caller: &Caller) -> KeySource {
        match caller.level(&self.path, self.scope) {
            Some(tier) => KeySource::Vault(tier),
            None if self.configured.is_some() => KeySource::InstallConfig,
            None => KeySource::Missing,
        }
    }
}

/// `e` as a person or an agent reads it: a missing key says who adds one
/// and where, and a busy backend says nothing was decided.
pub fn explain(e: &DecideError) -> String {
    match e {
        busy if busy.is_overloaded() => {
            format!("the decision model is busy and nothing was decided ({busy})")
        }
        DecideError::Unavailable { backends } => std::iter::once(format!("{e}."))
            .chain(
                backends
                    .iter()
                    .map(|b| crate::credentials::how_to_add(&key_path(b))),
            )
            .collect::<Vec<_>>()
            .join(" "),
        other => other.to_string(),
    }
}

static SLOT: OnceLock<parking_lot::RwLock<Option<Arc<Decisions>>>> = OnceLock::new();

fn slot() -> &'static parking_lot::RwLock<Option<Arc<Decisions>>> {
    SLOT.get_or_init(|| parking_lot::RwLock::new(None))
}

pub fn decisions() -> Option<Arc<Decisions>> {
    slot().read().clone()
}

impl Decisions {
    pub fn primary_name(&self) -> &str {
        self.decider.primary_name()
    }

    /// Each backend as `caller` reaches it, the primary first.
    pub fn access(&self, caller: &Caller) -> Vec<BackendAccess> {
        let primary = self.primary_name();
        let mut backends: Vec<BackendAccess> = self
            .keys
            .iter()
            .map(|(name, key)| BackendAccess {
                name: name.clone(),
                primary: name == primary,
                requires_key: key.required,
                key: key.source(caller),
                path: key.path.clone(),
            })
            .collect();
        backends.sort_by_key(|b| !b.primary);
        backends
    }

    /// Whether the branch fallback is on, and only recording what it would pick.
    pub fn branch_fallback(&self) -> Option<bool> {
        self.branch_fallback.as_ref().map(|b| b.shadow)
    }

    pub fn tool_enabled(&self) -> bool {
        self.tool
    }

    pub fn decision_steps_enabled(&self) -> bool {
        self.decision_steps
    }

    pub fn scope_gate(&self) -> ScopeGateUse {
        self.scope_gate
    }

    pub fn routing(&self) -> RoutingUse {
        self.routing
    }

    pub fn decide_first_use(&self) -> DecideFirstUse {
        self.decide_first
    }

    fn limits(&self, usage: Use) -> Limits {
        self.limits[&usage]
    }

    /// How much text one of `usage`'s requests may carry, state and
    /// questions together.
    pub fn request_budget(&self, usage: Use) -> RequestBudget {
        self.limits(usage).request
    }

    /// The request `input` makes within its use's budget, or `None` when it
    /// carries nothing to decide about. Every request the hub sends is
    /// built here.
    fn request(&self, input: &Input) -> Option<Request> {
        input.request(self.request_budget(input.usage()))
    }

    /// `input` put to the backends as its use puts it, or `None` when it
    /// carries nothing to decide about: for whoever asks a recorded input
    /// again, offline.
    pub async fn put(
        &self,
        caller: &Caller,
        input: &Input,
    ) -> Result<Option<Decision>, DecideError> {
        match self.request(input) {
            Some(Request { state, questions }) => self
                .ask(caller, input.usage(), &state, &questions)
                .await
                .map(Some),
            None => Ok(None),
        }
    }

    /// Whether `text` is off-topic for a scope whose remit is `topic`,
    /// rejecting only at or above `floor`, with the records of what was
    /// asked for the gate to keep once it knows what its other classifier
    /// said. A blank message is not off-topic, and nothing is asked. While
    /// routing is switched on its question rides in the same request, as
    /// long as that changes neither: the two build the same state and fit
    /// one budget together. A longer message is put to each alone, at once,
    /// so each is asked exactly what it is trained on.
    pub async fn scope_verdict(
        &self,
        caller: &Caller,
        subject: &Subject,
        text: &str,
        topic: &str,
        floor: f64,
    ) -> Result<(ScopeVerdict, Pending), DecideError> {
        let asked = Input::ScopeGate {
            message: text.to_string(),
            topic: topic.to_string(),
        };
        let Some(scope) = self.request(&asked) else {
            let on_topic = ScopeVerdict {
                off_topic: false,
                p_off_topic: 0.0,
            };
            return Ok((on_topic, Pending::default()));
        };
        let routing = (self.routing.mode != DecisionUse::Off)
            .then(|| self.routing_input(subject, text))
            .and_then(|input| Some((self.request(&input)?, input)));
        let budget = self
            .request_budget(Use::ScopeGate)
            .get()
            .min(self.request_budget(Use::Routing).get());
        let together = routing
            .as_ref()
            .and_then(|(routing, _)| scope.joined(routing, RequestBudget::chars(budget)));
        let (decision, rode) = match (together, &routing) {
            (Some(Request { state, questions }), Some((_, routing))) => {
                let decision = self.ask(caller, Use::ScopeGate, &state, &questions).await?;
                let rode = self.routed(subject, routing, &decision);
                (decision, rode)
            }
            (_, Some((_, routing))) => {
                let (decision, alone) = tokio::join!(
                    self.ask(caller, Use::ScopeGate, &scope.state, &scope.questions),
                    self.route_input(caller, subject, routing)
                );
                if let Err(e) = alone {
                    debug!(target: "decision", error = %e, "the request was not classified");
                }
                (decision?, Pending::default())
            }
            _ => (
                self.ask(caller, Use::ScopeGate, &scope.state, &scope.questions)
                    .await?,
                Pending::default(),
            ),
        };
        rode.keep(None);
        let verdict = scope::verdict(&decision.response.answers[scope::QUESTION_ID], floor);
        let acted = self.scope_gate.mode == DecisionUse::On;
        let budget = self.request_budget(Use::ScopeGate);
        Ok((
            verdict,
            Pending::of(subject, &asked, budget, &decision, acted),
        ))
    }

    fn routing_input(&self, subject: &Subject, text: &str) -> Input {
        Input::Routing {
            message: text.to_string(),
            agent: subject.agent_id.clone(),
            channel: subject.channel.clone(),
        }
    }

    /// The use case of the request `text`, or `None` while routing is off.
    /// Below the install's floor, and for a blank message, the class is
    /// `ambiguous`.
    pub async fn route(
        &self,
        caller: &Caller,
        subject: &Subject,
        text: &str,
    ) -> Result<Option<Route>, DecideError> {
        if self.routing.mode == DecisionUse::Off {
            return Ok(None);
        }
        self.route_input(caller, subject, &self.routing_input(subject, text))
            .await
            .map(Some)
    }

    async fn route_input(
        &self,
        caller: &Caller,
        subject: &Subject,
        asked: &Input,
    ) -> Result<Route, DecideError> {
        let Some(Request { state, questions }) = self.request(asked) else {
            return Ok(Route {
                use_case: UseCase::Ambiguous,
                probability: 0.0,
            });
        };
        let decision = self.ask(caller, Use::Routing, &state, &questions).await?;
        self.routed(subject, asked, &decision).keep(None);
        Ok(self.route_of(&decision))
    }

    /// Log the class `decision` gives the routing input `asked`, and the
    /// record of it.
    fn routed(&self, subject: &Subject, asked: &Input, decision: &Decision) -> Pending {
        self.route_of(decision);
        let acted = self.routing.mode == DecisionUse::On;
        let budget = self.request_budget(Use::Routing);
        Pending::of(subject, asked, budget, decision, acted)
    }

    fn route_of(&self, decision: &Decision) -> Route {
        let route = routing::route(
            &decision.response.answers[routing::QUESTION_ID],
            self.routing.min_probability.get(),
        );
        info!(
            target: "decision",
            r#use = Use::Routing.as_str(),
            backend = %decision.backend,
            use_case = route.use_case.label(),
            probability = route.probability,
            "request classified"
        );
        route
    }

    /// The answers to the `questions` an agent asks of a `state` it wrote,
    /// or `None` when the state is empty.
    pub async fn ask_tool(
        &self,
        caller: &Caller,
        state: Value,
        questions: BTreeMap<String, Question>,
    ) -> Result<Option<Decision>, DecideError> {
        match self.request(&Input::Tool { state, questions }) {
            Some(Request { state, questions }) => self
                .ask(caller, Use::Tool, &state, &questions)
                .await
                .map(Some),
            None => Ok(None),
        }
    }

    /// One request to the configured backends for `usage`, with `caller`'s
    /// keys and within the use's time budget on each backend asked.
    async fn ask(
        &self,
        caller: &Caller,
        usage: Use,
        state: &Value,
        questions: &BTreeMap<String, Question>,
    ) -> Result<Decision, DecideError> {
        let keys = |backend: &str| self.keys.get(backend)?.resolve(caller);
        let call = Call::new(usage, self.limits(usage).budget);
        self.decider.ask(call, &keys, state, questions).await
    }
}

/// Build what `[decision]` describes and install it. Off → nothing. Enabled
/// with no primary, an unknown name, a fallback equal to the primary, or
/// compare without a fallback is a boot error.
pub fn install_from_config(config: &DecisionConfig) -> Result<()> {
    let built = build(config)?;
    match &built {
        Some(d) => info!(
            primary = d.decider.primary_name(),
            min_probability = d.min_probability,
            branch_fallback = d.branch_fallback.is_some(),
            shadow = d.branch_fallback.as_ref().is_some_and(|b| b.shadow),
            decision_steps = d.decision_steps,
            tool = d.tool,
            scope_gate = ?d.scope_gate,
            routing = ?d.routing,
            decide_first = ?d.decide_first,
            limits = ?d.limits,
            "decision backends configured"
        ),
        None => tracing::debug!("decision backends off"),
    }
    *slot().write() = built.map(Arc::new);
    Ok(())
}

/// What `[decision]` describes, not installed: `None` when it is off.
pub fn build(config: &DecisionConfig) -> Result<Option<Decisions>> {
    if !config.enabled {
        return Ok(None);
    }
    let primary_name = config
        .primary
        .as_deref()
        .ok_or_else(|| eyre!("[decision] enabled but primary is not set"))?;
    let find = |name: &str| -> Result<&DecisionBackend> {
        config
            .backends
            .iter()
            .find(|b| b.name == name)
            .ok_or_else(|| eyre!("[decision] no backend named {name:?}"))
    };
    let primary = find(primary_name)?;
    let secondary = match config.fallback.as_deref() {
        Some(name) if name == primary_name => {
            return Err(eyre!("[decision] fallback must differ from primary"));
        }
        Some(name) => Some(find(name)?),
        None => None,
    };
    if config.compare && secondary.is_none() {
        return Err(eyre!(
            "[decision] compare = true needs a fallback backend to compare against"
        ));
    }
    let (fb, uses) = (&config.branch_fallback, &config.uses);
    let (primary, primary_key) = client(primary)?;
    let (secondary, secondary_key) = secondary.map(client).transpose()?.unzip();
    Ok(Some(Decisions {
        decider: Decider::new(
            Arc::new(primary),
            secondary.map(Arc::new),
            DeciderPolicy {
                fallback_on_error: config.fallback.is_some(),
                compare: config.compare,
            },
        ),
        keys: std::iter::once(primary_key).chain(secondary_key).collect(),
        min_probability: fb.min_probability.get(),
        branch_fallback: fb.enabled.then_some(BranchFallback { shadow: fb.shadow }),
        decision_steps: uses.decision_steps.enabled(),
        tool: uses.tool.enabled(),
        scope_gate: uses.scope_gate,
        routing: uses.routing,
        decide_first: uses.decide_first,
        limits: BTreeMap::from([
            Limits::of(Use::DecideFirst, uses.decide_first.limits),
            Limits::of(Use::BranchFallback, fb.limits),
            Limits::of(Use::DecisionStep, uses.decision_steps.limits()),
            Limits::of(Use::Tool, uses.tool.limits()),
            Limits::of(Use::ScopeGate, uses.scope_gate.limits),
            Limits::of(Use::Routing, uses.routing.limits),
        ]),
    }))
}

/// `backend`'s client and, under its name, where its key comes from.
fn client(backend: &DecisionBackend) -> Result<(SystemOneClient, (String, BackendKey))> {
    let url = Url::parse(backend.url.trim())
        .wrap_err_with(|| format!("[decision] backend {}: url", backend.name))?;
    let key = BackendKey::of(backend, &url);
    let requires_key = key.required;
    let max_concurrent = backend.max_concurrent.max(1);
    info!(backend = %backend.name, requires_key, max_concurrent, vault_path = %key.path, "decision backend");
    let client = SystemOneClient::new(Backend {
        name: backend.name.clone(),
        url,
        requires_key,
        model: backend.model.clone(),
        timeout: Duration::from_secs(backend.timeout_secs.max(1)),
        max_concurrent,
    })
    .wrap_err_with(|| format!("[decision] backend {}: client", backend.name))?;
    Ok((client, (backend.name.clone(), key)))
}

/// The branch the decision model settles `step_id` on, when it has no route
/// of its own: an agent step with declared branches, no pause, no `goto`,
/// and no `OUTCOME:` line naming a declared branch — the step that would
/// otherwise stop its run. A step with a default route is never asked:
/// taking it without a declared outcome is that step's ordinary path, not a
/// failure. `None` leaves the step to route as it would have; in shadow the
/// pick is only recorded. The provenance says what the model picked.
pub async fn settle_branch(
    caller: &Caller,
    subject: &Subject,
    def: &WorkflowDefinition,
    step_id: &str,
    mode: &StepMode,
    produced: Produced<'_>,
    provenance: &mut StepProvenance,
) -> Option<String> {
    match decisions() {
        Some(decisions) if matches!(mode, StepMode::Agent) => {
            decisions
                .settle_branch(caller, subject, def, step_id, produced, provenance)
                .await
        }
        _ => None,
    }
}

/// Whether `step` ended on `response` with no route of its own.
pub fn has_no_route(step: &StepDefinition, response: &str) -> bool {
    step.pause.is_none()
        && step.goto.is_none()
        && !step.branches.is_empty()
        && branch_outcome(step, response).is_none()
}

impl Decisions {
    pub(crate) async fn settle_branch(
        &self,
        caller: &Caller,
        subject: &Subject,
        def: &WorkflowDefinition,
        step_id: &str,
        produced: Produced<'_>,
        provenance: &mut StepProvenance,
    ) -> Option<String> {
        let fallback = self.branch_fallback.as_ref()?;
        let step = def
            .get_step(step_id)
            .filter(|step| has_no_route(step, produced.response))?;

        let asked = branch_input(def, step, &produced);
        let (Some(Request { state, questions }), Input::BranchFallback { branches, .. }) =
            (self.request(&asked), &asked)
        else {
            debug!(target: "decision", step = step_id, "the step produced no text; nothing to settle");
            return None;
        };
        let decision = match self
            .ask(caller, Use::BranchFallback, &state, &questions)
            .await
        {
            Ok(d) => d,
            Err(e) if e.is_unavailable() => {
                debug!(step = step_id, error = %e, "no decision backend for this run; the step routes as it would have");
                return None;
            }
            Err(e) if e.is_overloaded() => {
                debug!(target: "decision", step = step_id, error = %e, "decision backend busy; the step routes as it would have");
                return None;
            }
            Err(e) => {
                warn!(step = step_id, error = %e, "branch decision failed; the step routes as it would have");
                return None;
            }
        };
        let answer = &decision.response.answers[QUESTION_ID];
        let chosen = branch::choose(answer, branches, self.min_probability);
        let acted = chosen.is_some() && !fallback.shadow;
        let budget = self.request_budget(Use::BranchFallback);
        Pending::of(subject, &asked, budget, &decision, acted).keep(None);
        let Some((label, p)) = chosen else {
            info!(
                step = step_id,
                backend = %decision.backend,
                verdict = %answer.verdict(),
                top_p = answer.top_probability(),
                "no branch chosen above the floor; the step routes as it would have"
            );
            return None;
        };
        if fallback.shadow {
            info!(step = step_id, backend = %decision.backend, %label, p, "shadow: would route on the decision model's pick");
            provenance.branch_decided_by = Some(format!("{} (shadow) p={p:.2}", decision.backend));
            return None;
        }
        info!(step = step_id, backend = %decision.backend, %label, p, fell_back = decision.fell_back, "routing on the decision model's pick");
        provenance.branch_decided_by = Some(format!("{} p={p:.2}", decision.backend));
        Some(label)
    }
}

/// What a step's `decide_first` block asks of `state`: the question over
/// its branches and described options, and the floors an answer must reach,
/// `floor` standing in where the block sets none.
pub fn first_input(step: &StepDefinition, spec: &DecideFirst, state: &str, floor: f64) -> Input {
    let branches: Vec<String> = step.branches.iter().map(|b| b.when.clone()).collect();
    Input::DecideFirst {
        state: state.to_string(),
        questions: first::questions(
            spec.question.as_deref(),
            &step.task,
            &branches,
            &spec.options,
            spec.escape,
        ),
        floors: Floors {
            min_probability: spec.min_probability.unwrap_or(floor),
            options: spec
                .floors
                .iter()
                .map(|(option, floor)| {
                    let at = match floor {
                        Floor::At(p) => Some(*p),
                        Floor::Never => None,
                    };
                    (option.clone(), at)
                })
                .collect(),
        },
    }
}

/// The option a recorded answer to `input` would have settled its step on,
/// with its probability: for whoever judges recorded answers offline.
pub fn would_settle(input: &Input, answers: &BTreeMap<String, Answer>) -> Option<(String, f64)> {
    match input {
        Input::DecideFirst {
            questions, floors, ..
        } => first::settle(
            answers.get(first::QUESTION_ID)?,
            &first::options(questions),
            floors,
        ),
        _ => None,
    }
}

/// What the decision model answered when a step asked it first.
pub struct FirstAnswer {
    asked: Input,
    budget: RequestBudget,
    decision: Decision,
    /// The option the answer settles the step on, with its probability:
    /// one that reached its own floor.
    pub pick: Option<(String, f64)>,
    /// The answer is recorded and the turn runs whatever it says.
    pub shadow: bool,
}

impl FirstAnswer {
    pub fn backend(&self) -> &str {
        &self.decision.backend
    }

    /// The option the agent's own run of `step` chose, as `record` has it:
    /// the label the ask is recorded with. `None` when the agent chose
    /// nothing: the step found no route, or its outcome was forced or given
    /// by someone else.
    pub fn agent_choice(&self, step: &StepDefinition, record: &StepRecord) -> Option<String> {
        let routed = match record.provenance.outcome_by? {
            OutcomeBy::OutcomeLine => record.outcome.as_deref(),
            OutcomeBy::Default => None,
            OutcomeBy::IterationCap | OutcomeBy::DecisionModel | OutcomeBy::Resume => return None,
        };
        let Input::DecideFirst { questions, .. } = &self.asked else {
            return None;
        };
        let branches: Vec<String> = step.branches.iter().map(|b| b.when.clone()).collect();
        Some(first::agent_choice(
            &first::options(questions),
            &branches,
            parse_outcome(&record.agent_response).as_deref(),
            routed,
        ))
    }

    /// The response a step the model settled completes with: a line saying
    /// so and the block's outputs, which must read as the step's package.
    /// `Err` says why they do not; the step is then left to its turn.
    fn settled_response(
        &self,
        ask: &FirstAsk,
        step: &StepDefinition,
        instance: &WorkflowInstance,
    ) -> std::result::Result<Option<String>, String> {
        let Some((label, p)) = &self.pick else {
            return Ok(None);
        };
        let answer = json!({
            "label": label,
            "probability": format!("{p:.2}"),
            "backend": self.decision.backend,
            "model": self.decision.response.model,
        });
        let outputs = ask.outputs(instance, answer).map_err(|missing| {
            format!("its output templates read {missing:?}, which this run does not have")
        })?;
        let block = match outputs.is_empty() {
            true => String::new(),
            false => {
                let lines: Vec<String> = outputs
                    .iter()
                    .map(|(name, value)| format!("{name}: {value}"))
                    .collect();
                format!("\n\nOUTPUTS:\n{}\nEND_OUTPUTS", lines.join("\n"))
            }
        };
        let response = format!(
            "The decision model settled this step before its turn ran: {label} (p={p:.2}, {}).{block}",
            self.decision.backend
        );
        match crate::handoff::package_of(&response, &step.outputs, false, false) {
            Ok(_) => Ok(Some(response)),
            Err(crate::handoff::Refusal::Fail(why)) => Err(why),
            Err(crate::handoff::Refusal::SendBack(correction)) => Err(correction.detail),
        }
    }
}

/// A step's `decide_first` ask, from the answer to the step's end: whether
/// it settled the step, and the record of it, which is kept exactly once —
/// acted on and unlabelled once a settled step is saved, labelled with what
/// the agent chose once a step whose turn ran is saved, and bare and not
/// acted on when the step is never saved.
pub struct AskedFirst {
    answer: FirstAnswer,
    /// The label and response of a step the model settled.
    settled: Option<(String, String)>,
    /// The question as a decision step asks it: what a settled step runs as.
    decision: DecideSpec,
    /// Whom the record is for; taken once it is kept.
    subject: Option<Subject>,
    recorder: Option<Recorder>,
}

impl AskedFirst {
    /// `answer` to `ask` for `step` of `instance`, settled when the answer
    /// reached its floor, the use is past shadow and the block's outputs
    /// read as the step's package.
    pub fn of(
        answer: FirstAnswer,
        ask: &FirstAsk,
        step: &StepDefinition,
        instance: &WorkflowInstance,
        subject: Subject,
        recorder: Option<Recorder>,
    ) -> Self {
        let response = match answer.shadow {
            true => None,
            false => answer
                .settled_response(ask, step, instance)
                .unwrap_or_else(|why| {
                    warn!(target: "decision", step = %step.id, %why, "decide_first: the settled step would hand over no valid package; its turn runs");
                    None
                }),
        };
        let settled = answer
            .pick
            .as_ref()
            .map(|(label, _)| label.clone())
            .zip(response);
        Self {
            answer,
            settled,
            decision: ask.spec.decide(),
            subject: Some(subject),
            recorder,
        }
    }

    /// The outcome the model settled the step on and the response it
    /// completes with, when it did.
    pub fn settled(&self) -> Option<(&str, &str)> {
        self.settled
            .as_ref()
            .map(|(label, response)| (label.as_str(), response.as_str()))
    }

    pub fn backend(&self) -> &str {
        self.answer.backend()
    }

    /// The response a settled step completes with, in place of its turn:
    /// the step then runs as a decision — nothing sends it back, verifies it
    /// or asks about its branch — served by the backend that answered.
    /// `None` when the model did not settle it.
    pub fn stand_in(&self, mode: &mut StepMode, provenance: &mut StepProvenance) -> Option<String> {
        let (_, response) = self.settled()?;
        *mode = StepMode::Decide(self.decision.clone());
        provenance.engine = Some(DECISION_ENGINE.into());
        provenance.model = Some(self.backend().to_string());
        Some(response.to_string())
    }

    /// Set the outcome the model settled the step on on its last record, for
    /// the engine to route on.
    pub fn preset(&self, instance: &mut WorkflowInstance, step_id: &str) {
        if let (Some((label, _)), Some((_, p))) = (self.settled(), &self.answer.pick) {
            let answer = format!("{} {label} p={p:.2}", self.backend());
            instance.settle_first(step_id, label, answer);
        }
    }

    /// Say on the record of a step whose turn ran what the model would have
    /// settled it on, and whether the agent agreed, once the engine has
    /// completed it. Nothing unless the answer reached its floor.
    pub fn stamp(&self, step: &StepDefinition, instance: &mut WorkflowInstance) {
        let Some((label, p)) = self.answer.pick.as_ref().filter(|_| self.settled.is_none()) else {
            return;
        };
        let Some(record) = instance.step_history.last_mut() else {
            return;
        };
        let agent = match self.answer.agent_choice(step, record) {
            Some(chose) if &chose == label => "agent agreed".to_string(),
            Some(chose) => format!("agent chose {chose}"),
            None => "agent chose nothing".to_string(),
        };
        let held = match self.answer.shadow {
            true => "shadow",
            false => "not settled",
        };
        record.provenance.decided_first = Some(DecidedFirst {
            settled: false,
            answer: format!("{} ({held}) {label} p={p:.2}, {agent}", self.backend()),
        });
    }

    /// Keep the record of the ask now that the step has completed and
    /// been saved: acted on when the model settled it, else labelled with
    /// what the agent chose.
    pub fn keep(mut self, step: &StepDefinition, instance: &WorkflowInstance) {
        let label = match (&self.settled, instance.step_history.last()) {
            (None, Some(record)) => self.answer.agent_choice(step, record),
            _ => None,
        };
        self.keep_record(self.settled.is_some(), label.as_deref());
    }

    fn keep_record(&mut self, acted: bool, label: Option<&str>) {
        if let Some(subject) = self.subject.take() {
            let answer = &self.answer;
            Pending::of(
                &subject,
                &answer.asked,
                answer.budget,
                &answer.decision,
                acted,
            )
            .keep_in(self.recorder.as_ref(), label);
        }
    }
}

impl Drop for AskedFirst {
    fn drop(&mut self) {
        self.keep_record(false, None);
    }
}

impl Decisions {
    /// Ask the decision model which outcome `step` should end in, before
    /// its turn, as its `decide_first` block `ask`s. `None` whenever there
    /// is no answer — the use is off for the install, the `agent` or the
    /// step, the state is empty, the caller has no key, the backends are
    /// busy, out of time or failing: the step's turn then runs as it would
    /// without the block. Never an error.
    pub async fn decide_first(
        &self,
        caller: &Caller,
        agent: Option<&AgentDecisionConfig>,
        step: &StepDefinition,
        ask: &FirstAsk,
    ) -> Option<FirstAnswer> {
        let mode = self.decide_first.for_agent(agent).min(ask.spec.mode);
        if mode == DecisionUse::Off {
            return None;
        }
        let floor = self.decide_first.min_probability.get();
        let asked = first_input(step, &ask.spec, &ask.state, floor);
        if let Input::DecideFirst {
            questions, floors, ..
        } = &asked
        {
            let unmeant = first::unmeant(questions);
            if !unmeant.is_empty() {
                warn!(target: "decision", step = %step.id, ?unmeant, "decide_first: options nothing describes; the step's turn runs");
                return None;
            }
            let tied = first::options(questions)
                .into_iter()
                .filter_map(|option| floors.of(option))
                .find(|floor| !first::breaks_ties(*floor));
            if let Some(floor) = tied.filter(|_| !ask.spec.escape) {
                warn!(target: "decision", step = %step.id, floor, "decide_first: escape = false needs every floor above 0.5 and the install's is not; the step's turn runs");
                return None;
            }
        }
        let Some(Request { state, questions }) = self.request(&asked) else {
            debug!(target: "decision", step = %step.id, "decide_first: the state is empty; the step's turn runs");
            return None;
        };
        let decision = match self.ask(caller, Use::DecideFirst, &state, &questions).await {
            Ok(decision) => decision,
            Err(e) if e.is_unavailable() || e.is_overloaded() => {
                debug!(target: "decision", step = %step.id, error = %e, "decide_first: no answer; the step's turn runs");
                return None;
            }
            Err(e) => {
                warn!(target: "decision", step = %step.id, error = %e, "decide_first failed; the step's turn runs");
                return None;
            }
        };
        let answer = decision.response.answers.get(first::QUESTION_ID)?;
        let pick = would_settle(&asked, &decision.response.answers);
        let shadow = mode == DecisionUse::Shadow;
        info!(
            target: "decision",
            step = %step.id,
            backend = %decision.backend,
            verdict = %answer.verdict(),
            top_p = answer.top_probability(),
            settles = pick.is_some(),
            shadow,
            fell_back = decision.fell_back,
            "decide_first answered"
        );
        Some(FirstAnswer {
            budget: self.request_budget(Use::DecideFirst),
            asked,
            decision,
            pick,
            shadow,
        })
    }
}

/// What a decision step produced: the rendered step response (outputs block
/// and `OUTCOME:` line included) and who answered.
pub struct Decided {
    pub response: String,
    pub backend: String,
}

/// Why a decision step has no answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Undecided {
    pub reason: String,
    /// The backends were only busy: asked again shortly, the step may well
    /// be answered.
    pub busy: bool,
}

impl Undecided {
    fn for_good(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
            busy: false,
        }
    }
}

/// How long a decision step waits before asking again of backends that were
/// busy, once per entry: the hub's stock retry shape for an overloaded
/// model. A step has no other path, so one busy moment must not end a long
/// run; a backend still busy after the last wait fails the step, and the
/// run can be retried from it.
pub const BUSY_RETRY_WAITS: [Duration; 3] = [
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(4),
];

/// Ask a decision step's question once: build it from the spec and the
/// step's branches, ask, and render the answer the way a tool step's result
/// is rendered so outputs and the outcome flow through the same machinery.
pub async fn run_decision_step(
    caller: &Caller,
    spec: &DecideSpec,
    state_text: &str,
    branches: &[String],
    declared: &[String],
) -> Result<Decided, Undecided> {
    match decisions() {
        Some(decisions) => {
            decisions
                .decision_step(caller, spec, state_text, branches, declared)
                .await
        }
        None => Err(Undecided::for_good(
            "no decision backend is configured on this install ([decision] is off)",
        )),
    }
}

impl Decisions {
    async fn decision_step(
        &self,
        caller: &Caller,
        spec: &DecideSpec,
        state_text: &str,
        branches: &[String],
        declared: &[String],
    ) -> Result<Decided, Undecided> {
        if !self.decision_steps {
            return Err(Undecided::for_good(
                "decision steps are switched off ([decision.uses] decision_steps = false)",
            ));
        }
        let floor = spec.min_probability.unwrap_or(self.min_probability);
        let asked = Input::DecisionStep {
            task: state_text.to_string(),
            questions: BTreeMap::from([(QUESTION_ID.to_string(), build_question(spec, branches))]),
        };
        let Some(Request { state, questions }) = self.request(&asked) else {
            return Err(Undecided::for_good(
                "the step's task is empty, so there is nothing to decide about",
            ));
        };
        let decision = self
            .ask(caller, Use::DecisionStep, &state, &questions)
            .await
            .map_err(|e| Undecided {
                reason: explain(&e),
                busy: e.is_overloaded(),
            })?;
        let answer = &decision.response.answers[QUESTION_ID];
        let output = render_answer(
            spec,
            answer,
            branches,
            floor,
            &decision.backend,
            &decision.response.model,
        );
        let outcome = output
            .get("outcome")
            .and_then(|v| v.as_str())
            .unwrap_or("-")
            .to_string();
        info!(
            backend = %decision.backend,
            kind = ?spec.kind,
            verdict = %answer.verdict(),
            top_p = answer.top_probability(),
            outcome = %outcome,
            fell_back = decision.fell_back,
            "decision step answered"
        );
        Ok(Decided {
            response: crate::workflow_runtime::render_tool_step_response(&output, declared),
            backend: decision.backend,
        })
    }
}

pub(crate) fn build_question(spec: &DecideSpec, branches: &[String]) -> Question {
    match spec.kind {
        DecideKind::Choice => {
            let options: Vec<(String, Value)> = if spec.options.is_empty() {
                branches.iter().map(|b| (b.clone(), Value::Null)).collect()
            } else {
                spec.options
                    .iter()
                    .map(|(k, v)| (k.clone(), Value::String(v.clone())))
                    .collect()
            };
            Question::choice(spec.question.clone(), options)
        }
        DecideKind::Noul => Question::Noul {
            instructions: Value::String(spec.question.clone()),
            criteria: (!spec.options.is_empty()).then(|| NoulCriteria {
                yes: spec.options.get("yes").cloned().map(Value::String),
                no: spec.options.get("no").cloned().map(Value::String),
            }),
        },
        DecideKind::Score => Question::score(spec.question.clone(), spec.levels.clone()),
    }
}

/// The step's output object. `outcome` is set only when the winning option
/// is a declared branch label at or above the floor; the rest is always
/// there for later steps and for anyone reading the instance.
fn render_answer(
    spec: &DecideSpec,
    answer: &Answer,
    branches: &[String],
    floor: f64,
    backend: &str,
    model: &str,
) -> Value {
    let mut out = answer_fields(answer, &spec.levels);
    out.insert("kind".into(), json!(kind_name(spec.kind)));
    out.insert("question".into(), json!(spec.question));
    out.insert("backend".into(), json!(backend));
    out.insert("model".into(), json!(model));
    out.insert("min_probability".into(), json!(floor));
    let winner = out.get("label").and_then(Value::as_str).map(str::to_string);
    let p = out
        .get("probability")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    if let Some(label) = winner.filter(|l| p >= floor && branches.iter().any(|b| b == l)) {
        out.insert("outcome".into(), json!(label));
    }
    Value::Object(out)
}

pub(crate) fn kind_name(kind: DecideKind) -> &'static str {
    match kind {
        DecideKind::Choice => "choice",
        DecideKind::Noul => "noul",
        DecideKind::Score => "score",
    }
}

/// One answer as fields: `label` and its `probability`, every option's
/// `probabilities`, plus `score` and `level` for a score and `confidence`
/// where the model reports one. Score levels come back by index and are
/// named from `levels`, so readers see words, not numbers.
pub fn answer_fields(answer: &Answer, levels: &[String]) -> serde_json::Map<String, Value> {
    let mut out = serde_json::Map::new();
    match answer {
        Answer::Choice {
            choice,
            probabilities,
            confidence,
        } => {
            out.insert("label".into(), json!(choice));
            out.insert(
                "probability".into(),
                json!(probabilities.get(choice).copied().unwrap_or(0.0)),
            );
            out.insert("probabilities".into(), json!(probabilities));
            out.insert("confidence".into(), json!(confidence));
        }
        Answer::Noul { noul } => {
            let (label, p) = if *noul >= 0.5 {
                ("yes", *noul)
            } else {
                ("no", 1.0 - *noul)
            };
            out.insert("label".into(), json!(label));
            out.insert("probability".into(), json!(p));
            out.insert(
                "probabilities".into(),
                json!({"yes": noul, "no": 1.0 - noul}),
            );
        }
        Answer::Score {
            score,
            legend,
            probabilities,
            confidence,
        } => {
            let name = |idx: &str| {
                idx.parse::<usize>()
                    .ok()
                    .and_then(|i| levels.get(i).cloned())
                    .or_else(|| legend.get(idx).cloned())
                    .unwrap_or_else(|| idx.to_string())
            };
            let (level, p) = probabilities
                .iter()
                .max_by(|a, b| a.1.total_cmp(b.1))
                .map(|(i, p)| (name(i), *p))
                .unwrap_or_default();
            let named: BTreeMap<String, f64> =
                probabilities.iter().map(|(i, p)| (name(i), *p)).collect();
            out.insert("label".into(), json!(level));
            out.insert("level".into(), json!(level));
            out.insert("score".into(), json!(score));
            out.insert("probability".into(), json!(p));
            out.insert("probabilities".into(), json!(named));
            out.insert("confidence".into(), json!(confidence));
        }
    }
    out
}

/// A stand-in System One server and a `[decision]` that points at it.
#[cfg(test)]
pub(crate) mod testing {
    use super::*;
    use axum::{routing::post, Json, Router};

    pub(crate) type Requests = Arc<parking_lot::Mutex<Vec<Value>>>;
    /// The request bodies as the bytes that arrived.
    pub(crate) type Bodies = Arc<parking_lot::Mutex<Vec<String>>>;

    pub(crate) fn backend(name: &str) -> DecisionBackend {
        DecisionBackend {
            name: name.into(),
            url: "http://127.0.0.1:9".into(),
            api_key: None,
            requires_key: None,
            model: format!("{name}-latest"),
            timeout_secs: 5,
            max_concurrent: 4,
        }
    }

    /// A server that takes `delay` over every request, then answers each
    /// choice with `choice` at 0.9 and each yes/no with 0.1, recording the
    /// request bodies.
    pub(crate) async fn recording_backend(
        choice: &'static str,
        delay: Duration,
    ) -> (String, Requests) {
        let (url, seen, _) = recording_backend_with_bodies(choice, delay).await;
        (url, seen)
    }

    /// [`recording_backend`], keeping each body's bytes too.
    pub(crate) async fn recording_backend_with_bodies(
        choice: &'static str,
        delay: Duration,
    ) -> (String, Requests, Bodies) {
        let seen = Requests::default();
        let bodies = Bodies::default();
        let record = Arc::clone(&seen);
        let raw = Arc::clone(&bodies);
        let app = Router::new().route(
            "/v1/systemone",
            post(move |bytes: String| async move {
                let body: Value = serde_json::from_str(&bytes).unwrap();
                raw.lock().push(bytes);
                record.lock().push(body.clone());
                tokio::time::sleep(delay).await;
                let answers: serde_json::Map<String, Value> = body["questions"]
                    .as_object()
                    .unwrap()
                    .iter()
                    .map(|(id, question)| {
                        let answer = match question["type"].as_str() {
                            Some("choice") => json!({
                                "type": "choice", "choice": choice,
                                "probabilities": {choice: 0.9}, "confidence": 0.9
                            }),
                            _ => json!({"type": "noul", "noul": 0.1}),
                        };
                        (id.clone(), answer)
                    })
                    .collect();
                Json(json!({"model": "fake-1", "answers": answers}))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (url, seen, bodies)
    }

    /// `[decision]` with one keyless backend, `kev`, at `url`.
    pub(crate) fn config_for(url: String) -> DecisionConfig {
        DecisionConfig {
            enabled: true,
            backends: vec![DecisionBackend {
                url,
                ..backend("kev")
            }],
            primary: Some("kev".into()),
            ..Default::default()
        }
    }
}

/// The install's decision backends, asked with the sender's keys.
struct SenderDecider<'a> {
    decisions: &'a Decisions,
    caller: &'a Caller,
    subject: &'a Subject,
}

#[async_trait::async_trait]
impl ScopeDecider for SenderDecider<'_> {
    fn rollout(&self) -> ScopeGateUse {
        self.decisions.scope_gate()
    }

    async fn verdict(
        &self,
        text: &str,
        topic: &str,
        floor: f64,
    ) -> eyre::Result<(ScopeVerdict, Pending)> {
        Ok(self
            .decisions
            .scope_verdict(self.caller, self.subject, text, topic, floor)
            .await?)
    }
}

/// Classify `text` against `scope` and report whether the agent should see it.
/// Always fails open — a flaky classifier never silences the bot. `sender`
/// is whose keys the decision model is asked with and `subject` whom its
/// answer is recorded for; `surface` and `agent_id` are for audit logging
/// only.
pub async fn evaluate_scope(
    scope: &workforce_common::settings::ScopeFilterConfig,
    llm: &dyn workforce_agent::llm::provider::LlmProvider,
    sender: &Caller,
    subject: &Subject,
    text: &str,
    surface: &str,
    agent_id: &str,
) -> ScopeOutcome {
    let decisions = decisions();
    let decider = decisions.as_deref().map(|decisions| SenderDecider {
        decisions,
        caller: sender,
        subject,
    });
    crate::scope_gate::evaluate_with(
        scope,
        llm,
        decider.as_ref().map(|d| d as &dyn ScopeDecider),
        crate::decision_records::recorder(),
        text,
        surface,
        agent_id,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::testing::{backend, config_for, recording_backend};
    use super::*;
    use crate::decision_records::testing::recorded;
    use std::time::Instant;
    use workforce_common::settings::{BranchFallbackConfig, DecisionUses, UseSwitch};
    use workforce_decide::export;
    use workforce_orchestrator::workflow::engine::parse_outcome;

    const REVIEW: &str = r#"
[workflow]
id = "review"
name = "Review"

[[step]]
id = "review"
name = "Review the PR"
task = """
Review it.
End with OUTCOME: approved when every check passes.
"""
outputs = [{ name = "summary" }]
branches = [{ when = "approved", goto = "merge" }, { when = "changes-requested", goto = "fix" }]

[[step]]
id = "merge"
name = "Merge the PR"
task = "Merge."
exit = true

[[step]]
id = "fix"
name = "Ask the author for changes"
task = "Ask."
exit = true
"#;

    fn limits(budget_ms: u64, state_chars: usize) -> DecisionLimits {
        toml::from_str(&format!(
            "budget_ms = {budget_ms}\nstate_chars = {state_chars}"
        ))
        .unwrap()
    }

    fn nobody() -> Caller {
        Caller::new(None, None, None)
    }

    /// The request the hub sent, as the bytes of its state and questions.
    fn sent(request: &Value) -> (String, String) {
        (
            request["state"].to_string(),
            request["questions"].to_string(),
        )
    }

    /// The record the offline exporter writes for `example`, as the bytes of
    /// its state and questions: the spans of the line itself, which `sent`
    /// compares with what the client serialised.
    fn exported(example: Value, state_chars: usize) -> (String, String) {
        let line = export::line(&example.to_string(), RequestBudget::chars(state_chars))
            .unwrap()
            .expect("a record");
        let record: Value = serde_json::from_str(&line).unwrap();
        let [state, questions] = ["state", "questions"].map(|field| {
            let span = record[field].to_string();
            assert!(
                line.contains(&format!("\"{field}\":{span}")),
                "the line carries {field} in other bytes than {span}: {line}"
            );
            span
        });
        (state, questions)
    }

    /// Every live path, driven through its real entry point with a long,
    /// noisy input and a state budget of its own, sends byte for byte what
    /// the exporter writes for the same input. A path that built its state
    /// or its questions any other way would fail here.
    #[tokio::test]
    async fn every_use_sends_exactly_the_state_and_questions_the_exporter_writes() {
        let (url, seen) = recording_backend("approved", Duration::ZERO).await;
        let chars =
            |n: usize| -> DecisionLimits { toml::from_str(&format!("state_chars = {n}")).unwrap() };
        let config = DecisionConfig {
            branch_fallback: BranchFallbackConfig {
                shadow: false,
                limits: chars(900),
                ..Default::default()
            },
            uses: DecisionUses {
                decision_steps: UseSwitch::Limited {
                    enabled: true,
                    limits: chars(700),
                },
                tool: UseSwitch::Limited {
                    enabled: true,
                    limits: chars(600),
                },
                scope_gate: toml::from_str("mode = \"on\"\nstate_chars = 500").unwrap(),
                routing: toml::from_str("mode = \"on\"\nstate_chars = 400").unwrap(),
                decide_first: toml::from_str("mode = \"on\"\nstate_chars = 300").unwrap(),
            },
            ..config_for(url)
        };
        let decisions = Arc::new(build(&config).unwrap().unwrap());
        let last = |id: &str| {
            let seen = seen.lock();
            let asked = seen.iter().rev().find(|r| r["questions"].get(id).is_some());
            asked.cloned().expect("a request")
        };

        let chatter = "I opened another file and it was fine. ".repeat(300);
        let log: String = (0..60)
            .map(|i| format!("2026-09-26T10:00:{i:02}Z ERROR billing: charge {i} failed\n"))
            .collect();

        let def = WorkflowDefinition::from_toml(REVIEW).unwrap();
        let response = format!(
            "{chatter}\nOn balance this is approved.\n{chatter}\n```rust\nfn a() {{}}\nfn b() {{}}\nfn c() {{}}\n```\n\
             OUTPUTS:\nsummary: all checks pass\nEND_OUTPUTS\n\
             REPORT:\nReady to merge. {chatter}\nEND_REPORT\n\
             Everything is in order, so this can go in."
        );
        let report = format!("Ready to merge. {chatter}");
        let outputs = json!({"summary": "all checks pass", "files": 12});
        let settled = decisions
            .settle_branch(
                &nobody(),
                &Subject::default(),
                &def,
                "review",
                Produced {
                    response: &response,
                    outputs: Some(&outputs),
                    report: Some(&report),
                },
                &mut StepProvenance::default(),
            )
            .await;
        assert_eq!(settled.as_deref(), Some("approved"));
        let step = def.get_step("review").unwrap();
        assert_eq!(
            sent(&last(QUESTION_ID)),
            exported(
                json!({
                    "use": "branch_fallback",
                    "step": "Review the PR",
                    "task": step.task,
                    "outputs": {"summary": "all checks pass", "files": "12"},
                    "report": report,
                    "response": response,
                    "branches": [
                        {"label": "approved", "leads_to": "Merge the PR"},
                        {"label": "changes-requested", "leads_to": "Ask the author for changes"},
                    ],
                    "label": "approved",
                }),
                900
            ),
            "branch fallback"
        );

        let message = format!("Hi.\n{log}{chatter}\nHow many charges failed yesterday?");
        let _ = decisions
            .scope_verdict(
                &nobody(),
                &Subject::default(),
                &message,
                "billing questions",
                0.7,
            )
            .await
            .unwrap();
        assert_eq!(
            sent(&last(scope::QUESTION_ID)),
            exported(
                json!({"use": "scope_gate", "message": message, "topic": "billing questions", "label": false}),
                500
            ),
            "scope gate"
        );

        decisions
            .route(&nobody(), &Subject::default(), &message)
            .await
            .unwrap();
        assert_eq!(
            seen.lock().len(),
            4,
            "routing asked beside the gate, then alone"
        );
        assert_eq!(
            sent(&last(routing::QUESTION_ID)),
            exported(
                json!({"use": "routing", "message": message, "label": "database"}),
                400
            ),
            "routing"
        );

        let task =
            json!({"title": "Checkout fails", "body": format!("{log}{chatter}"), "votes": 3})
                .to_string();
        let spec = DecideSpec {
            options: BTreeMap::from([
                ("approved".to_string(), "ready".to_string()),
                ("rejected".to_string(), "not ready".to_string()),
            ]),
            ..spec(DecideKind::Choice)
        };
        decisions
            .decision_step(&nobody(), &spec, &task, &["approved".into()], &[])
            .await
            .unwrap();
        let step_request = last(QUESTION_ID);
        assert_eq!(
            sent(&step_request),
            exported(
                json!({
                    "use": "decision_step", "task": task,
                    "questions": step_request["questions"], "label": "approved",
                }),
                700
            ),
            "decision step"
        );
        assert_eq!(
            step_request["questions"],
            json!({QUESTION_ID: build_question(&spec, &["approved".into()])})
        );

        let state = json!({"title": "Checkout fails", "notes": [chatter, log], "votes": 3});
        let tool = crate::decide_tool::DecideTool::new(Arc::clone(&decisions), None, None);
        let result = workforce_agent::tools::Tool::execute(
            &tool,
            json!({
                "state": state,
                "questions": [
                    { "id": "kind", "question": "What is it?", "options": { "bug": "broken", "feature": "new" } },
                    { "id": "urgent", "question": "Urgent?", "kind": "noul" }
                ]
            }),
        )
        .await
        .unwrap();
        assert!(result.success);
        let tool_request = last("kind");
        assert_eq!(
            sent(&tool_request),
            exported(
                json!({
                    "use": "tool", "state": state,
                    "questions": tool_request["questions"], "label": {"kind": "bug"},
                }),
                600
            ),
            "decide.ask"
        );
        assert_eq!(seen.lock().len(), 6);

        let triage = WorkflowDefinition::from_toml(TRIAGE).unwrap();
        let step = triage.get_step("classify").unwrap();
        let ask = asked_first(
            &triage,
            json!({"ticket": {"subject": "Weekly digest", "body": format!("{log}{chatter}")}}),
        );
        decisions
            .decide_first(&nobody(), None, step, &ask)
            .await
            .expect("an answer");
        let first_request = last(first::QUESTION_ID);
        assert_eq!(
            sent(&first_request),
            exported(
                json!({
                    "use": "decide_first", "state": ask.state,
                    "questions": first_request["questions"],
                    "floors": {"min_probability": 0.9, "options": {"bug": null}},
                    "label": "ignore",
                }),
                300
            ),
            "decide_first"
        );
        let written = first::questions(
            None,
            &step.task,
            &["bug".into()],
            &step.decide_first.as_ref().unwrap().options,
            true,
        );
        assert_eq!(
            first_request["questions"],
            json!(workforce_decide::question::fit(
                written,
                RequestBudget::chars(300).questions()
            )),
            "the step's branches as its task means them, held to the use's budget"
        );
        assert_eq!(seen.lock().len(), 7);
    }

    /// The `decide_first` ask a run of `def` started with `payload` makes
    /// at its first step, as the engine prepares it.
    fn asked_first(def: &WorkflowDefinition, payload: Value) -> FirstAsk {
        use workforce_orchestrator::workflow::engine::WorkflowExecutor;
        use workforce_orchestrator::workflow::instance::{TriggerInfo, TriggerSource};
        let trigger = TriggerInfo {
            source: TriggerSource::Api,
            payload,
        };
        let run = WorkflowExecutor::start(def, trigger, None).unwrap();
        let exec = WorkflowExecutor::prepare_step(def, &run).unwrap();
        exec.decide_first.expect("the step declares decide_first")
    }

    /// Without the escape the question offers the step's options alone, and
    /// what is sent is what the exporter writes; an install floor a split
    /// answer could reach is never asked under, since a block without its
    /// own floor would settle a near-tie.
    #[tokio::test]
    async fn a_step_without_the_escape_sends_what_the_exporter_writes_above_a_tie_breaking_floor() {
        let (url, seen) = recording_backend("ignore", Duration::ZERO).await;
        let triage = WorkflowDefinition::from_toml(&TRIAGE.replace(
            "floors = { bug = \"never\" }",
            "floors = { bug = \"never\" }, escape = false",
        ))
        .unwrap();
        let step = triage.get_step("classify").unwrap();
        let ask = asked_first(
            &triage,
            json!({"ticket": {"subject": "Weekly digest", "body": "Unsubscribe here"}}),
        );
        for (floor, asked) in [("0.9", true), ("0.5", false)] {
            let config = DecisionConfig {
                uses: DecisionUses {
                    decide_first: toml::from_str(&format!(
                        "mode = \"on\"\nstate_chars = 300\nmin_probability = {floor}"
                    ))
                    .unwrap(),
                    ..Default::default()
                },
                ..config_for(url.clone())
            };
            let decisions = build(&config).unwrap().unwrap();
            let answer = decisions.decide_first(&nobody(), None, step, &ask).await;
            assert_eq!(answer.is_some(), asked, "floor {floor}");
        }
        let seen = seen.lock();
        assert_eq!(seen.len(), 1, "never asked under a floor of 0.5");
        let written = first::questions(
            None,
            &step.task,
            &["bug".into()],
            &step.decide_first.as_ref().unwrap().options,
            false,
        );
        assert_eq!(
            seen[0]["questions"],
            json!(workforce_decide::question::fit(
                written,
                RequestBudget::chars(300).questions()
            ))
        );
        let offered: Vec<&String> = seen[0]["questions"][first::QUESTION_ID]["criteria"]
            .as_object()
            .unwrap()
            .keys()
            .collect();
        assert_eq!(offered, ["bug", "ignore"]);
        assert_eq!(
            sent(&seen[0]),
            exported(
                json!({
                    "use": "decide_first", "state": ask.state,
                    "questions": seen[0]["questions"],
                    "floors": {"min_probability": 0.9, "options": {"bug": null}},
                    "label": "ignore",
                }),
                300
            )
        );
    }

    const TRIAGE: &str = r#"
[workflow]
id = "triage"
name = "Triage"

[[step]]
id = "classify"
name = "Classify the ticket"
task = "Classify it. A crash or a wrong result: OUTCOME: bug"
branches = [{ when = "bug", goto = "fix" }]
goto = "done"
decide_first = { state = "{{?ticket}}", options = { ignore = "A newsletter or an auto-reply" }, floors = { bug = "never" } }

[[step]]
id = "fix"
name = "Fix it"
task = "Fix."

[[step]]
id = "done"
name = "Close it"
task = "Close."
"#;

    #[tokio::test]
    async fn the_branch_fallback_asks_of_what_the_step_concluded_and_routes_on_the_answer() {
        let (url, seen) = recording_backend("approved", Duration::ZERO).await;
        let config = DecisionConfig {
            branch_fallback: BranchFallbackConfig {
                shadow: false,
                ..Default::default()
            },
            ..config_for(url)
        };
        let decisions = build(&config).unwrap().unwrap();
        let def = WorkflowDefinition::from_toml(REVIEW).unwrap();
        let chatter = "I opened another file and it was fine. ".repeat(300);
        let response = format!(
            "{chatter}\n```rust\nfn a() {{}}\nfn b() {{}}\nfn c() {{}}\n```\n\
             OUTPUTS:\nsummary: all checks pass\nEND_OUTPUTS\n\
             REPORT:\nReady to merge.\nEND_REPORT\n\
             Everything is in order, so this can go in."
        );
        let outputs = json!({"summary": "all checks pass"});
        let mut provenance = StepProvenance::default();
        let settled = decisions
            .settle_branch(
                &nobody(),
                &Subject::default(),
                &def,
                "review",
                Produced {
                    response: &response,
                    outputs: Some(&outputs),
                    report: Some("Ready to merge."),
                },
                &mut provenance,
            )
            .await;
        assert_eq!(settled.as_deref(), Some("approved"));
        assert_eq!(provenance.branch_decided_by.as_deref(), Some("kev p=0.90"));

        let sent = seen.lock()[0].clone();
        let state = &sent["state"];
        assert_eq!(state["step"], "Review the PR");
        assert_eq!(state["outputs"], outputs);
        assert_eq!(state["report"], "Ready to merge.");
        let closing = state["final_lines"].as_str().unwrap();
        assert!(closing.ends_with("Everything is in order, so this can go in."));
        assert!(
            closing.chars().count() < RequestBudget::DEFAULT.get(),
            "not the raw tail of a {}-character response",
            response.chars().count()
        );
        let criteria = &sent["questions"][QUESTION_ID]["criteria"];
        assert_eq!(
            criteria["approved"],
            "End with OUTCOME: approved when every check passes."
        );
        assert_eq!(
            criteria["changes-requested"],
            "What the step concluded calls for the next step: Ask the author for changes"
        );
    }

    #[tokio::test]
    async fn a_step_or_a_message_with_no_text_is_never_put_to_the_model() {
        let (url, seen) = recording_backend("approved", Duration::ZERO).await;
        let config = DecisionConfig {
            branch_fallback: BranchFallbackConfig {
                shadow: false,
                ..Default::default()
            },
            uses: DecisionUses {
                scope_gate: toml::from_str("mode = \"on\"").unwrap(),
                routing: toml::from_str("mode = \"on\"").unwrap(),
                ..Default::default()
            },
            ..config_for(url)
        };
        let decisions = build(&config).unwrap().unwrap();
        let def = WorkflowDefinition::from_toml(REVIEW).unwrap();
        let mut provenance = StepProvenance::default();
        let settled = decisions
            .settle_branch(
                &nobody(),
                &Subject::default(),
                &def,
                "review",
                Produced {
                    response: " \n",
                    outputs: None,
                    report: None,
                },
                &mut provenance,
            )
            .await;
        assert_eq!(settled, None, "nothing to settle");
        assert_eq!(provenance.branch_decided_by, None);

        let (verdict, pending) = decisions
            .scope_verdict(&nobody(), &recorded(), "  \n", "billing", 0.7)
            .await
            .unwrap();
        assert!(pending.is_empty(), "nothing asked, nothing to keep");
        assert!(!verdict.off_topic, "the gate fails open on nothing");
        let route = decisions
            .route(&nobody(), &Subject::default(), "")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(route.use_case, UseCase::Ambiguous);

        let empty = decisions
            .decision_step(&nobody(), &spec(DecideKind::Choice), "  ", &[], &[])
            .await
            .err()
            .unwrap();
        assert!(
            !empty.busy && empty.reason.contains("task is empty"),
            "{empty:?}"
        );
        let tool = decisions
            .ask_tool(&nobody(), json!({}), questions())
            .await
            .unwrap();
        assert!(tool.is_none());
        assert!(seen.lock().is_empty(), "nothing was sent");
    }

    #[tokio::test]
    async fn the_scope_gate_sends_the_collapsed_request_and_routing_rides_in_the_same_call() {
        let (url, seen) = recording_backend("database", Duration::ZERO).await;
        let routing = |mode: &str, state_chars: usize| -> RoutingUse {
            toml::from_str(&format!("mode = \"{mode}\"\nstate_chars = {state_chars}")).unwrap()
        };
        let with = |routing: RoutingUse| {
            let config = DecisionConfig {
                uses: DecisionUses {
                    routing,
                    ..Default::default()
                },
                ..config_for(url.clone())
            };
            build(&config).unwrap().unwrap()
        };
        let log: String = (0..60)
            .map(|i| format!("2026-09-26T10:00:{i:02}Z ERROR billing: charge {i} failed\n"))
            .collect();
        let message = format!("Hi.\n{log}How many charges failed yesterday?");
        let asked = |n: usize| -> Vec<String> {
            let questions = seen.lock()[n]["questions"].clone();
            questions.as_object().unwrap().keys().cloned().collect()
        };

        let (verdict, _) = with(routing("shadow", 1_500))
            .scope_verdict(
                &nobody(),
                &Subject::default(),
                &message,
                "billing questions",
                0.7,
            )
            .await
            .unwrap();
        assert!(verdict.off_topic && (verdict.p_off_topic - 0.9).abs() < 1e-9);
        assert_eq!(seen.lock().len(), 1, "two questions, one request");
        assert_eq!(asked(0), [scope::QUESTION_ID, routing::QUESTION_ID]);
        let batched = seen.lock()[0].clone();
        assert_eq!(
            batched["state"],
            json!({"message": "Hi.\n[60 lines of log]\nHow many charges failed yesterday?"})
        );
        for (id, recorded) in [
            (
                scope::QUESTION_ID,
                json!({"use": "scope_gate", "message": message, "topic": "billing questions", "label": false}),
            ),
            (
                routing::QUESTION_ID,
                json!({"use": "routing", "message": message, "label": "database"}),
            ),
        ] {
            let (state, questions) = exported(recorded, 1_500);
            assert_eq!(state, batched["state"].to_string(), "{id}");
            assert_eq!(
                questions,
                json!({id: batched["questions"][id]}).to_string(),
                "each question in the batch is the one its use is trained on"
            );
        }

        let _ = with(routing("off", 1_500))
            .scope_verdict(
                &nobody(),
                &Subject::default(),
                &message,
                "billing questions",
                0.7,
            )
            .await
            .unwrap();
        assert_eq!(
            asked(1),
            [scope::QUESTION_ID],
            "routing off asks nothing more"
        );

        let long = format!(
            "{}How many charges failed yesterday?",
            "The dashboard showed nothing unusual at the time. ".repeat(60)
        );
        let _ = with(routing("shadow", 1_500))
            .scope_verdict(
                &nobody(),
                &Subject::default(),
                &long,
                "billing questions",
                0.7,
            )
            .await
            .unwrap();
        let apart: Vec<Value> = seen.lock()[2..].to_vec();
        assert_eq!(
            apart.len(),
            2,
            "a message too long for both sets of questions is put to each alone"
        );
        for request in &apart {
            let ids: Vec<&String> = request["questions"].as_object().unwrap().keys().collect();
            let example = match ids[..] {
                [id] if id == scope::QUESTION_ID => {
                    json!({"use": "scope_gate", "message": long, "topic": "billing questions", "label": false})
                }
                [id] if id == routing::QUESTION_ID => {
                    json!({"use": "routing", "message": long, "label": "database"})
                }
                _ => panic!("one question each: {ids:?}"),
            };
            assert_eq!(sent(request), exported(example, 1_500));
        }
    }

    #[tokio::test]
    async fn routing_is_off_until_switched_on_and_an_unsure_class_is_ambiguous() {
        let (url, seen) = recording_backend("database", Duration::ZERO).await;
        let off = build(&config_for(url.clone())).unwrap().unwrap();
        assert_eq!(
            off.route(&nobody(), &Subject::default(), "How many users?")
                .await
                .unwrap(),
            None
        );
        assert!(seen.lock().is_empty());

        let on = |min_probability: f64| {
            let config = DecisionConfig {
                uses: DecisionUses {
                    routing: toml::from_str(&format!(
                        "mode = \"on\"\nmin_probability = {min_probability}"
                    ))
                    .unwrap(),
                    ..Default::default()
                },
                ..config_for(url.clone())
            };
            build(&config).unwrap().unwrap()
        };
        let sure = on(0.7)
            .route(&nobody(), &Subject::default(), "How many users?")
            .await
            .unwrap();
        assert_eq!(sure.unwrap().use_case, routing::UseCase::Database);
        let unsure = on(0.95)
            .route(&nobody(), &Subject::default(), "How many users?")
            .await
            .unwrap();
        assert_eq!(unsure.unwrap().use_case, routing::UseCase::Ambiguous);
        assert_eq!(
            seen.lock()[0]["state"],
            json!({"message": "How many users?"})
        );
    }

    #[tokio::test]
    async fn a_busy_backend_is_never_a_decision_and_costs_each_use_at_most_its_budget() {
        let (url, seen) = recording_backend("approved", Duration::from_secs(3)).await;
        let config = DecisionConfig {
            branch_fallback: BranchFallbackConfig {
                shadow: false,
                limits: limits(100, 1_500),
                ..Default::default()
            },
            uses: DecisionUses {
                decision_steps: UseSwitch::Limited {
                    enabled: true,
                    limits: limits(100, 1_500),
                },
                tool: UseSwitch::Limited {
                    enabled: true,
                    limits: limits(100, 1_500),
                },
                scope_gate: toml::from_str("mode = \"on\"\nbudget_ms = 100").unwrap(),
                routing: toml::from_str("mode = \"on\"\nstate_chars = 800").unwrap(),
                decide_first: toml::from_str("mode = \"on\"\nbudget_ms = 100").unwrap(),
            },
            ..config_for(url)
        };
        let decisions = build(&config).unwrap().unwrap();
        let bounded = Duration::from_secs(1);

        let started = Instant::now();
        let err = decisions
            .scope_verdict(&nobody(), &Subject::default(), "anything", "billing", 0.7)
            .await
            .unwrap_err();
        assert!(started.elapsed() < bounded, "{:?}", started.elapsed());
        assert!(err.is_overloaded() && !err.is_unavailable(), "{err}");
        assert!(explain(&err).starts_with("the decision model is busy and nothing was decided"));

        let started = Instant::now();
        let err = decisions
            .route(&nobody(), &Subject::default(), "anything")
            .await
            .unwrap_err();
        assert!(
            started.elapsed() < bounded,
            "routing keeps its 300 ms default"
        );
        assert!(err.is_overloaded());

        let def = WorkflowDefinition::from_toml(REVIEW).unwrap();
        let mut provenance = StepProvenance::default();
        let started = Instant::now();
        let settled = decisions
            .settle_branch(
                &nobody(),
                &Subject::default(),
                &def,
                "review",
                Produced {
                    response: "Looks good to me.",
                    outputs: None,
                    report: None,
                },
                &mut provenance,
            )
            .await;
        assert!(started.elapsed() < bounded, "{:?}", started.elapsed());
        assert_eq!(settled, None, "the step routes as it would have");
        assert_eq!(provenance.branch_decided_by, None);

        let started = Instant::now();
        let undecided = decisions
            .decision_step(&nobody(), &spec(DecideKind::Choice), "a ticket", &[], &[])
            .await
            .err()
            .expect("no answer");
        assert!(started.elapsed() < bounded, "{:?}", started.elapsed());
        assert!(undecided.busy, "a step asks again of a busy backend");
        assert!(
            undecided
                .reason
                .starts_with("the decision model is busy and nothing was decided"),
            "{undecided:?}"
        );
        assert_eq!(seen.lock().len(), 4);

        let started = Instant::now();
        let err = decisions
            .ask_tool(&nobody(), json!("a ticket"), questions())
            .await
            .unwrap_err();
        assert!(started.elapsed() < bounded, "{:?}", started.elapsed());
        assert!(
            matches!(err, DecideError::Busy { .. }),
            "all four slots are held by requests the backend is still working on: {err}"
        );
        assert_eq!(seen.lock().len(), 4, "a full backend is sent nothing more");
    }

    #[tokio::test]
    async fn a_step_that_cannot_be_answered_for_good_is_not_asked_again() {
        let jev = DecisionConfig {
            enabled: true,
            backends: vec![backend("jev")],
            primary: Some("jev".into()),
            ..Default::default()
        };
        let no_key = build(&jev)
            .unwrap()
            .unwrap()
            .decision_step(&nobody(), &spec(DecideKind::Choice), "a ticket", &[], &[])
            .await
            .err()
            .expect("no answer");
        assert!(
            !no_key.busy && no_key.reason.starts_with("no API key for jev"),
            "{no_key:?}"
        );

        let off = DecisionConfig {
            uses: DecisionUses {
                decision_steps: UseSwitch::Enabled(false),
                ..Default::default()
            },
            ..config_for("http://127.0.0.1:9".into())
        };
        let switched_off = build(&off)
            .unwrap()
            .unwrap()
            .decision_step(&nobody(), &spec(DecideKind::Choice), "a ticket", &[], &[])
            .await
            .err()
            .expect("no answer");
        assert!(!switched_off.busy && switched_off.reason.contains("switched off"));

        let unreachable = build(&config_for("http://127.0.0.1:9".into()))
            .unwrap()
            .unwrap()
            .decision_step(&nobody(), &spec(DecideKind::Choice), "a ticket", &[], &[])
            .await
            .err()
            .expect("no answer");
        assert!(
            !unreachable.busy,
            "a backend that is down is a fault: {unreachable:?}"
        );
    }

    #[test]
    fn every_use_has_a_default_budget_and_config_overrides_it() {
        let stock = build(&config_for("http://kev:8009".into()))
            .unwrap()
            .unwrap();
        let budget_ms = |d: &Decisions, usage| d.limits(usage).budget.as_millis();
        assert_eq!(budget_ms(&stock, Use::Routing), 300);
        assert_eq!(budget_ms(&stock, Use::ScopeGate), 1_500);
        assert_eq!(budget_ms(&stock, Use::BranchFallback), 5_000);
        assert_eq!(budget_ms(&stock, Use::DecisionStep), 10_000);
        assert_eq!(budget_ms(&stock, Use::Tool), 10_000);
        assert_eq!(budget_ms(&stock, Use::DecideFirst), 1_500);
        for usage in [
            Use::Routing,
            Use::ScopeGate,
            Use::BranchFallback,
            Use::DecisionStep,
            Use::Tool,
            Use::DecideFirst,
        ] {
            assert_eq!(stock.request_budget(usage), RequestBudget::DEFAULT);
        }

        let tuned = DecisionConfig {
            branch_fallback: BranchFallbackConfig {
                limits: limits(4_000, 3_000),
                ..Default::default()
            },
            uses: DecisionUses {
                decision_steps: UseSwitch::Limited {
                    enabled: true,
                    limits: limits(20_000, 6_000),
                },
                tool: UseSwitch::Enabled(false),
                decide_first: toml::from_str("mode = \"shadow\"\nbudget_ms = 700").unwrap(),
                ..Default::default()
            },
            ..config_for("http://kev:8009".into())
        };
        let tuned = build(&tuned).unwrap().unwrap();
        assert_eq!(budget_ms(&tuned, Use::DecideFirst), 700);
        assert_eq!(tuned.decide_first_use().mode, DecisionUse::Shadow);
        assert_eq!(budget_ms(&tuned, Use::BranchFallback), 4_000);
        assert_eq!(tuned.request_budget(Use::BranchFallback).get(), 3_000);
        assert_eq!(budget_ms(&tuned, Use::DecisionStep), 20_000);
        assert_eq!(tuned.request_budget(Use::DecisionStep).get(), 6_000);
        assert!(tuned.decision_steps && !tuned.tool_enabled());
        assert_eq!(budget_ms(&tuned, Use::Tool), 10_000);
    }

    #[test]
    fn off_builds_nothing_and_inconsistent_config_refuses() {
        assert!(build(&DecisionConfig::default()).unwrap().is_none());
        let base = DecisionConfig {
            enabled: true,
            backends: vec![backend("kev"), backend("jev")],
            primary: Some("kev".into()),
            ..Default::default()
        };
        let built = build(&base).unwrap().unwrap();
        assert!(built.branch_fallback.is_some() && built.decision_steps && built.tool);
        assert_eq!(built.scope_gate.mode, DecisionUse::Off);
        for broken in [
            DecisionConfig {
                primary: None,
                ..base.clone()
            },
            DecisionConfig {
                primary: Some("nope".into()),
                ..base.clone()
            },
            DecisionConfig {
                fallback: Some("kev".into()),
                ..base.clone()
            },
            DecisionConfig {
                compare: true,
                ..base.clone()
            },
        ] {
            assert!(build(&broken).is_err(), "{broken:?}");
        }
        let no_fallback = DecisionConfig {
            branch_fallback: BranchFallbackConfig {
                enabled: false,
                ..Default::default()
            },
            ..base
        };
        let built = build(&no_fallback).unwrap().unwrap();
        assert!(built.branch_fallback.is_none() && built.decision_steps);
    }

    /// A System One server that always picks `approved`, recording the
    /// bearer each request carried.
    async fn fake_backend() -> (String, Arc<parking_lot::Mutex<Vec<Option<String>>>>) {
        use axum::{http::HeaderMap, routing::post, Json, Router};
        let seen: Arc<parking_lot::Mutex<Vec<Option<String>>>> = Arc::default();
        let record = Arc::clone(&seen);
        let app = Router::new().route(
            "/v1/systemone",
            post(
                move |headers: HeaderMap, Json(body): Json<Value>| async move {
                    record.lock().push(
                        headers
                            .get("authorization")
                            .map(|v| v.to_str().unwrap().to_string()),
                    );
                    let answers: serde_json::Map<String, Value> = body["questions"]
                        .as_object()
                        .unwrap()
                        .keys()
                        .map(|id| (id.clone(), json!({"type": "noul", "noul": 0.9})))
                        .collect();
                    Json(json!({"model": "fake-1", "answers": answers}))
                },
            ),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (url, seen)
    }

    fn questions() -> BTreeMap<String, Question> {
        BTreeMap::from([("q".to_string(), Question::noul("Is it urgent?"))])
    }

    fn vaults() -> (
        tempfile::TempDir,
        Arc<crate::vault::Vault>,
        Arc<crate::vault::Vault>,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let open = |name: &str| {
            Arc::new(crate::vault::Vault::open(&dir.path().join(name), "test-key").unwrap())
        };
        let (agent, global) = (open("a.enc"), open("g.enc"));
        (dir, agent, global)
    }

    #[tokio::test]
    async fn each_request_carries_the_callers_own_key_and_skips_a_backend_they_have_none_for() {
        let (jev_url, jev_seen) = fake_backend().await;
        let (kev_url, kev_seen) = fake_backend().await;
        let config = DecisionConfig {
            enabled: true,
            backends: vec![
                DecisionBackend {
                    url: jev_url,
                    ..backend("jev")
                },
                DecisionBackend {
                    url: kev_url,
                    ..backend("kev")
                },
            ],
            primary: Some("jev".into()),
            fallback: Some("kev".into()),
            ..Default::default()
        };
        let decisions = build(&config).unwrap().unwrap();
        let (_dir, agent, global) = vaults();
        let caller = |person: Option<&str>| {
            Caller::new(
                person.map(str::to_string),
                Some(Arc::clone(&agent)),
                Some(Arc::clone(&global)),
            )
        };
        global
            .set("users/alice/decision/jev/api_key", "alice-key")
            .unwrap();
        agent
            .set("users/bob/decision/kev/api_key", "bob-kev")
            .unwrap();

        let ask = |c: Caller| {
            let decisions = &decisions;
            async move { decisions.ask(&c, Use::Tool, &json!({}), &questions()).await }
        };
        let alice = ask(caller(Some("alice"))).await.unwrap();
        assert_eq!((alice.backend.as_str(), alice.fell_back), ("jev", false));
        let bob = ask(caller(Some("bob"))).await.unwrap();
        assert_eq!((bob.backend.as_str(), bob.fell_back), ("kev", true));
        let system = ask(caller(None)).await.unwrap();
        assert_eq!(system.backend, "kev");

        global.set("decision/jev/api_key", "org-key").unwrap();
        let bob = ask(caller(Some("bob"))).await.unwrap();
        assert_eq!(
            bob.backend, "jev",
            "the shared tier serves a person with none of their own"
        );
        global
            .set("users/alice/decision/jev/api_key", "alice-rotated")
            .unwrap();
        ask(caller(Some("alice"))).await.unwrap();

        assert_eq!(
            *jev_seen.lock(),
            vec![
                Some("Bearer alice-key".to_string()),
                Some("Bearer org-key".to_string()),
                Some("Bearer alice-rotated".to_string()),
            ]
        );
        assert_eq!(
            *kev_seen.lock(),
            vec![None, None],
            "kev's key is shared-only: bob's per-user value is never sent"
        );
    }

    #[tokio::test]
    async fn the_config_key_is_the_last_resort_and_without_any_key_nothing_is_asked() {
        let (url, seen) = fake_backend().await;
        let jev = DecisionBackend {
            url,
            ..backend("jev")
        };
        let config = |api_key: Option<&str>| DecisionConfig {
            enabled: true,
            backends: vec![DecisionBackend {
                api_key: api_key.map(str::to_string),
                ..jev.clone()
            }],
            primary: Some("jev".into()),
            ..Default::default()
        };
        let (_dir, agent, global) = vaults();
        let carol = Caller::new(Some("carol".into()), Some(agent), Some(global));

        let keyless = build(&config(None)).unwrap().unwrap();
        let err = keyless
            .ask(&carol, Use::Tool, &json!({}), &questions())
            .await
            .unwrap_err();
        assert!(err.is_unavailable());
        assert_eq!(
            explain(&err),
            "no API key for jev at any vault level for this user. A person adds their own in \
             My Settings (vault path decision/jev/api_key), or an admin sets one for the agent \
             or the install. Do not ask for the key in chat or try to store it yourself."
        );
        assert!(seen.lock().is_empty());

        let configured = build(&config(Some(" config-key "))).unwrap().unwrap();
        configured
            .ask(&carol, Use::Tool, &json!({}), &questions())
            .await
            .unwrap();
        assert_eq!(*seen.lock(), vec![Some("Bearer config-key".to_string())]);
    }

    #[tokio::test]
    async fn an_install_with_only_a_config_key_and_no_vault_keeps_sending_it() {
        let (url, seen) = fake_backend().await;
        let config = DecisionConfig {
            enabled: true,
            backends: vec![DecisionBackend {
                url,
                api_key: Some("kev-env-key".into()),
                ..backend("kev")
            }],
            primary: Some("kev".into()),
            ..Default::default()
        };
        let decisions = build(&config).unwrap().unwrap();
        let (_dir, agent, global) = vaults();
        for caller in [
            Caller::new(None, None, None),
            Caller::new(Some("alice".into()), Some(agent), Some(global)),
        ] {
            decisions
                .ask(&caller, Use::Tool, &json!({}), &questions())
                .await
                .unwrap();
        }
        assert_eq!(
            *seen.lock(),
            vec![Some("Bearer kev-env-key".to_string()); 2]
        );
    }

    #[test]
    fn a_backend_needs_a_key_by_default_unless_it_is_a_private_service() {
        let required = |name: &str, url: &str, explicit: Option<bool>| {
            let backend = DecisionBackend {
                url: url.into(),
                requires_key: explicit,
                ..backend(name)
            };
            BackendKey::of(&backend, &Url::parse(url).unwrap()).required
        };
        for (name, url, explicit, expected) in [
            ("jev", "https://api.typesafe.ai", None, true),
            ("jev", "http://127.0.0.1:9", None, true),
            ("kev", "http://kev:8009", None, false),
            ("kev", "http://10.20.0.5:8009", None, false),
            ("kev", "http://kev.prod.example.local:8009", None, false),
            ("kev", "https://kev.example.com", None, true),
            ("other", "https://decide.example.com", None, true),
            ("other", "http://other:8009", None, false),
            ("jev", "https://api.typesafe.ai", Some(false), false),
            ("kev", "http://kev:8009", Some(true), true),
        ] {
            assert_eq!(
                required(name, url, explicit),
                expected,
                "{name} at {url}, requires_key = {explicit:?}"
            );
        }
    }

    #[tokio::test]
    async fn an_explicit_requires_key_overrides_the_default_both_ways() {
        let (url, seen) = fake_backend().await;
        let only = |name: &str, requires_key: bool| DecisionConfig {
            enabled: true,
            backends: vec![DecisionBackend {
                url: url.clone(),
                requires_key: Some(requires_key),
                ..backend(name)
            }],
            primary: Some(name.into()),
            ..Default::default()
        };
        let (_dir, agent, global) = vaults();
        let carol = Caller::new(Some("carol".into()), Some(agent), Some(global));

        let closed_kev = build(&only("kev", true)).unwrap().unwrap();
        let err = closed_kev
            .ask(&carol, Use::Tool, &json!({}), &questions())
            .await
            .unwrap_err();
        assert!(err.is_unavailable());
        assert_eq!(
            explain(&err),
            "no API key for kev at any vault level for this user. An admin sets it in the \
             agent's or the global vault at decision/kev/api_key. Do not ask for the key in \
             chat or try to store it yourself."
        );
        assert!(seen.lock().is_empty());

        let open_jev = build(&only("jev", false)).unwrap().unwrap();
        open_jev
            .ask(&carol, Use::Tool, &json!({}), &questions())
            .await
            .unwrap();
        assert_eq!(*seen.lock(), vec![None]);
    }

    #[tokio::test]
    async fn compare_mode_compares_only_when_the_caller_can_reach_both() {
        let (kev_url, _) = fake_backend().await;
        let (jev_url, jev_seen) = fake_backend().await;
        let config = DecisionConfig {
            enabled: true,
            backends: vec![
                DecisionBackend {
                    url: kev_url,
                    ..backend("kev")
                },
                DecisionBackend {
                    url: jev_url,
                    ..backend("jev")
                },
            ],
            primary: Some("kev".into()),
            fallback: Some("jev".into()),
            compare: true,
            ..Default::default()
        };
        let decisions = build(&config).unwrap().unwrap();
        let (_dir, agent, global) = vaults();
        agent
            .set("users/alice/decision/jev/api_key", "alice-key")
            .unwrap();
        let caller = |p: &str| {
            Caller::new(
                Some(p.into()),
                Some(Arc::clone(&agent)),
                Some(Arc::clone(&global)),
            )
        };
        let bob = decisions
            .ask(&caller("bob"), Use::Tool, &json!({}), &questions())
            .await
            .unwrap();
        assert_eq!(bob.backend, "kev");
        let alice = decisions
            .ask(&caller("alice"), Use::Tool, &json!({}), &questions())
            .await
            .unwrap();
        assert_eq!(alice.backend, "kev");
        for _ in 0..400 {
            if !jev_seen.lock().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(
            *jev_seen.lock(),
            vec![Some("Bearer alice-key".to_string())],
            "only the caller with a key for it is compared, with their key"
        );
    }

    #[test]
    fn a_configured_key_never_shows_in_debug_output() {
        let jev = DecisionBackend {
            api_key: Some("sk-very-secret".into()),
            ..backend("jev")
        };
        let config = DecisionConfig {
            backends: vec![jev],
            ..Default::default()
        };
        let printed = format!("{config:?}");
        assert!(!printed.contains("sk-very-secret"), "{printed}");
        assert!(printed.contains("<redacted>"));
    }

    fn spec(kind: DecideKind) -> DecideSpec {
        DecideSpec {
            question: "Which?".into(),
            kind,
            options: BTreeMap::new(),
            levels: vec!["low".into(), "normal".into(), "high".into()],
            min_probability: None,
        }
    }

    #[test]
    fn a_choice_answer_routes_only_on_a_declared_label_above_the_floor() {
        let branches = vec!["bug".to_string(), "feature".to_string()];
        let answer = Answer::Choice {
            choice: "bug".into(),
            probabilities: BTreeMap::from([("bug".into(), 0.9), ("feature".into(), 0.1)]),
            confidence: 0.8,
        };
        let out = render_answer(
            &spec(DecideKind::Choice),
            &answer,
            &branches,
            0.7,
            "kev",
            "kev-1",
        );
        assert_eq!(out["outcome"], "bug");
        assert_eq!(out["label"], "bug");
        let text = crate::workflow_runtime::render_tool_step_response(&out, &["label".into()]);
        assert_eq!(parse_outcome(&text).as_deref(), Some("bug"));
        assert!(text.contains("OUTPUTS:\nlabel: bug"));

        let low = render_answer(
            &spec(DecideKind::Choice),
            &answer,
            &branches,
            0.95,
            "kev",
            "kev-1",
        );
        assert!(
            low.get("outcome").is_none(),
            "below the floor nothing routes"
        );
        let undeclared = render_answer(
            &spec(DecideKind::Choice),
            &answer,
            &["feature".into()],
            0.7,
            "kev",
            "kev-1",
        );
        assert!(undeclared.get("outcome").is_none());
    }

    #[test]
    fn yes_no_and_score_answers_name_their_outcome() {
        let yes = render_answer(
            &spec(DecideKind::Noul),
            &Answer::Noul { noul: 0.93 },
            &["yes".into(), "no".into()],
            0.7,
            "kev",
            "m",
        );
        assert_eq!(yes["outcome"], "yes");
        let no = render_answer(
            &spec(DecideKind::Noul),
            &Answer::Noul { noul: 0.12 },
            &["yes".into(), "no".into()],
            0.7,
            "kev",
            "m",
        );
        assert_eq!(no["outcome"], "no");
        assert!((no["probability"].as_f64().unwrap() - 0.88).abs() < 1e-9);

        let score = Answer::Score {
            score: 1.9,
            legend: BTreeMap::new(),
            probabilities: BTreeMap::from([
                ("0".into(), 0.05),
                ("1".into(), 0.15),
                ("2".into(), 0.8),
            ]),
            confidence: 0.7,
        };
        let out = render_answer(
            &spec(DecideKind::Score),
            &score,
            &["high".into()],
            0.7,
            "kev",
            "m",
        );
        assert_eq!(out["level"], "high");
        assert_eq!(out["outcome"], "high");
        assert_eq!(out["probabilities"]["high"], 0.8);
    }

    #[test]
    fn a_yes_no_question_carries_its_criteria_when_described() {
        let bare = build_question(&spec(DecideKind::Noul), &[]);
        assert!(matches!(bare, Question::Noul { criteria: None, .. }));
        let described = DecideSpec {
            options: BTreeMap::from([
                ("yes".to_string(), "blocked now".to_string()),
                ("no".to_string(), "can wait".to_string()),
            ]),
            ..spec(DecideKind::Noul)
        };
        match build_question(&described, &[]) {
            Question::Noul {
                criteria: Some(c), ..
            } => {
                assert_eq!(c.yes, Some(json!("blocked now")));
                assert_eq!(c.no, Some(json!("can wait")));
            }
            other => panic!("{other:?}"),
        }
    }
}
