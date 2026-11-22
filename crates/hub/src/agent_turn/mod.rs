//! An agent's turn across its routes.
//!
//! A turn's model spec and the agent's failover list resolve to one chain of
//! entries, which [`TurnChain`] groups by provider kind: every `claude-code`
//! or `codex` entry is a route of its own, run by its CLI as one whole turn,
//! and every run of consecutive runner entries is one runner route, which
//! fails over inside itself as any chain does. A route hands the turn on only
//! before it has produced output, so nothing it did is done twice. A chain
//! with no CLI route is the agent's provider exactly as it always was.

mod cli;
pub mod connect;
mod conversation;
mod relay;
mod round;
#[cfg(test)]
pub(crate) mod tests;
pub(crate) mod tool_loop;

pub use cli::AgentTurnRig;
#[cfg(test)]
pub(crate) use cli::SeatWait;
pub use round::TOOL_SEARCH;
pub use tool_loop::{ProviderLoop, ToolLoop, UsageFn};

use std::sync::Arc;
use std::time::Duration;

use eyre::Result;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use workforce_agent::llm::failover::{ChainSlice, ChainSpec, SUBSCRIPTION_UNAVAILABLE_MARKER};
use workforce_agent::llm::provider::{LlmProvider, ProviderKind};
use workforce_agent::llm::types::{ContentBlock, LlmMessage, TokenUsage};
use workforce_agent::runner::{
    AgentRunner, InjectedMessage, ProgressEvent, RunResult, RunnerConfig, Seeds,
};
use workforce_common::session::{EngineSlot, Session};
use workforce_common::subscription::{Payer, SubscriptionKind, SubscriptionRoute};

use crate::mcp_gate::RouteResume;
use crate::serve::{AgentService, SharedReplay};
use crate::subscription::CliKind;
use crate::turn_loop::tools::TurnTools;
use cli::{CliAttempt, CliTurn};
use connect::Connect;
use conversation::Conversation;
use relay::Relay;

/// The longest an agent turn on a CLI route runs: long enough for a
/// 20-minute `coding.session` round and a full approval wait inside it.
pub const AGENT_TURN_TIMEOUT: Duration = Duration::from_secs(60 * 60);
/// What a turn keeps of [`AGENT_TURN_TIMEOUT`] to answer in after its last
/// tool call gives up.
pub const ANSWER_RESERVE: Duration = Duration::from_secs(3 * 60);

/// The model id `model` names on `cli`: Codex resolves its own short names.
fn model_id(cli: CliKind, model: &str, service: &AgentService) -> String {
    match cli {
        CliKind::ClaudeCode => model.to_string(),
        CliKind::Codex => service.subscriptions().codex().resolve_model(model),
    }
}

/// The model the turn's hub tools see `cli` run on: Claude Code's aliases
/// resolved to the model that views images for it.
fn tools_model(cli: CliKind, model: &str, service: &AgentService) -> String {
    match cli {
        CliKind::ClaudeCode => crate::coding_engine::claude_vision_model(Some(model), service),
        CliKind::Codex => model.to_string(),
    }
}

/// `model` as a spec routed to `cli`.
fn spec(cli: CliKind, model: &str) -> String {
    format!("{}:{model}", cli.route().label())
}

/// One route of a turn: a run of runner entries the agent's provider chain
/// walks, or one CLI entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnRoute {
    Runner(Vec<String>),
    /// One CLI entry. A person's own route (`personal`) runs on their own
    /// logins only, never one an agent or the hub shares.
    Cli {
        cli: CliKind,
        model: String,
        personal: bool,
    },
}

impl TurnRoute {
    /// How the route is named when it could not serve.
    fn name(&self) -> String {
        match self {
            Self::Runner(raws) => raws.join(" → "),
            Self::Cli { cli, model, .. } => spec(*cli, model),
        }
    }

    /// The route did not serve, for `reason`.
    fn miss(&self, reason: impl Into<String>) -> RouteMiss {
        RouteMiss::new(self.name(), reason)
    }
}

/// The least an agent turn's scope stays up once its turn is over, whatever
/// `agent_turn_idle_secs` says: a quick reply finds it warm.
const AGENT_TURN_IDLE_FLOOR_SECS: u64 = 60;

/// How long an agent turn's scope stays up once its turn is over: as
/// configured, but never under [`AGENT_TURN_IDLE_FLOOR_SECS`], which is said.
fn agent_turn_idle_secs(agent: &str, configured: u64) -> u64 {
    if configured < AGENT_TURN_IDLE_FLOOR_SECS {
        tracing::warn!(
            agent,
            configured,
            floor = AGENT_TURN_IDLE_FLOOR_SECS,
            "agent_turn_idle_secs is under its floor; the floor applies"
        );
    }
    configured.max(AGENT_TURN_IDLE_FLOOR_SECS)
}

