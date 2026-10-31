//! `agent.*` — the collaboration tools a remote MCP client uses to talk to
//! this hub's agents. Registered only into the remote surface, never into an
//! agent's own registry. The caller is always the identity in
//! `CURRENT_USER_ID`, which the remote route scopes before dispatch.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use parking_lot::Mutex;
use serde::Deserialize;
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;

use workforce_agent::runner::{
    InjectedMessage, ProgressEvent, CURRENT_USER_ID, SELF_SERVE_APPROVAL_TIMEOUT,
};
use workforce_agent::tools::{Tool, ToolResult};
use workforce_common::sentinel_config::SentinelConfig;
use workforce_common::session::{MessageRole, SessionMessage};
use workforce_common::settings::RemoteMcpConfig;
use workforce_common::trust::{classify_trust_with_role, ContentSource};
use workforce_core::policy::{PermissionChoice, PermissionPromptOptions};
use workforce_identity::store::IdentityStore;

use crate::gateway::GatewayState;
use crate::serve::{AgentService, MessageProgress};

use crate::approvals::slack::hand_off;
use crate::ephemeral::{Ephemeral, EphemeralRegistry};
use crate::hosts::RoutedApproval;
use crate::mcp_gate::{choice_names, offered, PendingCalls, DECIDE_NEXT};

use super::access::{reachable, reachable_ids};
use super::approvals::not_offered;
use super::server::session_channel;
use super::stream;

const HISTORY_DEFAULT: usize = 20;
const HISTORY_MAX: usize = 200;
const TURNS_PER_IDENTITY: usize = 8;
const TURN_TTL: Duration = Duration::from_secs(60 * 60);
const INJECT_BUFFER: usize = 32;

pub struct AgentToolsContext {
    /// The hub whose agents, and whose access rules for them, the tools
    /// act through.
    pub gateway: Arc<GatewayState>,
    pub turns: Arc<ChatTurns>,
    pub pending_calls: Arc<PendingCalls>,
    pub routed: mpsc::Sender<RoutedApproval>,
    pub config: RemoteMcpConfig,
    pub identity_store: Option<Arc<dyn IdentityStore>>,
    pub sentinel: SentinelConfig,
}

impl AgentToolsContext {
    /// How long a call may wait on a turn: past the default only on a stream.
    pub(super) fn turn_wait(&self, requested: Option<u64>) -> Duration {
        Duration::from_secs(self.config.chat_wait_secs(requested, stream::active()))
    }
}

pub fn agent_tools(ctx: Arc<AgentToolsContext>) -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(ListTool {
            ctx: Arc::clone(&ctx),
        }),
        Arc::new(ChatTool {
            ctx: Arc::clone(&ctx),
        }),
        Arc::new(PollTool {
            ctx: Arc::clone(&ctx),
        }),
        Arc::new(HistoryTool { ctx }),
    ]
}

pub(super) fn caller() -> Result<String, ToolResult> {
    CURRENT_USER_ID
        .try_with(|u| u.clone())
        .ok()
        .filter(|u| !u.is_empty())
        .ok_or_else(|| ToolResult::failure("no caller identity in scope", 0))
}

/// The agent `agent_id` names (the default agent when it names none), if
/// `uid` may reach it. An agent they may not use is refused exactly as one
/// that does not exist.
pub(super) fn reach_agent(
    ctx: &AgentToolsContext,
    uid: &str,
    agent_id: Option<&str>,
) -> Result<Arc<AgentService>, ToolResult> {
    reachable(&ctx.gateway, uid, agent_id).map_err(|refused| {
        let which = match refused.asked() {
            "" => "no default agent you can reach".to_string(),
            asked => format!("unknown agent {asked:?}"),
        };
        ToolResult::failure(
            format!("{which}; call agent.list for the agents you can reach on this hub"),
            0,
        )
    })
}

pub(super) fn unwrap_or_result(step: Result<ToolResult, ToolResult>) -> eyre::Result<ToolResult> {
    Ok(step.unwrap_or_else(|failure| failure))
}

// ── Turns ────────────────────────────────────────────────────────────────

pub type TurnOutcome = Result<String, String>;

/// A self-serve approval the running turn is waiting on.
#[derive(Debug, Clone)]
pub struct ApprovalPrompt {
    pub request_id: String,
    pub tool_name: String,
    pub detail: String,
    pub reason: String,
    pub options: PermissionPromptOptions,
    raised: Instant,
}

impl ApprovalPrompt {
    /// The runner has already refused the call as unanswered.
    fn expired(&self) -> bool {
        self.raised.elapsed() >= SELF_SERVE_APPROVAL_TIMEOUT
    }

    fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "request_id": self.request_id,
            "tool": self.tool_name,
            "detail": self.detail,
            "reason": self.reason,
            "options": choice_names(&self.options),
            "seconds_left": SELF_SERVE_APPROVAL_TIMEOUT
                .saturating_sub(self.raised.elapsed())
                .as_secs(),
        })
    }
}

/// What a turn has raised so far: prompts waiting on the caller, approvals
/// routed to designated approvers, and the outcome once it settles.
#[derive(Debug, Clone, Default)]
pub struct TurnState {
    pending: Vec<ApprovalPrompt>,
    routed: Vec<RoutedApproval>,
    outcome: Option<TurnOutcome>,
}

impl TurnState {
    fn needs_caller(&self) -> bool {
        self.outcome.is_some() || !self.pending.is_empty()
    }
}

/// One `agent.chat` turn that outlived its caller's wait: the collector keeps
/// draining progress into it until the run settles, whether or not anyone
/// polls.
pub struct ChatTurn {
    pub owner: String,
    pub session_id: String,
    pub agent_id: String,
    started: Instant,
    cancel: CancellationToken,
    tools_used: Mutex<Vec<String>>,
    delivered: Mutex<usize>,
    inject: mpsc::Sender<InjectedMessage>,
    state: watch::Sender<TurnState>,
}

impl ChatTurn {
    /// Follow the turn `progress` reports on; `slot` is held until it
    /// settles, or until it has run the slot's whole life: then it is
    /// cancelled and the slot freed, whether or not anyone kept the turn for
    /// polling, so a turn that never ends can't hold one of its owner's
    /// slots for good.
    fn spawn(
        owner: String,
        agent_id: String,
        progress: MessageProgress,
        cancel: CancellationToken,
        inject: mpsc::Sender<InjectedMessage>,
        routed: mpsc::Sender<RoutedApproval>,
        slot: TurnSlot,
    ) -> Arc<Self> {
        let turn = Arc::new(Self {
            owner,
            session_id: progress.session_id.clone(),
            agent_id,
            started: Instant::now(),
            cancel,
            tools_used: Mutex::new(Vec::new()),
            delivered: Mutex::new(0),
            inject,
            state: watch::Sender::new(TurnState::default()),
        });
        let collector = Arc::clone(&turn);
        let MessageProgress {
            mut progress_rx,
            handle,
            ..
        } = progress;
        tokio::spawn(async move {
            let drain = async {
                while let Some(event) = progress_rx.recv().await {
                    collector.observe(event, &routed);
                }
            };
            let settled = async { tokio::join!(handle, drain) };
            tokio::pin!(settled);
            let ttl = slot.ttl;
            let mut slot = Some(slot);
            let (run, ()) = tokio::select! {
                done = &mut settled => done,
                () = tokio::time::sleep(ttl) => {
                    tracing::warn!(owner = %collector.owner, agent = %collector.agent_id, "remote MCP: a turn outlived its time and is cancelled");
                    collector.cancel.cancel();
                    slot = None;
                    settled.await
                }
            };
            let outcome = match run {
                Ok(Ok(result)) => Ok(result.response),
                Ok(Err(e)) => Err(e.to_string()),
                Err(e) => Err(format!("turn task failed: {e}")),
            };
            // Freed first, so a caller who sees the outcome may start
            // another turn at once.
            drop(slot);
            collector.state.send_modify(|s| s.outcome = Some(outcome));
        });
        turn
    }

