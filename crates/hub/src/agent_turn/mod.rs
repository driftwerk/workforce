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
    Cli { cli: CliKind, model: String },
}

impl TurnRoute {
    /// How the route is named when it could not serve.
    fn name(&self) -> String {
        match self {
            Self::Runner(raws) => raws.join(" → "),
            Self::Cli { cli, model } => spec(*cli, model),
        }
    }

    /// The route did not serve, for `reason`.
    fn miss(&self, reason: impl Into<String>) -> RouteMiss {
        RouteMiss::new(self.name(), reason)
    }
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
        ))),
        Err(e) => tracing::error!(
            agent = %service.agent_id,
            error = %e,
            "agent turns on CLI routes not wired: the coding executor cannot be built"
        ),
    }
}

/// What runs a CLI `route` on this hub, else the route's miss.
fn rig_or_miss(service: &AgentService, route: &TurnRoute) -> Result<Arc<AgentTurnRig>, RouteMiss> {
    service
        .turn_rig
        .get::<AgentTurnRig>()
        .ok_or_else(|| route.miss("CLI turns are not wired on this hub"))
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

    /// The model a turn's attachments are prepared for: the first route's.
    /// A CLI route reads images natively.
    pub fn media_model(&self, resolved: &str) -> String {
        match self.routes.first() {
            Some(TurnRoute::Cli { cli, model }) => spec(*cli, model),
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
    /// make one: saved as the turn's answer, with nothing to bill.
    Notice { text: String },
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
}

impl RouteMiss {
    fn new(route: String, reason: impl Into<String>) -> Self {
        Self {
            route,
            reason: reason.into(),
        }
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
    /// Run a turn on the chain's routes in order, until one serves it.
    pub(crate) async fn run(
        &self,
        service: &AgentService,
        turn: TurnRun<'_>,
    ) -> Result<ChainOutcome> {
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
        for route in &self.routes {
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
                    match Box::pin(run_runner(
                        &runner,
                        task.clone(),
                        prior.clone(),
                        progress.clone(),
                        Some(cancel.clone()),
                        inbox,
                    ))
                    .await
                    {
                        Ok(result) => {
                            return Ok(ChainOutcome::Served {
                                result: Box::new(result),
                                route: RouteTag::Runner,
                            })
                        }
                        Err(e) if !answered(slice.as_deref()) && !cancel.is_cancelled() => {
                            tracing::warn!(agent = %service.agent_id, route = %route.name(), error = %e, "turn route unserved; trying the next");
                            misses.push(route.miss(e.to_string()));
                        }
                        Err(e) => return Err(e),
                    }
                }
                TurnRoute::Cli { cli, model } => {
                    let rig = match rig_or_miss(service, route) {
                        Ok(rig) => rig,
                        Err(miss) => {
                            misses.push(miss);
                            continue;
                        }
                    };
                    let attempt = Box::pin(rig.run(
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
                        },
                        Some(&conversation),
                        inbox,
                    ))
                    .await?;
                    match attempt {
                        CliAttempt::Served(served) => {
                            if let Some(progress) = &progress {
                                let _ = progress
                                    .send(ProgressEvent::Response {
                                        text: served.text.clone(),
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
                            return Ok(ChainOutcome::Served {
                                result: Box::new(served.into_result(seeds.invocations.clone())),
                                route: RouteTag::Cli { cli: *cli, slot },
                            });
                        }
                        CliAttempt::Unserved(miss) => {
                            tracing::warn!(agent = %service.agent_id, route = %miss.route, reason = %miss.reason, "turn route unserved; trying the next");
                            misses.push(miss);
                        }
                    }
                }
            }
        }
        let connect = match misses.iter().all(|miss| names_a_login(&miss.reason)) {
            true => {
                self.connect(service, conversation.session, interactive, &misses)
                    .await
            }
            false => None,
        };
        match connect {
            Some(connect) => Ok(connect.notice(progress.as_ref()).await),
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