/// Wire what runs `service`'s turns on CLI routes: workspaces from the
/// coding executor, transcripts rotated as `/cc`'s are. Wired whatever
/// logins exist today, since a turn resolves its payer's login itself.
pub fn wire(
    service: &AgentService,
    roots: crate::projects::layout::WorkRoots,
    coding: &workforce_common::settings::CodingSessionConfig,
    exec: &workforce_common::settings::ExecConfig,
    codex_worker: Option<Arc<dyn crate::coding_sandbox::SandboxLauncher>>,
) {
    match crate::coding_sandbox::hub_launcher(
        roots,
        coding,
        exec,
        crate::coding_sandbox::McpSurface {
            exposed_tools: None,
        },
    ) {
        Ok(launcher) => service.turn_rig.set(Arc::new(AgentTurnRig::new(
            launcher,
            codex_worker,
            crate::cli_session::RotationPolicy::of(&service.config.claude_chat),
            std::time::Duration::from_secs(coding.executor.scope_ttl_secs),
            std::time::Duration::from_secs(agent_turn_idle_secs(
                service.agent_id.as_str(),
                coding.executor.agent_turn_idle_secs,
            )),
        ))),
        Err(e) => tracing::error!(
            agent = %service.agent_id,
            error = %e,
            "agent turns on CLI routes not wired: the coding executor cannot be built"
        ),
    }
}

/// `work` on the person's own logins only when `personal`.
pub(crate) async fn personal_if<F: std::future::Future>(personal: bool, work: F) -> F::Output {
    match personal {
        true => crate::subscription::personal_logins_only(work).await,
        false => work.await,
    }
}

/// What runs a CLI `route` on this hub, else the route's miss.
/// A person's own Codex route on a hub without its coding worker misses
/// with the reason they read, rather than the CLI's own.
fn rig_or_miss(service: &AgentService, route: &TurnRoute) -> Result<Arc<AgentTurnRig>, RouteMiss> {
    let rig = service
        .turn_rig
        .get::<AgentTurnRig>()
        .ok_or_else(|| route.miss("CLI turns are not wired on this hub"))?;
    match route {
        TurnRoute::Cli {
            cli,
            personal: true,
            ..
        } if !seats_personal(&rig, *cli) => {
            Err(route.miss(crate::personal_route::CODEX_NEEDS_WORKER))
        }
        _ => Ok(rig),
    }
}

/// Whether `rig` seats a person's own route on `cli`: Codex only on the
/// hub's coding worker.
fn seats_personal(rig: &AgentTurnRig, cli: CliKind) -> bool {
    cli != CliKind::Codex || rig.has_codex_worker()
}

/// Why a person's own route on `cli` can't take `service`'s turns, in the
/// words a turn misses with; `None` when it can. A hub whose CLI turns
/// aren't wired is left to the turn to say.
pub(crate) fn personal_refusal(service: &AgentService, cli: CliKind) -> Option<&'static str> {
    let rig = service.turn_rig.get::<AgentTurnRig>()?;
    (!seats_personal(&rig, cli)).then_some(crate::personal_route::CODEX_NEEDS_WORKER)
}

/// A turn's routes, in the order the chain is configured.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnChain {
    routes: Vec<TurnRoute>,
    /// The provider kinds each route's entries run on, in route order.
    route_kinds: Vec<Vec<ProviderKind>>,
    /// The provider kinds the chain's entries run on, each once, in order.
    kinds: Vec<ProviderKind>,
}

impl TurnChain {
    /// Group a resolved chain into routes, each entry's kind taken from its
    /// provider prefix, else the agent's `primary`.
    pub fn group(entries: Vec<ChainSpec>, primary: ProviderKind) -> Self {
        let mut routes = Vec::new();
        let mut route_kinds: Vec<Vec<ProviderKind>> = Vec::new();
        let mut kinds = Vec::new();
        for spec in entries {
            let kind = spec
                .provider_key
                .as_deref()
                .and_then(ProviderKind::from_str_loose)
                .unwrap_or(primary);
            if !kinds.contains(&kind) {
                kinds.push(kind);
            }
            match (CliKind::of(kind), routes.last_mut()) {
                (Some(cli), _) => {
                    routes.push(TurnRoute::Cli {
                        cli,
                        model: spec.model,
                        personal: false,
                    });
                    route_kinds.push(vec![kind]);
                }
                (None, Some(TurnRoute::Runner(raws))) => {
                    raws.push(spec.raw);
                    if let Some(last) = route_kinds.last_mut() {
                        last.push(kind);
                    }
                }
                (None, _) => {
                    routes.push(TurnRoute::Runner(vec![spec.raw]));
                    route_kinds.push(vec![kind]);
                }
            }
        }
        Self {
            routes,
            route_kinds,
            kinds,
        }
    }