    fn observe(&self, event: ProgressEvent, routed: &mpsc::Sender<RoutedApproval>) {
        match event {
            ProgressEvent::ToolStart { name, .. } => self.tools_used.lock().push(name),
            ProgressEvent::ApprovalResolved { request_id } => self.forget(&request_id),
            ProgressEvent::ApprovalRequired {
                request_id,
                agent_id,
                tool_name,
                detail,
                reason,
                options,
                routing: Some(routing),
                ..
            } => {
                let approval = RoutedApproval {
                    agent_id,
                    request_id,
                    tool_name,
                    detail,
                    reason,
                    options,
                    approver_user_ids: routing.approver_user_ids,
                    requester_user_id: routing.requester_user_id,
                    channel: None,
                    thread: None,
                };
                self.state.send_modify(|s| s.routed.push(approval.clone()));
                hand_off(routed, approval);
            }
            ProgressEvent::ApprovalRequired {
                request_id,
                tool_name,
                detail,
                reason,
                options,
                routing: None,
                ..
            } => self.state.send_modify(|s| {
                s.pending.push(ApprovalPrompt {
                    request_id,
                    tool_name,
                    detail,
                    reason,
                    options,
                    raised: Instant::now(),
                })
            }),
            _ => {}
        }
    }

    #[cfg(test)]
    fn settled(owner: &str, started: Instant, outcome: Option<TurnOutcome>) -> Arc<Self> {
        Arc::new(Self {
            owner: owner.to_string(),
            session_id: "s".into(),
            agent_id: "a".into(),
            started,
            cancel: CancellationToken::new(),
            tools_used: Mutex::new(Vec::new()),
            delivered: Mutex::new(0),
            inject: mpsc::channel(1).0,
            state: watch::Sender::new(TurnState {
                outcome,
                ..Default::default()
            }),
        })
    }

    /// Block until the turn settles or raises a prompt for the caller, or
    /// `for_at_most` passes; either way the current state comes back.
    pub(super) async fn wait(&self, for_at_most: Duration) -> TurnState {
        let mut rx = self.state.subscribe();
        let _ = tokio::time::timeout(for_at_most, rx.wait_for(TurnState::needs_caller)).await;
        self.state.borrow().clone()
    }

    fn state(&self) -> TurnState {
        self.state.borrow().clone()
    }

    pub(super) fn awaits(&self, request_id: &str) -> bool {
        self.prompt(request_id).is_some()
    }

    pub(super) fn prompt(&self, request_id: &str) -> Option<serde_json::Value> {
        self.state
            .borrow()
            .pending
            .iter()
            .find(|p| p.request_id == request_id)
            .map(ApprovalPrompt::to_json)
    }

    /// Deliver the caller's decision to the runner and clear the prompt.
    pub(super) fn answer(
        &self,
        request_id: &str,
        choice: PermissionChoice,
        user: &str,
    ) -> Result<(), ToolResult> {
        let prompt = {
            let state = self.state.borrow();
            state
                .pending
                .iter()
                .find(|p| p.request_id == request_id)
                .map(|p| (p.expired(), offered(&p.options, choice)))
        };
        match prompt {
            None => return Err(ToolResult::failure("unknown request", 0)),
            Some((true, _)) => {
                self.forget(request_id);
                return Err(ToolResult::failure(
                    "this prompt expired unanswered, so the call did not run",
                    0,
                ));
            }
            Some((false, false)) => return Err(not_offered(choice)),
            Some((false, true)) => {}
        }
        self.inject
            .try_send(InjectedMessage::ApprovalResponse {
                request_id: request_id.to_string(),
                approved: choice.is_allow(),
                user: user.to_string(),
                choice: Some(choice),
            })
            .map_err(|_| ToolResult::failure("the turn is no longer accepting approvals", 0))?;
        self.forget(request_id);
        Ok(())
    }

    /// Drop a prompt that can no longer be answered, waking waiters only when
    /// it was still listed.
    fn forget(&self, request_id: &str) {
        self.state.send_if_modified(|s| {
            let listed = s.pending.len();
            s.pending.retain(|p| p.request_id != request_id);
            s.pending.len() != listed
        });
    }

    fn tools_used(&self) -> Vec<String> {
        self.tools_used.lock().clone()
    }

    /// Tool names recorded since the previous call.
    fn tools_used_since_last_poll(&self) -> Vec<String> {
        let all = self.tools_used.lock();
        let mut delivered = self.delivered.lock();
        let fresh = all[*delivered..].to_vec();
        *delivered = all.len();
        fresh
    }
}

impl Ephemeral for ChatTurn {
    fn owner(&self) -> &str {
        &self.owner
    }

    fn started(&self) -> Instant {
        self.started
    }

    /// A settled turn holds only its reply, which its session keeps too.
    fn idle(&self) -> bool {
        self.state.borrow().outcome.is_some()
    }

    fn evict(&self) {
        self.cancel.cancel();
    }
}

/// One of a caller's running turns, held from before the turn starts until
/// it settles.
pub struct TurnSlot {
    running: Arc<Mutex<HashMap<String, usize>>>,
    owner: String,
    /// The longest its turn may hold it.
    ttl: Duration,
}

impl Drop for TurnSlot {
    fn drop(&mut self) {
        let mut running = self.running.lock();
        if let Some(count) = running.get_mut(&self.owner) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                running.remove(&self.owner);
            }
        }
    }
}

/// Each caller's running turns, at most `per_identity` at once, and the
/// turns that outlived their caller's wait, kept for `agent.poll`.
pub struct ChatTurns {
    held: EphemeralRegistry<ChatTurn>,
    running: Arc<Mutex<HashMap<String, usize>>>,
    per_identity: usize,
    ttl: Duration,
}

impl Default for ChatTurns {
    fn default() -> Self {
        Self::bounded(TURNS_PER_IDENTITY, TURN_TTL)
    }
}

impl ChatTurns {
    pub fn bounded(per_identity: usize, ttl: Duration) -> Self {
        Self {
            held: EphemeralRegistry::bounded(per_identity, ttl),
            running: Arc::default(),
            per_identity,
            ttl,
        }
    }

    /// A slot for one more of `owner`'s turns, taken before the turn starts.
    /// Refused while they already have `per_identity` running: a new turn
    /// never cancels one of theirs.
    pub(super) fn admit(&self, owner: &str) -> Result<TurnSlot, ToolResult> {
        let mut running = self.running.lock();
        let count = running.entry(owner.to_string()).or_default();
        if *count >= self.per_identity {
            return Err(ToolResult::failure(
                format!(
                    "too many turns running: you already have {} agent.chat turns running on this hub. \
                     Wait for one to finish (agent.poll reports on each) and try again.",
                    self.per_identity
                ),
                0,
            ));
        }
        *count += 1;
        Ok(TurnSlot {
            running: Arc::clone(&self.running),
            owner: owner.to_string(),
            ttl: self.ttl,
        })
    }

    /// Keep a turn for `agent.poll`, under a fresh id. Room is made only
    /// from the owner's settled turns: their running ones are already
    /// bounded by [`ChatTurns::admit`].
    pub(super) fn insert(&self, turn: Arc<ChatTurn>) -> String {
        self.held.insert(turn)
    }

    pub(super) fn get(&self, id: &str, owner: &str) -> Option<Arc<ChatTurn>> {
        self.held.get(id, owner)
    }

    pub(super) fn find(
        &self,
        owner: &str,
        pick: impl Fn(&ChatTurn) -> bool,
    ) -> Option<(String, Arc<ChatTurn>)> {
        self.held.find(owner, pick)
    }

    pub(super) fn remove(&self, id: &str) {
        self.held.remove(id);
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.held.len()
    }
}

/// The caller-facing view of a turn: its reply once settled (and the turn
/// forgotten), else its handle plus whatever it is waiting on. A turn
/// without a handle yet is registered here.
pub(super) fn turn_result(
    ctx: &AgentToolsContext,
    turn: &Arc<ChatTurn>,
    turn_id: Option<String>,
) -> ToolResult {
    let state = turn.state();
    let routed: Vec<serde_json::Value> = state.routed.iter().map(RoutedApproval::notice).collect();
    match state.outcome {
        Some(outcome) => {
            if let Some(id) = &turn_id {
                ctx.turns.remove(id);
            }
            settled_result(turn, outcome, turn.tools_used(), routed)
        }
        None => {
            let turn_id = turn_id.unwrap_or_else(|| ctx.turns.insert(Arc::clone(turn)));
            let mut payload = serde_json::json!({
                "turn_id": turn_id,
                "session_id": turn.session_id,
                "agent_id": turn.agent_id,
                "tools_used_since_last_poll": turn.tools_used_since_last_poll(),
                "routed_approvals": routed,
            });
            if state.pending.is_empty() {
                payload["status"] = "running".into();
                payload["next"] = "call agent.poll with this turn_id".into();
            } else {
                payload["status"] = "approval_required".into();
                payload["approvals"] = state.pending.iter().map(ApprovalPrompt::to_json).collect();
                payload["next"] = DECIDE_NEXT.into();
            }
            ToolResult::success(payload, 0)
        }
    }
}

// ── agent.list ───────────────────────────────────────────────────────────

struct ListTool {
    ctx: Arc<AgentToolsContext>,
}

#[async_trait]
impl Tool for ListTool {
    fn name(&self) -> &str {
        "agent.list"
    }

    fn description(&self) -> &str {
        "List the agents on this hub you can talk to with agent.chat: id, whether it is the default, how many tools it has and their families (the dot-prefix, e.g. jira, metabase, github). agent.show has one agent's details."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object", "properties": {} })
    }

    async fn execute(&self, _input: serde_json::Value) -> eyre::Result<ToolResult> {
        let uid = match caller() {
            Ok(uid) => uid,
            Err(failure) => return Ok(failure),
        };
        let registry = &self.ctx.gateway.registry;
        let default_id = registry.default_id();
        let agents: Vec<serde_json::Value> = reachable_ids(&self.ctx.gateway, &uid)
            .iter()
            .filter_map(|id| {
                let tools = registry.get(id)?.tools.names();
                Some(agent_summary(id, id == default_id, &tools))
            })
            .collect();
        Ok(ToolResult::success(
            serde_json::json!({ "agents": agents }),
            0,
        ))
    }
}

/// One agent as the caller sees it: an agent with hundreds of tools is
/// described by their families, not their names.
fn agent_summary(id: &str, default: bool, tools: &[String]) -> serde_json::Value {
    let mut families: Vec<&str> = tools
        .iter()
        .map(|t| crate::failed_tools::tool_family(t))
        .collect();
    families.sort_unstable();
    families.dedup();
    serde_json::json!({
        "id": id,
        "default": default,
        "tool_count": tools.len(),
        "families": families,
    })
}

// ── agent.chat ───────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, PartialEq, Eq)]
pub(crate) struct ChatArgs {
    pub message: String,
    #[serde(default)]
    pub agent_id: Option<String>,
    #[serde(default)]
    pub session: Option<String>,
    #[serde(default)]
    pub wait_seconds: Option<u64>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum SessionChoice<'a> {
    Continue,
    New,
    Resume(&'a str),
}

impl ChatArgs {
    pub(crate) fn session_choice(&self) -> SessionChoice<'_> {
        match self.session.as_deref().map(str::trim) {
            None | Some("") | Some("continue") => SessionChoice::Continue,
            Some("new") => SessionChoice::New,
            Some(id) => SessionChoice::Resume(id),
        }
    }
}

struct ChatTool {
    ctx: Arc<AgentToolsContext>,
}

impl ChatTool {
    fn trust_for(&self, identity_id: &str) -> workforce_common::trust::TrustLevel {
        let role = self
            .ctx
            .identity_store
            .as_ref()
            .zip(uuid::Uuid::parse_str(identity_id).ok())
            .and_then(|(store, uid)| store.get_by_id(uid).ok().flatten())
            .map(|identity| identity.role.as_str());
        classify_trust_with_role(
            &ContentSource::Gateway { device_id: None },
            None,
            &self.ctx.sentinel,
            true,
            role,
        )
    }

    async fn run(&self, args: ChatArgs) -> Result<ToolResult, ToolResult> {
        let uid = caller()?;
        let service = reach_agent(&self.ctx, &uid, args.agent_id.as_deref())?;
        let slot = self.ctx.turns.admit(&uid)?;
        let channel = session_channel(&uid);
        let resume = match args.session_choice() {
            SessionChoice::Continue => None,
            SessionChoice::New => {
                service.create_new_session(&channel, &uid).map_err(|e| {
                    ToolResult::failure(format!("could not start a new session: {e:#}"), 0)
                })?;
                None
            }
            SessionChoice::Resume(id) => {
                own_session(&service, id, &uid, &channel)?;
                Some(id.to_string())
            }
        };
        let cancel = CancellationToken::new();
        let (inject_tx, inject_rx) = mpsc::channel(INJECT_BUFFER);
        let progress = service
            .process_message_with_progress(
                &channel,
                &uid,
                None,
                &args.message,
                None,
                resume.as_deref(),
                None,
                None,
                None,
                None,
                Some(crate::serve::TurnInbox {
                    rx: inject_rx,
                    self_serve: workforce_agent::tools::approval::SelfServe::Answerable,
                }),
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                Some(self.trust_for(&uid)),
                Some(cancel.clone()),
                None,
                None,
            )
            .await
            .map_err(|e| ToolResult::failure(format!("could not start the turn: {e:#}"), 0))?;
        Ok(open_turn(
            &self.ctx,
            uid,
            service.agent_id.to_string(),
            progress,
            cancel,
            inject_tx,
            args.wait_seconds,
            slot,
        )
        .await)
    }
}

/// Follow a started turn for its caller: collect its progress, wait up to the
/// caller's patience, and report where it stands. `slot` is the turn's place
/// among the caller's running turns, held until it settles.
#[allow(clippy::too_many_arguments)]
pub(super) async fn open_turn(
    ctx: &AgentToolsContext,
    owner: String,
    agent_id: String,
    progress: MessageProgress,
    cancel: CancellationToken,
    inject: mpsc::Sender<InjectedMessage>,
    wait_seconds: Option<u64>,
    slot: TurnSlot,
) -> ToolResult {
    let turn = ChatTurn::spawn(
        owner,
        agent_id,
        progress,
        cancel,
        inject,
        ctx.routed.clone(),
        slot,
    );
    turn.wait(ctx.turn_wait(wait_seconds)).await;
    turn_result(ctx, &turn, None)
}

fn own_session(
    service: &AgentService,
    session_id: &str,
    uid: &str,
    channel: &str,
) -> Result<workforce_common::session::Session, ToolResult> {
    service
        .sessions
        .get(session_id)
        .ok()
        .flatten()
        .filter(|s| s.owner == uid && s.channel == channel)
        .ok_or_else(|| ToolResult::failure("unknown session", 0))
}