    /// Why the routes on subscription `kind` did not serve, person-facing:
    /// the reasons of the misses in route order, one per route that ran on
    /// it, without the chain's own marker. Every route has missed by the
    /// time a turn asks, so the misses line up with the routes.
    fn unserved_on(&self, kind: SubscriptionKind, misses: &[RouteMiss]) -> Vec<String> {
        self.route_kinds
            .iter()
            .zip(misses)
            .filter(|(kinds, _)| {
                kinds
                    .iter()
                    .any(|k| k.subscription_route().map(SubscriptionRoute::kind) == Some(kind))
            })
            .map(|(_, miss)| person_facing(&miss.reason))
            .collect()
    }

    #[cfg(test)]
    pub fn routes(&self) -> &[TurnRoute] {
        &self.routes
    }

    /// The provider kinds the chain's entries run on, each once, in order.
    pub fn kinds(&self) -> &[ProviderKind] {
        &self.kinds
    }

    /// Whether every route runs on the agent's provider, so the turn runs
    /// exactly as it would with no CLI route anywhere.
    pub fn runner_only(&self) -> bool {
        self.routes
            .iter()
            .all(|route| matches!(route, TurnRoute::Runner(_)))
    }

    /// This chain behind a person's own route on `cli` and `model`: the
    /// route alone when their fallback is `wait`, so a call it can't serve
    /// stops and says why; ahead of the chain's own routes when it is
    /// `hub`, which take the call only before the person's route has
    /// produced anything.
    pub fn on_personal_route(
        self,
        cli: CliKind,
        model: String,
        fallback: workforce_common::personal_route::RouteFallback,
    ) -> Self {
        let kind = crate::personal_route::provider_of(cli);
        let mut chain = Self {
            routes: vec![TurnRoute::Cli {
                cli,
                model,
                personal: true,
            }],
            route_kinds: vec![vec![kind]],
            kinds: vec![kind],
        };
        if fallback == workforce_common::personal_route::RouteFallback::Hub {
            chain.routes.extend(self.routes);
            chain.route_kinds.extend(self.route_kinds);
            for kind in self.kinds {
                if !chain.kinds.contains(&kind) {
                    chain.kinds.push(kind);
                }
            }
        }
        chain
    }

    /// Whether every route is a person's own, so whatever the turn draws on
    /// must be their own logins.
    pub fn personal_only(&self) -> bool {
        self.routes
            .iter()
            .all(|route| matches!(route, TurnRoute::Cli { personal: true, .. }))
    }

    /// The model a turn's attachments are prepared for: the first route's.
    /// A CLI route reads images natively.
    pub fn media_model(&self, resolved: &str) -> String {
        match self.routes.first() {
            Some(TurnRoute::Cli { cli, model, .. }) => spec(*cli, model),
            _ => resolved.to_string(),
        }
    }
}

/// Who can answer a call of the turn that needs a person's approval.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Oversight {
    /// The person the turn runs for, on the card of the turn in scope, or
    /// the approvers a rule routes the call to, the call resuming where
    /// this says.
    Attended(RouteResume),
    /// Nobody: such a call is refused.
    Unattended,
}

/// Whether a turn continues a transcript kept for its session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Continuity {
    Session,
    /// Every turn starts afresh: a shared room, whose transcript would carry
    /// other speakers' tool results, or an isolated run.
    OneShot,
}

impl Continuity {
    pub fn of(session: &Session) -> Self {
        match session.is_shared_conversation() {
            true => Self::OneShot,
            false => Self::Session,
        }
    }
}

/// Everything one turn runs with.
pub(crate) struct TurnRun<'a> {
    pub config: RunnerConfig,
    pub tools: TurnTools,
    pub task: Vec<ContentBlock>,
    pub prior: Vec<LlmMessage>,
    pub session: &'a Session,
    pub replay: Option<&'a SharedReplay>,
    pub progress: Option<mpsc::Sender<ProgressEvent>>,
    pub cancel: Option<CancellationToken>,
    pub inject: Option<mpsc::Receiver<InjectedMessage>>,
    /// Who the turn's tool calls run as.
    pub principal: &'a str,
    pub oversight: Oversight,
    pub continuity: Continuity,
}

/// How a turn ended.
pub enum ChainOutcome {
    Served {
        result: Box<RunResult>,
        route: RouteTag,
    },
    /// No route could serve, and the person the turn is for is told how to
    /// make one: saved as the turn's answer, with nothing to bill. When
    /// the turn `resumes` once they do, it keeps the `guard` it paused
    /// under.
    Notice {
        text: String,
        resumes: bool,
        guard: Option<Box<workforce_agent::security::monitor::ContentScope>>,
    },
}

/// The route that served a turn.
pub enum RouteTag {
    Runner,
    Cli {
        cli: CliKind,
        /// The transcript the turn continued, kept once its answer is saved.
        slot: Option<EngineSlot>,
    },
}

impl RouteTag {
    /// The provider task telemetry files a CLI-served turn under; `None`
    /// for a runner route, filed as it always was.
    pub fn label(&self) -> Option<&'static str> {
        match self {
            Self::Runner => None,
            Self::Cli { cli, .. } => Some(cli.route().label()),
        }
    }

    pub fn is_cli(&self) -> bool {
        matches!(self, Self::Cli { .. })
    }

    /// Keep the transcript a CLI turn continued, as seen through the
    /// session's last message: the answer, now saved.
    pub fn settle(self, session: &mut Session) {
        if let Self::Cli {
            cli,
            slot: Some(slot),
        } = self
        {
            let synced_through = session.messages.last().map(|m| m.id.clone());
            session.set_engine_slot(
                cli.slot_key(),
                &EngineSlot {
                    synced_through,
                    ..slot
                },
            );
        }
    }
}

/// Why one route of a turn did not serve it.
#[derive(Debug)]
pub struct RouteMiss {
    pub route: String,
    pub reason: String,
    /// The CLI route that missed, and whether it was the person's own: whose
    /// plan a person is told could not take the turn.
    pub plan: Option<(CliKind, bool)>,
    /// The coding worker had no workspace for the turn: every scope on it
    /// was busy for as long as the turn could wait.
    pub worker_full: bool,
}

impl RouteMiss {
    fn new(route: String, reason: impl Into<String>) -> Self {
        Self {
            route,
            reason: reason.into(),
            plan: None,
            worker_full: false,
        }
    }

    /// This miss, for want of a workspace on a full coding worker.
    fn worker_full(self) -> Self {
        Self {
            worker_full: true,
            ..self
        }
    }

    /// This miss, of the CLI route on `cli`, the person's own when
    /// `personal`.
    fn on_plan(self, cli: CliKind, personal: bool) -> Self {
        Self {
            plan: Some((cli, personal)),
            ..self
        }
    }
}

/// The plan a CLI's turns run on, as a person names it.
fn plan_name(cli: CliKind) -> &'static str {
    match cli {
        CliKind::ClaudeCode => "Claude",
        CliKind::Codex => "ChatGPT",
    }
}

/// Whose plan `plan` is, as the person the turn is for reads it.
fn whose(plan: (CliKind, bool)) -> String {
    match plan {
        (cli, true) => format!("Your {} plan", plan_name(cli)),
        (cli, false) => format!("This agent's {} plan", plan_name(cli)),
    }
}

/// What the person hears when no route served because the coding worker was
/// full: why, and what to do. A person whose route waits rather than fall
/// back is told they can let it. Only for a person in the loop
/// (`interactive`): a job or trigger fails with its misses instead, so it is
/// retried or alerted on. And only when the worker is all that stood in the
/// way, every other miss a login one: a route that failed for some other
/// reason is not hidden behind a full worker.
fn worker_full_notice(
    misses: &[RouteMiss],
    personal_only: bool,
    interactive: bool,
) -> Option<String> {
    if !interactive
        || !misses
            .iter()
            .all(|miss| miss.worker_full || names_a_login(&miss.reason))
    {
        return None;
    }
    let plan = misses.iter().find(|miss| miss.worker_full)?.plan?;
    let advice = match personal_only {
        true => "Try again in a minute, or switch this agent to the hub's models.",
        false => "Try again in a minute.",
    };
    Some(format!(
        "{} couldn't take this right now: the coding worker is full. {advice}",
        whose(plan)
    ))
}

/// The line a served turn ends with when the person's own route could not
/// take it for want of a workspace on a full coding worker and another
/// route did (`by`, as the person reads it): brief, and only what they need
/// to know. Every other hand-over (no login of hers, her plan at its limit,
/// the agent's own failover) stays as quiet as it always was, so a standing
/// setup never closes each answer with the same line. Only a person in the
/// loop (`interactive`) is told; a job's answer is its own.
fn fallback_note(misses: &[RouteMiss], by: &str, interactive: bool) -> Option<String> {
    if !interactive {
        return None;
    }
    let miss = misses
        .iter()
        .find(|miss| miss.worker_full && miss.plan.is_some_and(|(_, personal)| personal))?;
    let plan = whose(miss.plan?);
    Some(format!(
        "\n\n_{plan} couldn't take this right now (the coding worker was full), so {by} answered._"
    ))
}

/// Stream `note` to whoever watches the turn as the last part of its answer,
/// before the answer is given whole, so what is on screen and what is kept
/// are the same words and nothing follows the turn's `Done`.
async fn stream_note(note: &str, progress: &mpsc::Sender<ProgressEvent>) {
    for event in [
        ProgressEvent::StreamStart,
        ProgressEvent::StreamDelta {
            text: note.to_string(),
        },
        ProgressEvent::StreamEnd,
    ] {
        let _ = progress.send(event).await;
    }
}