fn settled_result(
    turn: &ChatTurn,
    outcome: TurnOutcome,
    tools_used: Vec<String>,
    routed_approvals: Vec<serde_json::Value>,
) -> ToolResult {
    match outcome {
        Ok(reply) => ToolResult::success(
            serde_json::json!({
                "status": "done",
                "session_id": turn.session_id,
                "agent_id": turn.agent_id,
                "reply": reply,
                "tools_used": tools_used,
                "routed_approvals": routed_approvals,
            }),
            0,
        ),
        Err(error) => ToolResult::failure(format!("turn failed: {error}"), 0),
    }
}

#[async_trait]
impl Tool for ChatTool {
    fn name(&self) -> &str {
        "agent.chat"
    }

    fn description(&self) -> &str {
        "Send a message to a workforce agent as yourself and get its reply. Continues your persistent session with that agent by default; session=\"new\" starts a fresh one, or pass a session_id from agent.history. Waits up to wait_seconds (hub default) — a longer turn returns status \"running\" with a turn_id for agent.poll, and a turn that needs your say-so on a tool call returns status \"approval_required\" with prompts to answer through approval.decide. If the call itself times out, the reply still lands in the session (agent.history)."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "message": { "type": "string", "description": "What to say to the agent" },
                "agent_id": { "type": "string", "description": "Agent to talk to (default: the hub's default agent; see agent.list)" },
                "session": { "type": "string", "description": "\"continue\" (default), \"new\", or a session_id to resume" },
                "wait_seconds": { "type": "integer", "minimum": 1, "description": "How long to wait for the reply before returning a turn_id" }
            },
            "required": ["message"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> eyre::Result<ToolResult> {
        let args: ChatArgs = match serde_json::from_value(input) {
            Ok(a) => a,
            Err(e) => return Ok(ToolResult::failure(format!("invalid arguments: {e}"), 0)),
        };
        unwrap_or_result(self.run(args).await)
    }
}

// ── agent.poll ───────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct PollArgs {
    turn_id: String,
}

const UNKNOWN_TURN: &str = "unknown turn: this hub holds no such turn, as happens once the hub that ran it restarts. A turn cut short that way is finished and its reply saved to its session; read it with agent.history and the session_id agent.chat returned.";

struct PollTool {
    ctx: Arc<AgentToolsContext>,
}

#[async_trait]
impl Tool for PollTool {
    fn name(&self) -> &str {
        "agent.poll"
    }

    fn description(&self) -> &str {
        "Check on an agent.chat turn that returned a turn_id: reports the tools it has used since your last poll, any approval prompts waiting on you (answer them with approval.decide), and, once finished, the reply."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": { "turn_id": { "type": "string" } },
            "required": ["turn_id"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> eyre::Result<ToolResult> {
        let args: PollArgs = match serde_json::from_value(input) {
            Ok(a) => a,
            Err(e) => return Ok(ToolResult::failure(format!("invalid arguments: {e}"), 0)),
        };
        let step = caller().and_then(|uid| {
            let turn = self
                .ctx
                .turns
                .get(&args.turn_id, &uid)
                .ok_or_else(|| ToolResult::failure(UNKNOWN_TURN, 0))?;
            reach_agent(&self.ctx, &uid, Some(&turn.agent_id))?;
            Ok(turn)
        });
        Ok(match step {
            Ok(turn) => turn_result(&self.ctx, &turn, Some(args.turn_id)),
            Err(failure) => failure,
        })
    }
}

// ── agent.history ────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct HistoryArgs {
    #[serde(default)]
    agent_id: Option<String>,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    last_n: Option<usize>,
}

struct HistoryTool {
    ctx: Arc<AgentToolsContext>,
}

impl HistoryTool {
    fn run(&self, args: HistoryArgs) -> Result<ToolResult, ToolResult> {
        let uid = caller()?;
        let service = reach_agent(&self.ctx, &uid, args.agent_id.as_deref())?;
        let channel = session_channel(&uid);
        let session = match args.session_id.as_deref() {
            Some(id) => own_session(&service, id, &uid, &channel)?,
            None => service
                .sessions
                .get_or_create(&channel, &uid, service.agent_id.as_str())
                .map_err(|e| ToolResult::failure(format!("session error: {e:#}"), 0))?,
        };
        let last_n = args.last_n.unwrap_or(HISTORY_DEFAULT).clamp(1, HISTORY_MAX);
        Ok(ToolResult::success(
            serde_json::json!({
                "session_id": session.id,
                "agent_id": session.agent_id,
                "messages": history_messages(&session.messages, last_n),
            }),
            0,
        ))
    }
}

/// The last `last_n` conversational messages, oldest first. System entries
/// (the prompt, compaction summaries) are the agent's internals, not the
/// dialogue the caller took part in.
fn history_messages(messages: &[SessionMessage], last_n: usize) -> Vec<serde_json::Value> {
    let dialogue: Vec<&SessionMessage> = messages
        .iter()
        .filter(|m| m.role != MessageRole::System)
        .collect();
    dialogue
        .iter()
        .skip(dialogue.len().saturating_sub(last_n))
        .map(|m| {
            serde_json::json!({
                "role": m.role,
                "content": m.content,
                "timestamp": m.timestamp,
            })
        })
        .collect()
}

#[async_trait]
impl Tool for HistoryTool {
    fn name(&self) -> &str {
        "agent.history"
    }