/// A progress channel for a runner route whose answer closes with `note`:
/// everything the runner says passes through, its whole answer with the
/// note streamed just before and appended, so the note lands before `Done`.
/// Await the handle once the runner is over, before anything else is said.
fn noting(
    note: Option<&str>,
    progress: Option<&mpsc::Sender<ProgressEvent>>,
) -> (
    Option<mpsc::Sender<ProgressEvent>>,
    Option<tokio::task::JoinHandle<()>>,
) {
    let (Some(note), Some(progress)) = (note, progress) else {
        return (progress.cloned(), None);
    };
    let (tx, mut rx) = mpsc::channel(64);
    let (note, progress) = (note.to_string(), progress.clone());
    let forward = tokio::spawn(async move {
        while let Some(event) = rx.recv().await {
            let event = match event {
                ProgressEvent::Response { text } => {
                    stream_note(&note, &progress).await;
                    ProgressEvent::Response {
                        text: format!("{text}{note}"),
                    }
                }
                event => event,
            };
            let _ = progress.send(event).await;
        }
    });
    (Some(tx), Some(forward))
}

/// No route served, and `text` says why: the turn's answer, given to
/// whoever watches it as a finished one, so every surface replaces its
/// status line with it. A bare `Response` is enough: the notice comes only
/// after every route missed before any output (a miss after output is an
/// error, not a miss), so no surface has a streamed answer to keep.
async fn notice(text: String, progress: Option<&mpsc::Sender<ProgressEvent>>) -> ChainOutcome {
    if let Some(progress) = progress {
        for event in [
            ProgressEvent::Response { text: text.clone() },
            done_of(&TokenUsage::default(), 0, 0),
        ] {
            let _ = progress.send(event).await;
        }
    }
    ChainOutcome::Notice {
        text,
        resumes: false,
        guard: None,
    }
}

/// No route of a turn could serve it; each says why.
#[derive(Debug, thiserror::Error)]
#[error("no route could serve this turn: {}", describe(.misses))]
pub struct TurnUnavailable {
    pub misses: Vec<RouteMiss>,
}

fn describe(misses: &[RouteMiss]) -> String {
    misses
        .iter()
        .map(|miss| format!("{} — {}", miss.route, miss.reason))
        .collect::<Vec<_>>()
        .join("; ")
}

impl TurnChain {
    /// Run a turn on the chain's routes in order, until one serves it, at
    /// the agent's `[llm] effort` and `[llm] fast` as its live config says
    /// them now.
    pub(crate) async fn run(
        &self,
        service: &AgentService,
        turn: TurnRun<'_>,
    ) -> Result<ChainOutcome> {
        let (effort, fast) = {
            let resolved = service.resolved.snapshot();
            (resolved.effort_level(), resolved.fast)
        };
        let kept = crate::monitor::scope::kept_guard(turn.session);
        let keeper = crate::monitor::scope::TurnKeeper::new(
            Arc::clone(&service.sessions),
            turn.session.id.clone(),
        );
        // Dropped unfinished (stopped, timed out, its client gone), the
        // turn's kept guard is cleared; cut off by a shutdown it is left for
        // the turn a restart resumes.
        let dropped = keeper.on_drop(crate::shutdown::in_progress);
        let keeps = keeper.keeper();
        // Every route runs under the turn's guard, made and armed from what
        // started the turn as a runner's turn is: a CLI engine's round is
        // guarded as the runner's loop is. A guard kept for the session (a
        // turn resumed) or the caller's is the one it arms.
        let started = turn
            .prior
            .iter()
            .cloned()
            .chain(std::iter::once(workforce_agent::llm::types::LlmMessage {
                role: workforce_agent::llm::types::Role::User,
                content: turn.task.clone(),
            }))
            .collect::<Vec<_>>();
        let guard = workforce_agent::runner::with_content_guard(
            kept,
            workforce_agent::runner::with_guard_keeper(Some(Arc::clone(&keeps)), async {
                workforce_agent::runner::guard_for_turn(&started, &service.tools)
            }),
        )
        .await;
        let routes = Box::pin(workforce_agent::runner::with_guard_keeper(
            Some(keeps),
            workforce_agent::runner::with_content_guard(
                Some(Arc::clone(&guard)),
                self.run_routes(service, turn),
            ),
        ));
        // A person's own route alone: what the turn finds missing or tells
        // them to connect is their own login, never a shared one.
        let routes: std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<ChainOutcome>> + Send + '_>,
        > = match self.personal_only() {
            true => Box::pin(crate::subscription::personal_logins_only(routes)),
            false => routes,
        };
        let outcome = workforce_agent::llm::provider::with_effort_override(
            effort,
            workforce_agent::llm::provider::with_fast_mode(fast, routes),
        )
        .await;
        // The turn's end is what it keeps: no write of its keeper lands
        // after it.
        keeper.finish().await;
        dropped.disarm();
        // A notice the turn resumes from once the person connects keeps
        // the guard it paused under.
        outcome.map(|outcome| match outcome {
            ChainOutcome::Notice {
                text,
                resumes: true,
                ..
            } => ChainOutcome::Notice {
                text,
                resumes: true,
                guard: guard.armed_scope(),
            },
            outcome => outcome,
        })
    }

    async fn run_routes(&self, service: &AgentService, turn: TurnRun<'_>) -> Result<ChainOutcome> {
        if self.runner_only() {
            let interactive = turn.config.interactive;
            let cancelled = turn.cancel.clone();
            let progress = turn.progress.clone();
            let runner = crate::turn_loop::runner_on(
                service,
                Arc::clone(&service.llm),
                turn.config,
                &turn.tools,
            );
            return match Box::pin(run_runner(
                &runner,
                turn.task,
                turn.prior,
                turn.progress,
                turn.cancel,
                turn.inject,
            ))
            .await
            {
                Ok(result) => Ok(ChainOutcome::Served {
                    result: Box::new(result),
                    route: RouteTag::Runner,
                }),
                Err(e)
                    if cancelled.is_some_and(|cancel| cancel.is_cancelled())
                        || !subscription_unavailable(&e) =>
                {
                    Err(e)
                }
                Err(e) => match self.connect(service, turn.session, interactive, &[]).await {
                    Some(connect) => Ok(connect.notice(progress.as_ref()).await),
                    None => Err(e),
                },
            };
        }
        let TurnRun {
            config,
            tools,
            task,
            prior,
            session,
            replay,
            progress,
            cancel,
            inject,
            principal,
            oversight,
            continuity,
        } = turn;
        let interactive = config.interactive;
        let cancel = cancel.unwrap_or_default();
        let seeds =
            crate::turn_loop::runner_on(service, Arc::clone(&service.llm), config.clone(), &tools)
                .seed(progress.as_ref(), Some(&cancel))
                .await;
        let relay = inject.map(Relay::start);
        let conversation = Conversation {
            session,
            replay,
            continuity,
        };
        let mut misses = Vec::new();
        let last = self.routes.len().saturating_sub(1);
        for (at, route) in self.routes.iter().enumerate() {
            if cancel.is_cancelled() {
                eyre::bail!("turn cancelled");
            }
            let inbox = match &relay {
                Some(relay) => Some(relay.next_route().await),
                None => None,
            };
            match route {
                TurnRoute::Runner(raws) => {
                    let (provider, slice) = route_provider(service, raws);
                    let runner = crate::turn_loop::runner_on(
                        service,
                        provider,
                        RunnerConfig {
                            model: raws.first().cloned(),
                            seeds: Some(seeds.clone()),
                            preexecute: None,
                            collect: Vec::new(),
                            ..config.clone()
                        },
                        &tools,
                    );
                    // A silent model is tried again only when no route
                    // follows to take the turn instead.
                    let runner = match at == last {
                        true => runner,
                        false => runner.without_silent_retry(),
                    };
                    let note = fallback_note(&misses, "the hub's models", interactive);
                    let (watched, forward) = noting(note.as_deref(), progress.as_ref());
                    let ran = Box::pin(run_runner(
                        &runner,
                        task.clone(),
                        prior.clone(),
                        watched,
                        Some(cancel.clone()),
                        inbox,
                    ))
                    .await;
                    // Every sender the runner was given is dropped once it
                    // returns (tools read the turn's progress sender inline,
                    // never keeping it), so the forwarder ends here.
                    if let Some(forward) = forward {
                        let _ = forward.await;
                    }
                    match ran {
                        Ok(mut result) => {
                            if let Some(note) = &note {
                                result.response.push_str(note);
                            }
                            return Ok(ChainOutcome::Served {
                                result: Box::new(result),
                                route: RouteTag::Runner,
                            });
                        }
                        Err(e) if !answered(slice.as_deref()) && !cancel.is_cancelled() => {
                            tracing::warn!(agent = %service.agent_id, route = %route.name(), error = %e, "turn route unserved; trying the next");
                            misses.push(route.miss(e.to_string()));
                        }
                        Err(e) => return Err(e),
                    }
                }
                TurnRoute::Cli {
                    cli,
                    model,
                    personal,
                } => {
                    let rig = match rig_or_miss(service, route) {
                        Ok(rig) => rig,
                        Err(miss) => {
                            misses.push(miss.on_plan(*cli, *personal));
                            continue;
                        }
                    };
                    let seat_wait = rig.seat_wait().of(at == last);
                    let mut inbox = inbox;
                    let mut tries = 0;
                    loop {
                        tries += 1;
                        let attempt = Box::pin(personal_if(
                            *personal,
                            rig.run(
                                service,
                                CliTurn {
                                    cli: *cli,
                                    model: model.clone(),
                                    system_prompt: config.system_prompt.clone(),
                                    task: with_seeds(&task, &seeds),
                                    tools: tools.clone(),
                                    max_turns: config.max_rounds,
                                    principal: principal.to_string(),
                                    oversight,
                                    self_serve: config.self_serve,
                                    journal: config.journal.clone(),
                                    progress: progress.clone(),
                                    cancel: cancel.clone(),
                                    timeout: AGENT_TURN_TIMEOUT,
                                    seat_wait,
                                },
                                Some(&conversation),
                                inbox.take(),
                            ),
                        ))
                        .await?;
                        match attempt {
                            CliAttempt::Served(served) => {
                                let note = fallback_note(
                                    &misses,
                                    &match personal {
                                        true => format!("your {} plan", plan_name(*cli)),
                                        false => format!("this agent's {} plan", plan_name(*cli)),
                                    },
                                    interactive,
                                )
                                .unwrap_or_default();
                                if let Some(progress) = &progress {
                                    if !note.is_empty() {
                                        stream_note(&note, progress).await;
                                    }
                                    let _ = progress
                                        .send(ProgressEvent::Response {
                                            text: format!("{}{note}", served.text),
                                        })
                                        .await;
                                    let _ = progress
                                        .send(done_of(
                                            &served.usage,
                                            served.rounds,
                                            seeds.invocations.len() + served.invocations.len(),
                                        ))
                                        .await;
                                }
                                let slot = served.slot.clone();
                                let mut result = served.into_result(seeds.invocations.clone());
                                result.response.push_str(&note);
                                return Ok(ChainOutcome::Served {
                                    result: Box::new(result),
                                    route: RouteTag::Cli { cli: *cli, slot },
                                });
                            }
                            // Its login at its limit or turned away: skipped from
                            // now, so the route runs again on the payer's next
                            // login of the same plan, when there is one.
                            CliAttempt::Unserved(miss)
                                if tries < MAX_LOGINS_PER_ROUTE
                                    && names_a_login(&miss.reason)
                                    && personal_if(*personal, async {
                                        service.logins.resolve(cli.login()).is_ok()
                                    })
                                    .await =>
                            {
                                tracing::warn!(agent = %service.agent_id, route = %miss.route, reason = %miss.reason, "turn route's login can't serve; trying the next login");
                                inbox = match &relay {
                                    Some(relay) => Some(relay.next_route().await),
                                    None => None,
                                };
                            }
                            CliAttempt::Unserved(miss) => {
                                tracing::warn!(agent = %service.agent_id, route = %miss.route, reason = %miss.reason, worker_full = miss.worker_full, "turn route unserved; trying the next");
                                misses.push(miss.on_plan(*cli, *personal));
                                break;
                            }
                        }
                    }
                }
            }
        }
        if cancel.is_cancelled() {
            eyre::bail!("turn cancelled");
        }
        let connect = match misses.iter().all(|miss| names_a_login(&miss.reason)) {
            true => {
                self.connect(service, conversation.session, interactive, &misses)
                    .await
            }
            false => None,
        };
        if let Some(connect) = connect {
            return Ok(connect.notice(progress.as_ref()).await);
        }
        match worker_full_notice(&misses, self.personal_only(), interactive) {
            Some(text) => {
                tracing::warn!(agent = %service.agent_id, misses = %describe(&misses), "no route could serve the turn: the coding worker is full; the person is told");
                Ok(notice(text, progress.as_ref()).await)
            }
            None => Err(TurnUnavailable { misses }.into()),
        }
    }