    fn description(&self) -> &str {
        "Read the recent messages of your own session with an agent (default: your current session with the default agent)."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "agent_id": { "type": "string" },
                "session_id": { "type": "string" },
                "last_n": { "type": "integer", "minimum": 1, "maximum": HISTORY_MAX, "description": "Conversational messages to return, newest last (default 20); system entries are omitted" }
            }
        })
    }

    async fn execute(&self, input: serde_json::Value) -> eyre::Result<ToolResult> {
        let args: HistoryArgs = match serde_json::from_value(input) {
            Ok(a) => a,
            Err(e) => return Ok(ToolResult::failure(format!("invalid arguments: {e}"), 0)),
        };
        unwrap_or_result(self.run(args))
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::super::approvals::approval_tools;
    use super::*;

    #[test]
    fn chat_args_default_to_continuing_the_session() {
        let args: ChatArgs =
            serde_json::from_value(serde_json::json!({ "message": "hi" })).unwrap();
        assert_eq!(args.session_choice(), SessionChoice::Continue);
        assert_eq!(args.wait_seconds, None);
        assert_eq!(args.agent_id, None);
    }

    #[test]
    fn chat_args_distinguish_new_continue_and_resume() {
        let with = |session: &str| ChatArgs {
            message: "hi".into(),
            agent_id: None,
            session: Some(session.into()),
            wait_seconds: None,
        };
        assert_eq!(with("new").session_choice(), SessionChoice::New);
        assert_eq!(with("continue").session_choice(), SessionChoice::Continue);
        assert_eq!(with(" ").session_choice(), SessionChoice::Continue);
        assert_eq!(
            with("abc-123").session_choice(),
            SessionChoice::Resume("abc-123")
        );
    }

    #[test]
    fn chat_args_require_a_message() {
        assert!(
            serde_json::from_value::<ChatArgs>(serde_json::json!({ "agent_id": "x" })).is_err()
        );
    }

    #[test]
    fn an_unstreamed_wait_stops_at_the_default() {
        let ctx = test_ctx();
        let default = Duration::from_secs(ctx.config.chat_default_wait_secs);
        assert_eq!(ctx.turn_wait(None), default);
        assert_eq!(ctx.turn_wait(Some(u64::MAX)), default);
        assert_eq!(ctx.turn_wait(Some(1)), Duration::from_secs(1));
    }

    /// `agent.chat` with a session id continues that session only for the
    /// person its turns run as: an administrator's identity is another caller.
    #[tokio::test]
    async fn a_session_is_resumed_only_by_its_owner() {
        let tmp = tempfile::tempdir().unwrap();
        let service = crate::serve::testing::plain_service(tmp.path()).await;
        let channel = session_channel("alice");
        let session = service
            .sessions
            .get_or_create(&channel, "alice", service.agent_id.as_str())
            .unwrap();

        assert!(own_session(&service, &session.id, "alice", &channel).is_ok());
        for other in ["admin", "bob"] {
            assert!(own_session(&service, &session.id, other, &channel).is_err());
            assert!(
                own_session(&service, &session.id, other, &session_channel(other)).is_err(),
                "{other}"
            );
        }
    }

    #[test]
    fn turns_are_owner_checked() {
        let turns = ChatTurns::default();
        let id = turns.insert(ChatTurn::settled("alice", Instant::now(), None));
        assert!(turns.get(&id, "alice").is_some());
        assert!(turns.get(&id, "bob").is_none());
        assert!(turns.get("nope", "alice").is_none());
    }

    /// Room for a turn kept for `agent.poll` is made from its owner's
    /// settled turns only: a running one is never cancelled for it.
    #[test]
    fn a_settled_turn_makes_room_and_a_running_one_is_never_evicted() {
        let turns = ChatTurns::bounded(2, Duration::from_secs(3600));
        let now = Instant::now();
        let running = ChatTurn::settled("alice", now - Duration::from_secs(30), None);
        let cancel = running.cancel.clone();
        let oldest = turns.insert(running);
        let settled = turns.insert(ChatTurn::settled(
            "alice",
            now - Duration::from_secs(20),
            Some(Ok("done".into())),
        ));
        let bobs = turns.insert(ChatTurn::settled(
            "bob",
            now - Duration::from_secs(25),
            None,
        ));
        let newest = turns.insert(ChatTurn::settled("alice", now, None));
        assert!(
            turns.get(&settled, "alice").is_none(),
            "the settled turn made room"
        );
        assert!(
            turns.get(&oldest, "alice").is_some(),
            "the running turn stays"
        );
        assert!(!cancel.is_cancelled(), "and keeps running");
        assert!(turns.get(&newest, "alice").is_some());
        assert!(turns.get(&bobs, "bob").is_some(), "other owners untouched");
        assert_eq!(turns.len(), 3);
    }

    #[test]
    fn a_turn_past_the_per_identity_cap_is_refused_rather_than_cancelling_one() {
        let turns = ChatTurns::bounded(2, Duration::from_secs(3600));
        let first = turns.admit("alice").unwrap();
        let _second = turns.admit("alice").unwrap();
        let refused = turns.admit("alice").err().expect("a third is refused");
        let error = refused.error.unwrap_or_default();
        assert!(error.starts_with("too many turns running"), "{error}");
        assert!(turns.admit("bob").is_ok(), "other callers are not held up");
        drop(first);
        assert!(
            turns.admit("alice").is_ok(),
            "a settled turn frees its slot"
        );
    }

    /// A turn that never ends, kept for polling or not, is cancelled when
    /// its time is up and gives its slot back: its owner is never locked out.
    #[tokio::test]
    async fn a_turn_that_never_ends_frees_its_slot_when_its_time_is_up() {
        let turns = ChatTurns::bounded(1, Duration::from_millis(200));
        let live = live_turn_in(&turns);
        assert!(turns.admit("alice").is_err(), "the one slot is held");
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(live.turn.cancel.is_cancelled(), "the turn was cancelled");
        assert!(
            turns.admit("alice").is_ok(),
            "its slot is free though the stub never settled"
        );
        drop(live.finish);
    }

    #[test]
    fn expired_turns_are_swept_on_insert() {
        let turns = ChatTurns::bounded(8, Duration::from_secs(60));
        let now = Instant::now();
        let stale = turns.insert(ChatTurn::settled(
            "alice",
            now - Duration::from_secs(120),
            None,
        ));
        let fresh = turns.insert(ChatTurn::settled("alice", now, None));
        assert!(turns.get(&stale, "alice").is_none());
        assert!(turns.get(&fresh, "alice").is_some());
    }

    #[test]
    fn a_turn_past_its_age_is_cancelled() {
        let turns = ChatTurns::bounded(8, Duration::from_secs(60));
        let stale = ChatTurn::settled("alice", Instant::now() - Duration::from_secs(120), None);
        let cancel = stale.cancel.clone();
        turns.insert(stale);
        turns.insert(ChatTurn::settled("alice", Instant::now(), None));
        assert!(cancel.is_cancelled());
    }

    #[test]
    fn polling_reports_only_tools_since_the_last_poll() {
        let turn = ChatTurn::settled("alice", Instant::now(), None);
        turn.tools_used
            .lock()
            .extend(["a".to_string(), "b".to_string()]);
        assert_eq!(turn.tools_used_since_last_poll(), ["a", "b"]);
        turn.tools_used.lock().push("c".to_string());
        assert_eq!(turn.tools_used_since_last_poll(), ["c"]);
        assert!(turn.tools_used_since_last_poll().is_empty());
        assert_eq!(turn.tools_used(), ["a", "b", "c"]);
    }

    #[tokio::test]
    async fn waiting_on_a_settled_turn_returns_at_once() {
        let turn = ChatTurn::settled("alice", Instant::now(), Some(Ok("done".into())));
        assert_eq!(
            turn.wait(Duration::from_millis(10)).await.outcome,
            Some(Ok("done".into()))
        );
        let pending = ChatTurn::settled("alice", Instant::now(), None);
        assert_eq!(pending.wait(Duration::from_millis(10)).await.outcome, None);
    }

    pub(in crate::remote_mcp) fn test_ctx() -> Arc<AgentToolsContext> {
        ctx_on(&crate::remote_mcp::testing::fixture(true))
    }

    fn ctx_on(fx: &crate::remote_mcp::testing::Fixture) -> Arc<AgentToolsContext> {
        Arc::new(AgentToolsContext {
            gateway: Arc::clone(&fx.state),
            turns: Arc::new(ChatTurns::default()),
            pending_calls: Arc::new(PendingCalls::default()),
            routed: mpsc::channel(4).0,
            config: RemoteMcpConfig::default(),
            identity_store: None,
            sentinel: SentinelConfig::default(),
        })
    }

    struct LiveTurn {
        turn: Arc<ChatTurn>,
        progress: mpsc::Sender<ProgressEvent>,
        finish: tokio::sync::oneshot::Sender<()>,
        inject: mpsc::Receiver<InjectedMessage>,
        routed: mpsc::Receiver<RoutedApproval>,
    }

    /// A turn whose runner is a stub: it emits whatever the test pushes and
    /// settles (with an error) when told to finish.
    fn live_turn() -> LiveTurn {
        live_turn_in(&ChatTurns::default())
    }

    fn live_turn_in(turns: &ChatTurns) -> LiveTurn {
        let (progress_tx, progress_rx) = mpsc::channel(8);
        let (finish, finished) = tokio::sync::oneshot::channel::<()>();
        let handle = tokio::spawn(async move {
            let _ = finished.await;
            Err(eyre::eyre!("stopped"))
        });
        let (inject_tx, inject_rx) = mpsc::channel(4);
        let (routed_tx, routed_rx) = mpsc::channel(4);
        let turn = ChatTurn::spawn(
            "alice".into(),
            "agent".into(),
            MessageProgress {
                progress_rx,
                handle,
                session_id: "s".into(),
                turn_token: String::new(),
                turn_claim_id: None,
                cancel_token: CancellationToken::new(),
                code_citations: Vec::new(),
                served: Default::default(),
            },
            CancellationToken::new(),
            inject_tx,
            routed_tx,
            turns.admit("alice").unwrap(),
        );
        LiveTurn {
            turn,
            progress: progress_tx,
            finish,
            inject: inject_rx,
            routed: routed_rx,
        }
    }

    fn approval_event(
        request_id: &str,
        routing: Option<workforce_agent::runner::ApprovalRouting>,
    ) -> ProgressEvent {
        ProgressEvent::ApprovalRequired {
            request_id: request_id.into(),
            agent_id: "agent".into(),
            tool_name: "vault.delete".into(),
            detail: "vault.delete path=x".into(),
            reason: "destructive".into(),
            round: 1,
            options: PermissionPromptOptions::ask(false),
            routing,
        }
    }

    #[tokio::test]
    async fn a_self_serve_prompt_wakes_the_wait_and_is_answered_into_the_runner() {
        let mut live = live_turn();
        live.progress
            .send(approval_event("agent-1-vault.delete", None))
            .await
            .unwrap();
        let state = live.turn.wait(Duration::from_secs(2)).await;
        assert_eq!(state.outcome, None);
        assert_eq!(state.pending.len(), 1);
        assert!(live.turn.awaits("agent-1-vault.delete"));
        let prompt = live.turn.prompt("agent-1-vault.delete").unwrap();
        assert_eq!(prompt["tool"], "vault.delete");
        assert_eq!(
            prompt["options"],
            serde_json::json!(["allow_once", "deny_once", "deny_always"])
        );

        assert!(live
            .turn
            .answer("nope", PermissionChoice::AllowOnce, "alice")
            .is_err());
        let refused = live
            .turn
            .answer(
                "agent-1-vault.delete",
                PermissionChoice::AllowAlways,
                "alice",
            )
            .unwrap_err();
        assert_eq!(
            refused.error.as_deref(),
            Some("allow_always is not offered for this prompt")
        );
        assert!(live.turn.awaits("agent-1-vault.delete"), "still pending");
        live.turn
            .answer(
                "agent-1-vault.delete",
                PermissionChoice::DenyAlways,
                "alice",
            )
            .unwrap();
        match live.inject.recv().await.unwrap() {
            InjectedMessage::ApprovalResponse {
                request_id,
                approved,
                user,
                choice,
            } => {
                assert_eq!(request_id, "agent-1-vault.delete");
                assert!(!approved);
                assert_eq!(user, "alice");
                assert_eq!(choice, Some(PermissionChoice::DenyAlways));
            }
            other => panic!("unexpected injection {other:?}"),
        }
        assert!(!live.turn.awaits("agent-1-vault.delete"));

        drop(live.progress);
        live.finish.send(()).unwrap();
        let state = live.turn.wait(Duration::from_secs(2)).await;
        assert_eq!(state.outcome, Some(Err("stopped".into())));
    }

    #[tokio::test]
    async fn a_routed_prompt_is_handed_to_delivery_and_noted_on_the_turn() {
        let mut live = live_turn();
        let routing = workforce_agent::runner::ApprovalRouting {
            approver_user_ids: vec!["bob".into()],
            requester_user_id: "alice".into(),
        };
        live.progress
            .send(approval_event(
                "11111111-1111-1111-1111-111111111111",
                Some(routing),
            ))
            .await
            .unwrap();
        let delivered = live.routed.recv().await.unwrap();
        assert_eq!(delivered.approver_user_ids, ["bob"]);
        assert_eq!(delivered.requester_user_id, "alice");
        assert_eq!(delivered.tool_name, "vault.delete");
        let state = live.turn.state();
        assert!(
            state.pending.is_empty(),
            "routed prompts are not the caller's to answer"
        );
        assert_eq!(state.routed, [delivered]);
        live.finish.send(()).unwrap();
    }

    fn prompt_raised(request_id: &str, age: Duration) -> ApprovalPrompt {
        ApprovalPrompt {
            request_id: request_id.into(),
            tool_name: "vault.delete".into(),
            detail: String::new(),
            reason: "destructive".into(),
            options: PermissionPromptOptions::default(),
            raised: Instant::now().checked_sub(age).unwrap(),
        }
    }

    #[test]
    fn an_expired_prompt_is_refused_and_forgotten() {
        let turn = ChatTurn::settled("alice", Instant::now(), None);
        turn.state.send_modify(|s| {
            s.pending
                .push(prompt_raised("old", SELF_SERVE_APPROVAL_TIMEOUT))
        });
        let refused = turn
            .answer("old", PermissionChoice::AllowOnce, "alice")
            .unwrap_err();
        assert_eq!(
            refused.error.as_deref(),
            Some("this prompt expired unanswered, so the call did not run")
        );
        assert!(!turn.awaits("old"));
    }

    /// The turn's state once `ready` holds, or as it stands after a few
    /// seconds.
    async fn state_once(turn: &ChatTurn, ready: impl Fn(&TurnState) -> bool) -> TurnState {
        let mut state = turn.state.subscribe();
        let _ = tokio::time::timeout(Duration::from_secs(5), state.wait_for(|s| ready(s))).await;
        turn.state()
    }

    #[tokio::test]
    async fn an_approval_resolved_event_prunes_only_its_prompt() {
        let live = live_turn();
        for request_id in ["first", "second"] {
            live.progress
                .send(approval_event(request_id, None))
                .await
                .unwrap();
        }
        assert_eq!(
            state_once(&live.turn, |s| s.pending.len() == 2)
                .await
                .pending
                .len(),
            2
        );
        live.progress
            .send(ProgressEvent::ToolEnd {
                name: "vault.delete".into(),
                success: false,
                duration_ms: 0,
                round: 1,
            })
            .await
            .unwrap();
        live.progress
            .send(ProgressEvent::ApprovalResolved {
                request_id: "first".into(),
            })
            .await
            .unwrap();
        drop(live.progress);
        live.finish.send(()).unwrap();
        let settled = state_once(&live.turn, |s| s.outcome.is_some()).await;
        assert!(settled.outcome.is_some());
        assert!(!live.turn.awaits("first"), "the runner settled it");
        assert!(
            live.turn.awaits("second"),
            "a failed call of the same tool does not settle another prompt"
        );
    }

    #[tokio::test]
    async fn turn_result_registers_a_waiting_turn_and_forgets_a_settled_one() {
        let ctx = test_ctx();
        let waiting = ChatTurn::settled("alice", Instant::now(), None);
        waiting.state.send_modify(|s| {
            s.pending.push(ApprovalPrompt {
                request_id: "r1".into(),
                tool_name: "vault.delete".into(),
                detail: String::new(),
                reason: "destructive".into(),
                options: PermissionPromptOptions::default(),
                raised: Instant::now(),
            })
        });
        let result = turn_result(&ctx, &waiting, None);
        assert_eq!(result.output["status"], "approval_required");
        assert_eq!(result.output["approvals"][0]["request_id"], "r1");
        let turn_id = result.output["turn_id"].as_str().unwrap().to_string();
        assert!(ctx.turns.find("alice", |t| t.awaits("r1")).is_some());

        let running = ChatTurn::settled("alice", Instant::now(), None);
        assert_eq!(
            turn_result(&ctx, &running, None).output["status"],
            "running"
        );
        assert_eq!(ctx.turns.len(), 2);

        let settled = ChatTurn::settled("alice", Instant::now(), Some(Ok("hi".into())));
        let result = turn_result(&ctx, &settled, Some(turn_id));
        assert_eq!(result.output["status"], "done");
        assert_eq!(result.output["reply"], "hi");
        assert_eq!(ctx.turns.len(), 1, "a settled turn is forgotten");
    }

    #[test]
    fn agents_are_summarised_by_tool_family() {
        let tools: Vec<String> = [
            "jira.search",
            "jira.get_issue",
            "metabase.card_query",
            "mcp.hubspot.contacts",
            "investigate",
            "jira.search",
        ]
        .map(String::from)
        .to_vec();
        let summary = agent_summary("atlas", true, &tools);
        assert_eq!(summary["id"], "atlas");
        assert_eq!(summary["default"], true);
        assert_eq!(summary["tool_count"], 6);
        assert_eq!(
            summary["families"],
            serde_json::json!(["investigate", "jira", "mcp.hubspot", "metabase"])
        );
        assert!(summary.get("skills").is_none());
    }

    #[test]
    fn history_omits_system_entries_and_counts_from_the_tail() {
        let msg = |role: MessageRole, content: &str| SessionMessage {
            id: content.to_string(),
            role,
            content: content.to_string(),
            tool_data: None,
            timestamp: chrono::Utc::now(),
            tokens: 1,
            external_id: None,
            attachments: None,
            trust_level: None,
            actor: None,
            desk: None,
        };
        let messages = [
            msg(MessageRole::System, "prompt"),
            msg(MessageRole::User, "one"),
            msg(MessageRole::Assistant, "two"),
            msg(MessageRole::System, "summary"),
            msg(MessageRole::Tool, "three"),
            msg(MessageRole::Assistant, "four"),
        ];
        let contents = |n: usize| -> Vec<String> {
            history_messages(&messages, n)
                .into_iter()
                .map(|m| m["content"].as_str().unwrap().to_string())
                .collect()
        };
        assert_eq!(contents(2), ["three", "four"]);
        assert_eq!(contents(10), ["one", "two", "three", "four"]);
        assert!(history_messages(&messages[..1], 5).is_empty());
    }

    mod real_runner {
        use super::*;
        use workforce_agent::llm::types::ContentBlock;
        use workforce_agent::tools::approval::testing::{
            as_member, gated_runner, RecordingTool, ScriptedModel, MEMBER,
        };

        const PATIENCE: u64 = 5;

        fn entry(name: &str) -> serde_json::Value {
            serde_json::json!({ "name": name })
        }

        /// Start a real gated runner under the bridge, as `agent.chat` does,
        /// on a hub where its agent is registered; the hub's data lives as
        /// long as the returned directory.
        async fn open_gated_chat(
            model: ScriptedModel,
            tool: &Arc<RecordingTool>,
        ) -> (Arc<AgentToolsContext>, ToolResult, tempfile::TempDir) {
            let data = tempfile::tempdir().unwrap();
            let fx = crate::remote_mcp::testing::fixture(true);
            crate::remote_mcp::testing::with_agents(&fx, data.path(), &["agent"]).await;
            let ctx = ctx_on(&fx);
            let runner = gated_runner(model, Arc::clone(tool));
            let (progress_tx, progress_rx) = mpsc::channel(64);
            let (inject_tx, inject_rx) = mpsc::channel(INJECT_BUFFER);
            let cancel = CancellationToken::new();
            let handle = tokio::spawn(as_member(async move {
                runner
                    .run_with_progress_and_history(
                        vec![ContentBlock::Text {
                            text: "remove the entry".into(),
                        }],
                        Vec::new(),
                        progress_tx,
                        None,
                        Some(inject_rx),
                    )
                    .await
            }));
            let progress = MessageProgress {
                progress_rx,
                handle,
                session_id: "s".into(),
                turn_token: String::new(),
                turn_claim_id: None,
                cancel_token: cancel.clone(),
                code_citations: Vec::new(),
                served: Default::default(),
            };
            let opened = open_turn(
                &ctx,
                MEMBER.into(),
                "agent".into(),
                progress,
                cancel,
                inject_tx,
                Some(PATIENCE),
                ctx.turns.admit(MEMBER).unwrap(),
            )
            .await;
            (ctx, opened, data)
        }

        fn turn_id(result: &ToolResult) -> &str {
            result.output["turn_id"].as_str().expect("an open turn")
        }

        fn approvals(result: &ToolResult) -> Vec<serde_json::Value> {
            result.output["approvals"]
                .as_array()
                .cloned()
                .unwrap_or_default()
        }

        async fn decide(
            ctx: &Arc<AgentToolsContext>,
            turn_id: &str,
            request_id: &str,
            choice: &str,
        ) -> ToolResult {
            let decide = approval_tools(Arc::clone(ctx))
                .into_iter()
                .find(|tool| tool.name() == "approval.decide")
                .expect("approval.decide");
            as_member(decide.execute(serde_json::json!({
                "request_id": request_id,
                "choice": choice,
                "turn_id": turn_id,
                "wait_seconds": PATIENCE,
            })))
            .await
            .expect("approval.decide answers")
        }

        #[tokio::test]
        async fn approving_a_turn_prompt_runs_the_call_once_and_the_turn_completes() {
            let tool = Arc::new(RecordingTool::default());
            let model = ScriptedModel::new([serde_json::json!({})], entry("figma"), 4);
            let (ctx, opened, _data) = open_gated_chat(model, &tool).await;
            let prompts = approvals(&opened);
            assert_eq!(prompts.len(), 1, "{}", opened.output);
            assert_eq!(prompts[0]["detail"], "figma", "{}", opened.output);

            let request_id = prompts[0]["request_id"].as_str().unwrap();
            let decided = decide(&ctx, turn_id(&opened), request_id, "allow_once").await;
            assert_eq!(decided.output["status"], "done", "{}", decided.output);
            assert_eq!(tool.runs(), [entry("figma")]);
        }

        /// A turn's prompt is shown, and answered, only while its agent is
        /// one the caller may reach: once its policy shuts them out, the
        /// prompt reads as one on an agent that isn't there.
        #[tokio::test]
        async fn a_turn_prompt_is_neither_shown_nor_answered_once_its_agent_shuts_the_caller_out() {
            let tool = Arc::new(RecordingTool::default());
            let model = ScriptedModel::new([serde_json::json!({})], entry("figma"), 4);
            let (ctx, opened, _data) = open_gated_chat(model, &tool).await;
            let prompts = approvals(&opened);
            assert_eq!(prompts.len(), 1, "{}", opened.output);
            let request_id = prompts[0]["request_id"].as_str().unwrap();
            ctx.gateway.access_gate.upsert(
                "agent".into(),
                workforce_common::settings::AgentAccessConfig {
                    mode: workforce_common::settings::AccessMode::Allowlist,
                    allow: vec!["someone-else".into()],
                    ..Default::default()
                },
            );
            let status = approval_tools(Arc::clone(&ctx))
                .into_iter()
                .find(|tool| tool.name() == "approval.status")
                .expect("approval.status");
            let shown = as_member(status.execute(serde_json::json!({
                "request_id": request_id,
                "turn_id": turn_id(&opened),
            })))
            .await
            .expect("approval.status answers");
            let error = shown.error.unwrap_or_default();
            assert!(error.starts_with("unknown agent \"agent\""), "{error}");
            let decided = decide(&ctx, turn_id(&opened), request_id, "allow_once").await;
            assert!(!decided.success, "{}", decided.output);
            assert!(tool.runs().is_empty());
        }

        #[tokio::test]
        async fn denying_a_turn_prompt_completes_the_turn_without_a_new_prompt() {
            let tool = Arc::new(RecordingTool::default());
            let model = ScriptedModel::new([entry("figma")], entry("figma"), 4);
            let (ctx, opened, _data) = open_gated_chat(model, &tool).await;
            let prompts = approvals(&opened);
            assert_eq!(prompts.len(), 1, "{}", opened.output);

            let request_id = prompts[0]["request_id"].as_str().unwrap();
            let decided = decide(&ctx, turn_id(&opened), request_id, "deny_once").await;
            assert_eq!(decided.output["status"], "done", "{}", decided.output);
            assert!(tool.runs().is_empty());
        }

        #[tokio::test]
        async fn polling_a_turn_this_hub_does_not_hold_points_to_the_session() {
            let poll = agent_tools(test_ctx())
                .into_iter()
                .find(|tool| tool.name() == "agent.poll")
                .expect("agent.poll");
            let polled = as_member(poll.execute(serde_json::json!({ "turn_id": "gone" })))
                .await
                .expect("agent.poll answers");
            let error = polled.error.unwrap_or_default();
            assert!(error.starts_with("unknown turn"), "{error}");
            assert!(error.contains("agent.history"), "{error}");
        }

        #[tokio::test]
        async fn an_unknown_request_id_leaves_the_turn_prompt_answerable() {
            let tool = Arc::new(RecordingTool::default());
            let model = ScriptedModel::new([entry("figma")], entry("figma"), 4);
            let (ctx, opened, _data) = open_gated_chat(model, &tool).await;
            let prompts = approvals(&opened);
            assert_eq!(prompts.len(), 1, "{}", opened.output);
            let request_id = prompts[0]["request_id"].as_str().unwrap();

            let refused = decide(&ctx, turn_id(&opened), "not-a-request", "allow_once").await;
            assert_eq!(refused.error.as_deref(), Some("unknown request"));
            let turn = ctx.turns.get(turn_id(&opened), MEMBER).expect("the turn");
            assert!(turn.awaits(request_id), "the real prompt is still listed");
            assert!(tool.runs().is_empty());

            let decided = decide(&ctx, turn_id(&opened), request_id, "allow_once").await;
            assert_eq!(decided.output["status"], "done", "{}", decided.output);
            assert_eq!(tool.runs(), [entry("figma")]);
        }

        #[tokio::test]
        async fn two_prompts_in_one_turn_are_answered_independently() {
            let tool = Arc::new(RecordingTool::default());
            let model = ScriptedModel::new([entry("figma"), entry("probe")], entry("figma"), 4);
            let (ctx, opened, _data) = open_gated_chat(model, &tool).await;
            let turn = ctx.turns.get(turn_id(&opened), MEMBER).expect("the turn");
            let state = state_once(&turn, |s| s.pending.len() == 2).await;
            assert_eq!(state.pending.len(), 2, "{state:?}");
            let prompt_for = |detail: &str| {
                state
                    .pending
                    .iter()
                    .find(|p| p.detail == detail)
                    .map(|p| p.request_id.clone())
                    .expect("a prompt per call")
            };
            let (figma, probe) = (prompt_for("figma"), prompt_for("probe"));
            assert_ne!(figma, probe, "each call has its own request id");

            let denied = decide(&ctx, turn_id(&opened), &figma, "deny_once").await;
            let still: Vec<serde_json::Value> = approvals(&denied)
                .iter()
                .map(|p| p["request_id"].clone())
                .collect();
            assert_eq!(still, [serde_json::json!(probe)], "{}", denied.output);

            let decided = decide(&ctx, turn_id(&opened), &probe, "allow_once").await;
            assert_eq!(decided.output["status"], "done", "{}", decided.output);
            assert_eq!(tool.runs(), [entry("probe")]);
        }
    }

    #[tokio::test]
    async fn tools_fail_closed_without_a_caller() {
        let ctx = test_ctx();
        for tool in agent_tools(Arc::clone(&ctx))
            .into_iter()
            .chain(approval_tools(ctx))
        {
            let input = serde_json::json!({
                "message": "hi", "turn_id": "t", "request_id": "r", "choice": "allow_once"
            });
            let result = tool.execute(input).await.unwrap();
            assert!(
                !result.success,
                "{} must refuse without a caller",
                tool.name()
            );
            assert_eq!(result.error.as_deref(), Some("no caller identity in scope"));
        }
    }

    mod reach {
        use super::*;
        use crate::remote_mcp::testing::{fixture, with_agents};
        use workforce_common::settings::{AccessMode, AgentAccessConfig};

        /// A hub with the default agent and `private`, which admits alice
        /// only.
        async fn hub(data_dir: &std::path::Path) -> Arc<AgentToolsContext> {
            let fx = fixture(true);
            with_agents(&fx, data_dir, &["default", "private"]).await;
            fx.state.access_gate.upsert(
                "private".into(),
                AgentAccessConfig {
                    mode: AccessMode::Allowlist,
                    allow: vec!["alice".into()],
                    ..Default::default()
                },
            );
            ctx_on(&fx)
        }

        async fn call(
            ctx: &Arc<AgentToolsContext>,
            who: &str,
            tool: &str,
            input: serde_json::Value,
        ) -> ToolResult {
            let tool = agent_tools(Arc::clone(ctx))
                .into_iter()
                .find(|t| t.name() == tool)
                .expect("the tool");
            CURRENT_USER_ID
                .scope(who.to_string(), tool.execute(input))
                .await
                .expect("the tool answers")
        }

        fn listed(result: &ToolResult) -> Vec<String> {
            result.output["agents"]
                .as_array()
                .unwrap()
                .iter()
                .map(|a| a["id"].as_str().unwrap().to_string())
                .collect()
        }

        #[tokio::test]
        async fn the_list_shows_only_agents_whose_policy_admits_the_caller() {
            let tmp = tempfile::tempdir().unwrap();
            let ctx = hub(tmp.path()).await;
            let all = call(&ctx, "alice", "agent.list", serde_json::json!({})).await;
            assert_eq!(listed(&all), ["default", "private"]);
            let some = call(&ctx, "bob", "agent.list", serde_json::json!({})).await;
            assert_eq!(listed(&some), ["default"]);
        }

        #[tokio::test]
        async fn chat_and_history_refuse_an_agent_the_caller_may_not_use_or_that_is_not_there() {
            let tmp = tempfile::tempdir().unwrap();
            let ctx = hub(tmp.path()).await;
            for (tool, agent) in [
                ("agent.chat", "private"),
                ("agent.history", "private"),
                ("agent.chat", "PRIVATE"),
                ("agent.chat", "privat"),
                ("agent.history", "removed"),
            ] {
                let input = serde_json::json!({ "message": "hi", "agent_id": agent });
                let refused = call(&ctx, "bob", tool, input).await;
                assert_eq!(
                    refused.error.as_deref(),
                    Some(
                        format!(
                            "unknown agent {agent:?}; call agent.list for the agents you can reach on this hub"
                        )
                        .as_str()
                    ),
                    "{tool} {agent}"
                );
            }
            let default = ctx.gateway.registry.default_agent().unwrap();
            assert!(
                default
                    .sessions
                    .get_or_create(&session_channel("bob"), "bob", default.agent_id.as_str())
                    .unwrap()
                    .messages
                    .iter()
                    .all(|m| m.role != MessageRole::User),
                "nothing reached the default agent"
            );
            let history = call(
                &ctx,
                "alice",
                "agent.history",
                serde_json::json!({ "agent_id": "private" }),
            )
            .await;
            assert!(history.success, "{:?}", history.error);
        }

        #[tokio::test]
        async fn a_chat_past_the_running_cap_is_refused_before_it_starts() {
            let tmp = tempfile::tempdir().unwrap();
            let ctx = hub(tmp.path()).await;
            let _held: Vec<TurnSlot> = (0..TURNS_PER_IDENTITY)
                .map(|_| ctx.turns.admit("alice").unwrap())
                .collect();
            let refused = call(
                &ctx,
                "alice",
                "agent.chat",
                serde_json::json!({ "message": "hi" }),
            )
            .await;
            let error = refused.error.unwrap_or_default();
            assert!(error.starts_with("too many turns running"), "{error}");
            assert_eq!(ctx.turns.len(), 0, "no turn was started or kept");
        }
    }
}