    /// How the person an interactive turn is for can make it serve, when a
    /// subscription the chain runs on has no login of theirs: the first such
    /// subscription in chain order. A ChatGPT sign-in is started (or
    /// joined) for them, the turn waiting on it. The notice first says why
    /// each plan they do have a login for did not serve (`misses`, one per
    /// route in chain order), so nobody is told to connect a second plan
    /// without hearing what happened to the first.
    async fn connect(
        &self,
        service: &AgentService,
        session: &Session,
        interactive: bool,
        misses: &[RouteMiss],
    ) -> Option<Connect> {
        let Payer::Person(person) = crate::user_config::current_payer() else {
            return None;
        };
        if !interactive {
            return None;
        }
        let chatgpt_direct = service.subscriptions().chatgpt().enabled();
        let wanted = self.kinds.iter().filter_map(|kind| match kind {
            ProviderKind::ChatGpt if !chatgpt_direct => None,
            kind => kind.subscription_route().map(SubscriptionRoute::kind),
        });
        let mut seen = Vec::new();
        let mut unserved = Vec::new();
        for kind in wanted {
            if seen.contains(&kind) {
                continue;
            }
            seen.push(kind);
            let Err(no_login) = service.logins.resolve(kind) else {
                unserved.push(connect::Unserved {
                    kind,
                    reasons: self.unserved_on(kind, misses),
                });
                continue;
            };
            let serving = connect::connectable(service.agent_id.as_str(), &no_login);
            let Some(start) = serving.first().cloned() else {
                continue;
            };
            return Some(
                connect::prompt(
                    service,
                    session,
                    person,
                    kind,
                    (start, &serving),
                    &no_login,
                    &unserved,
                )
                .await,
            );
        }
        None
    }
}

/// `reason` as a person reads it: without the chain's subscription marker,
/// which only says the chain should move on.
fn person_facing(reason: &str) -> String {
    match reason.find(SUBSCRIPTION_UNAVAILABLE_MARKER) {
        Some(at) => reason[at + SUBSCRIPTION_UNAVAILABLE_MARKER.len()..]
            .trim_start_matches(':')
            .trim()
            .to_string(),
        None => reason.trim().to_string(),
    }
}

/// The provider a runner route runs on: its entries of the agent's chain,
/// and the slice they make, which says whether it produced output. Without a
/// chain the agent's provider is the route.
fn route_provider(
    service: &AgentService,
    raws: &[String],
) -> (Arc<dyn LlmProvider>, Option<Arc<ChainSlice>>) {
    match &service.failover_provider {
        Some(chain) => {
            let slice = chain.slice(raws.to_vec());
            (Arc::clone(&slice) as Arc<dyn LlmProvider>, Some(slice))
        }
        None => (Arc::clone(&service.llm), None),
    }
}

/// Whether a runner turn failed because a subscription it ran on could not
/// serve it, rather than on anything a login can't fix.
fn subscription_unavailable(error: &eyre::Report) -> bool {
    names_a_login(&format!("{error:#}"))
}

/// Whether a route's failure was its payer's login, not anything a login
/// can't fix.
fn names_a_login(reason: &str) -> bool {
    reason
        .to_lowercase()
        .contains(SUBSCRIPTION_UNAVAILABLE_MARKER)
}

/// The most logins one CLI route of a turn is tried on, each refused one
/// skipped.
const MAX_LOGINS_PER_ROUTE: usize = 3;

/// Whether a runner route produced output: always, with no slice to ask.
fn answered(slice: Option<&ChainSlice>) -> bool {
    slice.is_none_or(ChainSlice::answered)
}

/// A runner's turn, streamed when someone is watching it.
async fn run_runner(
    runner: &AgentRunner,
    task: Vec<ContentBlock>,
    prior: Vec<LlmMessage>,
    progress: Option<mpsc::Sender<ProgressEvent>>,
    cancel: Option<CancellationToken>,
    inject: Option<mpsc::Receiver<InjectedMessage>>,
) -> Result<RunResult> {
    match progress {
        Some(progress) => {
            runner
                .run_with_progress_and_history(task, prior, progress, cancel, inject)
                .await
        }
        None => runner.run_with_history_content(task, prior).await,
    }
}

/// `task` with the calls the turn already made set out before it, for a
/// route that did not make them itself: each call and its result as the
/// runner would have seen them, credentials already tokenised.
fn with_seeds(task: &[ContentBlock], seeds: &Seeds) -> Vec<ContentBlock> {
    let blocks = || seeds.messages.iter().flat_map(|message| &message.content);
    let calls = blocks().filter_map(|block| match block {
        ContentBlock::ToolUse { name, input, .. } => Some((name, input)),
        _ => None,
    });
    let results = blocks().filter_map(|block| match block {
        ContentBlock::ToolResult {
            content, is_error, ..
        } => Some((content, is_error.unwrap_or(false))),
        _ => None,
    });
    let made: Vec<String> = calls
        .zip(results)
        .map(|((name, input), (content, failed))| {
            let outcome = if failed { "failed: " } else { "" };
            format!("- {name} {input} → {outcome}{content}")
        })
        .collect();
    match made.is_empty() {
        true => task.to_vec(),
        false => std::iter::once(ContentBlock::Text {
            text: format!(
                "[CALLS MADE FOR THIS TURN — already run on your behalf; do not repeat them]\n{}",
                made.join("\n")
            ),
        })
        .chain(task.iter().cloned())
        .collect(),
    }
}

fn done_of(usage: &TokenUsage, rounds: u32, tool_count: usize) -> ProgressEvent {
    ProgressEvent::Done {
        rounds,
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        cache_read_tokens: usage.cache_read_input_tokens,
        tool_count,
    }
}
