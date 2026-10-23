//! The one gated executor behind every MCP surface the hub serves: the rounds
//! of the CLIs it spawns and the remote MCP external clients reach. A call
//! runs through the same policy → capability → cascade → secret-marker →
//! sentinel → access chain the agent runner applies, inside the scope of the
//! principal it is made for, and its result is tokenised before it leaves.
//!
//! What differs is how a call that needs a person's approval is held:
//! remote MCP parks it for the caller to decide with `approval.decide`
//! ([`ApprovalMode::Park`]); a CLI round holds the call open on its turn's
//! approval ledger until the person answers ([`ApprovalMode::Hold`]).

use std::sync::Arc;
use std::time::{Duration, Instant};
use workforce_agent::runner::AgentRunner;

use async_trait::async_trait;
use parking_lot::Mutex;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::info;
use uuid::Uuid;

use workforce_agent::llm::types::ContentBlock;
use workforce_agent::runner::{
    approval_detail, approval_route, await_approval, deliver_generated, loaded_result,
    render_viewed, route_approval, scope_turn, ApprovalAsk, ApprovalDecision, ApprovalRouter,
    PermissionWriter, ProgressEvent, RouteResolution, SessionContext, ToolInvocation,
    UNREACHABLE_APPROVER,
};
use workforce_agent::security::external_content::tokenize_for_egress;
use workforce_agent::spill::{spill_tool_output, RetrievalSurface};
use workforce_agent::tools::access::ToolAccessSnapshot;
use workforce_agent::tools::approval::{GatedCall, Refusal, TurnApprovals};
use workforce_agent::tools::dispatch::{
    execute_approved, execute_gated, gate_tool_call, prepare_allowed, GateOutcome, ToolExecution,
    ToolGates,
};
use workforce_agent::tools::image_gen::ImageSink;
use workforce_agent::tools::image_view::ImageViewSink;
use workforce_agent::tools::registry::ToolRegistry;
use workforce_agent::tools::{Tool, ToolResult};
use workforce_agent::turn_journal::{ToolCall, TurnJournal};
use workforce_common::session::SpillConfig;
use workforce_core::policy::{PermissionChoice, PermissionPromptOptions};
use workforce_policy::engine::PolicyEngine;

use crate::approvals::slack::hand_off;
use crate::ephemeral::{Ephemeral, EphemeralRegistry};
use crate::hosts::RoutedApproval;
use crate::mcp::{result_text, McpContent, McpReply, Surface, ToolCallExecutor};
use crate::serve::AgentService;
use crate::subscription::pools::TurnSlot;

const PENDING_CALLS_PER_IDENTITY: usize = 8;
const PENDING_CALL_TTL: Duration = Duration::from_secs(600);

/// What a held call answers when its round ended before it did.
pub(crate) const ROUND_ENDED: &str =
    "The turn this call belonged to has ended, so it did not finish.";

pub const DECIDE_NEXT: &str =
    "Ask the user whether to allow this, then call approval.decide with the request_id and their choice";
pub const ROUTED_NEXT: &str =
    "Designated approvers have been asked in Slack; approval.status reports their decision";

pub fn choice_name(choice: PermissionChoice) -> &'static str {
    match choice {
        PermissionChoice::AllowOnce => "allow_once",
        PermissionChoice::DenyOnce => "deny_once",
        PermissionChoice::AllowAlways => "allow_always",
        PermissionChoice::DenyAlways => "deny_always",
    }
}

/// Whether a prompt offers `choice`; an `always_ask` rule withholds
/// `allow_always`, and a caller must not be able to type it back in.
pub fn offered(options: &PermissionPromptOptions, choice: PermissionChoice) -> bool {
    match choice {
        PermissionChoice::AllowOnce => options.allow_once,
        PermissionChoice::DenyOnce => options.deny_once,
        PermissionChoice::AllowAlways => options.allow_always,
        PermissionChoice::DenyAlways => options.deny_always,
    }
}

/// The choices a prompt offers, as `approval.decide` accepts them.
pub fn choice_names(options: &PermissionPromptOptions) -> Vec<&'static str> {
    [
        PermissionChoice::AllowOnce,
        PermissionChoice::DenyOnce,
        PermissionChoice::AllowAlways,
        PermissionChoice::DenyAlways,
    ]
    .into_iter()
    .filter(|choice| offered(options, *choice))
    .map(choice_name)
    .collect()
}

/// What a call answers when a rule routes its approval but nothing could
/// resume it once the approvers decide.
fn unresumable(approvers: &[String], agent_id: &str) -> String {
    format!(
        "This action needs approval from {}. Ask for it in a normal conversation with {agent_id}, \
         where the approval can resume the action.",
        approvers.join(", ")
    )
}

/// Shape a pending outcome for the caller; the route adds its own fields.
fn pending_payload(
    route: &str,
    request_id: &str,
    tool: &str,
    detail: &str,
    reason: &str,
    expires_in_secs: u64,
    next: &str,
) -> serde_json::Value {
    serde_json::json!({
        "status": "approval_pending",
        "route": route,
        "request_id": request_id,
        "tool": tool,
        "detail": detail,
        "reason": reason,
        "expires_in_secs": expires_in_secs,
        "next": next,
    })
}

/// Where a call came from, as its audit line names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CallOrigin {
    /// An external client under a remote MCP token.
    RemoteToken(Uuid),
    /// A CLI round the hub spawned.
    Round,
}

#[derive(Clone)]
pub struct CallAudit {
    pub origin: CallOrigin,
    pub identity_id: String,
    pub agent_id: String,
}

impl CallAudit {
    pub fn record(&self, tool: &str, decision: &str, reason: Option<&str>) {
        let reason = reason.unwrap_or("");
        match self.origin {
            CallOrigin::RemoteToken(token) => info!(
                target: "remote_mcp.audit",
                token = %token,
                identity = %self.identity_id,
                agent = %self.agent_id,
                tool,
                decision,
                reason,
                "remote MCP tool call"
            ),
            CallOrigin::Round => info!(
                target: "mcp.audit",
                identity = %self.identity_id,
                agent = %self.agent_id,
                tool,
                decision,
                reason,
                "round MCP tool call"
            ),
        }
    }
}

/// Where a call runs: the agent's gates, tool surface and access snapshot,
/// as the surface was bound.
#[derive(Clone)]
pub struct CallSite {
    pub gates: ToolGates,
    pub policy: Arc<PolicyEngine>,
    pub surface: Surface,
    pub access: ToolAccessSnapshot,
    pub spill: SpillConfig,
    pub audit: CallAudit,
}

impl CallSite {
    /// Audit a settled call under the decision its record carries, and shape
    /// its result for the MCP client from `tools`, the surface it ran on.
    fn report(&self, tools: &ToolRegistry, name: &str, held: Held) -> ToolResult {
        let Held { exec, decision } = held;
        self.audit
            .record(name, decision, exec.deny_reason.as_deref());
        self.shape(tools, name, exec)
    }

    /// A settled call's result as the MCP client receives it. The text is
    /// tokenised on both branches: a credential in a result or an error
    /// becomes a marker under the scoped session, or is redacted when there
    /// is none.
    fn shape(&self, tools: &ToolRegistry, name: &str, exec: ToolExecution) -> ToolResult {
        if exec.is_error {
            return ToolResult::failure(
                tokenize_for_egress(&exec.output_text, name),
                exec.duration_ms,
            );
        }
        let (_, output) = spill_tool_output(
            name,
            exec.output_text,
            exec.output,
            false,
            &self.spill,
            RetrievalSurface::from_registry(tools),
        );
        ToolResult::success(tokenised(output, name), exec.duration_ms)
    }
}

/// `output` with every credential in it tokenised, kept structured when there
/// was none.
fn tokenised(output: serde_json::Value, source: &str) -> serde_json::Value {
    let text = result_text(output.clone());
    let safe = tokenize_for_egress(&text, source);
    match safe == text {
        true => output,
        false => serde_json::Value::String(safe),
    }
}

/// A direct tool call parked on a self-serve approval, holding everything
/// its execution needs once the caller decides.
pub struct PendingCall {
    pub owner: String,
    pub tool_name: String,
    pub input: serde_json::Value,
    pub detail: String,
    pub reason: String,
    pub options: PermissionPromptOptions,
    raised: Instant,
    site: CallSite,
}

impl PendingCall {
    fn new(
        tools: &ToolRegistry,
        tool_name: String,
        input: serde_json::Value,
        reason: String,
        options: PermissionPromptOptions,
        site: CallSite,
    ) -> Self {
        Self {
            owner: site.audit.identity_id.clone(),
            detail: approval_detail(tools, &tool_name, &input),
            tool_name,
            input,
            reason,
            options,
            raised: Instant::now(),
            site,
        }
    }

    pub fn agent_id(&self) -> &str {
        &self.site.audit.agent_id
    }

    pub fn expires_in_secs(&self) -> u64 {
        PENDING_CALL_TTL
            .saturating_sub(self.raised.elapsed())
            .as_secs()
    }

    pub fn payload(&self, request_id: &str) -> serde_json::Value {
        let mut payload = pending_payload(
            "self",
            request_id,
            &self.tool_name,
            &self.detail,
            &self.reason,
            self.expires_in_secs(),
            DECIDE_NEXT,
        );
        payload["options"] = choice_names(&self.options).into();
        payload
    }

    /// Run the parked call as the caller. The gate is consulted again so a
    /// capability or rule withdrawn since parking still holds, with the
    /// human's approval standing in for the prompt; secret markers are
    /// substituted and parameters scanned now, so cleartext never waits in
    /// memory.
    pub async fn run(&self) -> ToolResult {
        let site = &self.site;
        let tools = site.surface.now();
        let exec = execute_approved(
            &site.gates,
            &site.policy,
            &tools,
            &site.access,
            &self.tool_name,
            &self.input,
        )
        .await;
        site.report(&tools, &self.tool_name, Held::ran(exec))
    }
}

impl Ephemeral for PendingCall {
    fn owner(&self) -> &str {
        &self.owner
    }

    fn started(&self) -> Instant {
        self.raised
    }
}

pub type PendingCalls = EphemeralRegistry<PendingCall>;

impl Default for PendingCalls {
    fn default() -> Self {
        Self::bounded(PENDING_CALLS_PER_IDENTITY, PENDING_CALL_TTL)
    }
}

/// Whether the call a routed approval holds can be resumed once its
/// approvers decide.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RouteResume {
    /// In the session the call was made in: a runner turn's, or a remote
    /// caller's. The decision resumes it there.
    InSession,
    /// Nowhere: the round that asked has no runner turn to resume, so a
    /// call that routes is refused and nothing is written.
    Nowhere,
}

/// Where a round's media goes: the sink that renders files its calls stage
/// for viewing, and the one that delivers the files they generate.
#[derive(Clone)]
pub struct MediaSinks {
    pub view: Arc<dyn ImageViewSink>,
    pub deliver: Arc<dyn ImageSink>,
}

/// Everything a round's calls run under, captured once at bind.
#[derive(Clone)]
pub struct McpTurn {
    pub(crate) ctx: SessionContext,
    pub(crate) progress: Option<mpsc::Sender<ProgressEvent>>,
    /// The turn's approval ledger; `None` when nobody can answer a prompt.
    pub(crate) approvals: Option<Arc<TurnApprovals>>,
    pub(crate) patience: Duration,
    pub(crate) writer: Option<Arc<dyn PermissionWriter>>,
    pub(crate) routed: Option<mpsc::Sender<RoutedApproval>>,
    pub(crate) resume: RouteResume,
    pub(crate) media: MediaSinks,
    /// The model a viewed file is rendered for.
    pub(crate) model: String,
    pub(crate) journal: Option<Arc<dyn TurnJournal>>,
    /// The agent turn's pool permit, given back while a call waits.
    pub(crate) park: Option<TurnSlot>,
    /// Whether each call is reported on `progress` as the turn's own tool
    /// activity: the round is an agent's turn, whose CLI reports none.
    pub(crate) reports: bool,
    /// Ends every call the round still holds when the round ends.
    pub(crate) cancel: CancellationToken,
    pub(crate) invocations: Arc<Mutex<Vec<ToolInvocation>>>,
}

impl McpTurn {
    fn record(&self, invocation: ToolInvocation) {
        self.invocations.lock().push(invocation);
    }

    /// `wait`, run with the turn's pool permit given back when it has one.
    async fn parked<F: std::future::Future>(&self, wait: F) -> F::Output {
        match &self.park {
            Some(slot) => slot.parked(wait).await,
            None => wait.await,
        }
    }

    /// Tell the turn's progress about a call, when the round reports its
    /// calls.
    async fn report(&self, event: impl FnOnce() -> ProgressEvent) {
        if let (true, Some(progress)) = (self.reports, &self.progress) {
            let _ = progress.send(event()).await;
        }
    }

    /// Deliver the media `invocations` generated, and the items the files
    /// they staged for viewing render to.
    async fn media(&self, invocations: &[ToolInvocation], ctx: &SessionContext) -> Vec<McpContent> {
        deliver_generated(self.media.deliver.as_ref(), ctx, invocations).await;
        mcp_items(render_viewed(self.media.view.as_ref(), ctx, invocations, &self.model).await)
    }
}

/// How a call that needs a person's approval waits for one.
pub enum ApprovalMode {
    /// Parked for the remote caller to decide with `approval.decide`, whose
    /// own tools (`bridge`) are the only ones exempt from the gate.
    Park {
        pending: Arc<PendingCalls>,
        routed: mpsc::Sender<RoutedApproval>,
        bridge: Arc<[Arc<dyn Tool>]>,
    },
    /// Held open on the round's turn until the person answers, or refused
    /// when nobody can; every call still held ends with `ends`.
    Hold {
        turn: Arc<McpTurn>,
        ends: CancellationToken,
    },
}

/// Runs a resolved tool exactly as the agent runner would for this principal,
/// inside its session scope.
pub struct GatedExecutor {
    pub site: CallSite,
    pub ctx: SessionContext,
    pub router: Option<Arc<dyn ApprovalRouter>>,
    pub mode: ApprovalMode,
}

/// A settled call's execution and the decision its record carries.
struct Held {
    exec: ToolExecution,
    decision: &'static str,
}

impl Held {
    fn ran(exec: ToolExecution) -> Self {
        let decision = exec.decision();
        Self { exec, decision }
    }

    /// Refused while it waited on a person's approval.
    fn refused(message: String, reason: String) -> Self {
        Self {
            exec: ToolExecution::blocked(message, reason),
            decision: "require_approval",
        }
    }
}

/// Where a gated call stands once the gate and the router have spoken.
enum Gated {
    Settled(Held),
    Routed {
        approval: RoutedApproval,
        ttl: Duration,
    },
    Ask {
        reason: String,
        options: PermissionPromptOptions,
    },
}

impl GatedExecutor {
    /// The approval bridge's own tools are not gated: they only act on calls
    /// that were already gated, and gating them on approval would need an
    /// approval to give an approval. Only the instances the bridge built
    /// qualify, so a skill that shares their names is gated like any other.
    fn bridged(&self, tool: &Arc<dyn Tool>) -> bool {
        match &self.mode {
            ApprovalMode::Park { bridge, .. } => bridge.iter().any(|own| Arc::ptr_eq(own, tool)),
            ApprovalMode::Hold { .. } => false,
        }
    }

    fn resume(&self) -> RouteResume {
        match &self.mode {
            ApprovalMode::Park { .. } => RouteResume::InSession,
            ApprovalMode::Hold { turn, .. } => turn.resume,
        }
    }

    /// Gate the call on `tools`, run it when the gate allows, and route it
    /// when it needs a person's approval.
    async fn gate_and_route(
        &self,
        tools: &ToolRegistry,
        tool: &Arc<dyn Tool>,
        name: &str,
        input: &serde_json::Value,
    ) -> Gated {
        let site = &self.site;
        let outcome = match self.bridged(tool) {
            true => prepare_allowed(&site.gates, name, input),
            false => gate_tool_call(&site.gates, &site.policy, tools, name, input),
        };
        match outcome {
            GateOutcome::Blocked {
                message, reason, ..
            } => Gated::Settled(Held::ran(ToolExecution::blocked(message, reason))),
            GateOutcome::Execute { exec_input, .. } => Gated::Settled(Held::ran(
                execute_gated(tools, &site.access, name, input, exec_input).await,
            )),
            GateOutcome::RequireApproval {
                reason,
                allow_save_allow,
            } => self.route(
                tools,
                name,
                input,
                reason,
                PermissionPromptOptions::ask(allow_save_allow),
            ),
        }
    }

    /// Route a call that needs approval the way the runner would: to
    /// designated approvers when a rule says so, else to the person asking.
    fn route(
        &self,
        tools: &ToolRegistry,
        name: &str,
        input: &serde_json::Value,
        reason: String,
        options: PermissionPromptOptions,
    ) -> Gated {
        let router = self.router.as_deref();
        let agent_id = &self.site.audit.agent_id;
        let unreachable = || {
            Gated::Settled(Held {
                exec: ToolExecution::blocked(
                    UNREACHABLE_APPROVER.to_string(),
                    UNREACHABLE_APPROVER.to_string(),
                ),
                decision: "deny",
            })
        };
        match self.resume() {
            RouteResume::Nowhere => match approval_route(router, agent_id, name) {
                ApprovalDecision::Routed(route) => {
                    let refusal = unresumable(&route.approver_user_ids, agent_id);
                    Gated::Settled(Held::refused(refusal.clone(), refusal))
                }
                ApprovalDecision::DenyUnreachable => unreachable(),
                ApprovalDecision::SelfServe => Gated::Ask { reason, options },
            },
            RouteResume::InSession => {
                match route_approval(router, agent_id, name, &reason, input) {
                    RouteResolution::Routed {
                        request_id,
                        approver_user_ids,
                        ttl,
                    } => Gated::Routed {
                        approval: RoutedApproval {
                            agent_id: agent_id.clone(),
                            request_id,
                            tool_name: name.to_string(),
                            detail: approval_detail(tools, name, input),
                            reason,
                            options,
                            approver_user_ids,
                            requester_user_id: self.site.audit.identity_id.clone(),
                            channel: self.ctx.channel_id.clone(),
                            thread: self.ctx.thread_id.clone(),
                        },
                        ttl,
                    },
                    RouteResolution::DenyUnreachable => unreachable(),
                    RouteResolution::SelfServe => Gated::Ask { reason, options },
                }
            }
        }
    }

    // ── Park ────────────────────────────────────────────────────────────

    /// Run a remote call, parking one that needs approval: with its
    /// designated approvers when a rule routes it, else for the caller to
    /// decide.
    async fn run_parked(
        &self,
        tool: &Arc<dyn Tool>,
        name: &str,
        input: serde_json::Value,
        pending: &Arc<PendingCalls>,
        routed: &mpsc::Sender<RoutedApproval>,
    ) -> ToolResult {
        let site = &self.site;
        let tools = site.surface.now();
        match self.gate_and_route(&tools, tool, name, &input).await {
            Gated::Settled(held) => site.report(&tools, name, held),
            Gated::Routed { approval, ttl } => {
                site.audit
                    .record(name, "require_approval", Some(&approval.reason));
                let mut payload = pending_payload(
                    "routed",
                    &approval.request_id,
                    &approval.tool_name,
                    &approval.detail,
                    &approval.reason,
                    ttl.as_secs(),
                    ROUTED_NEXT,
                );
                payload["approvers"] = approval.approver_user_ids.clone().into();
                hand_off(routed, approval);
                ToolResult::success(payload, 0)
            }
            Gated::Ask { reason, options } => {
                site.audit.record(name, "require_approval", Some(&reason));
                let call = PendingCall::new(
                    &tools,
                    name.to_string(),
                    input,
                    reason,
                    options,
                    site.clone(),
                );
                let request_id = Uuid::new_v4().to_string();
                let payload = call.payload(&request_id);
                pending.insert_as(request_id, Arc::new(call));
                ToolResult::success(payload, 0)
            }
        }
    }

    // ── Hold ────────────────────────────────────────────────────────────

    /// Run one call of `turn`'s round: gated, held on the turn's ledger when
    /// it needs approval, and abandoned when the round ends first. Everything
    /// the call does — its result tokenised, its media delivered — happens in
    /// the round's session, under a call of its own.
    async fn run_held(
        &self,
        turn: &McpTurn,
        ends: &CancellationToken,
        tool: &Arc<dyn Tool>,
        name: &str,
        input: serde_json::Value,
    ) -> McpReply {
        let call = ToolCall::new(turn.journal.clone(), input.clone());
        // Boxed before the scopes wrap it, so each wrapper holds a pointer
        // rather than another copy of the whole call on the poll stack.
        let held = Box::pin(async {
            let started = Instant::now();
            let tools = self.site.surface.now();
            turn.report(|| ProgressEvent::ToolStart {
                name: name.to_string(),
                round: 0,
                detail: approval_detail(&tools, name, &input),
            })
            .await;
            let held = tokio::select! {
                held = turn.parked(self.hold(turn, &tools, tool, name, &input)) => held,
                _ = ends.cancelled() => return McpReply::text(ROUND_ENDED, true),
            };
            turn.report(|| ProgressEvent::ToolEnd {
                name: name.to_string(),
                success: held.exec.success,
                duration_ms: started.elapsed().as_millis() as u64,
                round: 0,
            })
            .await;
            let invocation = ToolInvocation {
                tool_name: name.to_string(),
                input,
                output: held.exec.output.clone(),
                success: held.exec.success,
                policy_decision: held.decision.to_string(),
                duration_ms: started.elapsed().as_millis() as u64,
                denial_reason: held.exec.deny_reason.clone(),
                error: (!held.exec.success).then(|| held.exec.output_text.clone()),
            };
            if let Some(journal) = &turn.journal {
                journal.finished(call.id(), &invocation).await;
            }
            let loads = held.exec.loads.clone();
            let reply = self.with_loads(
                McpReply::from_result(self.site.report(&tools, name, held)),
                &loads,
            );
            let media = turn
                .media(std::slice::from_ref(&invocation), &self.ctx)
                .await;
            turn.record(invocation);
            McpReply {
                content: reply.content.into_iter().chain(media).collect(),
                ..reply
            }
        });
        self.ctx
            .clone()
            .scope(call.clone().scope(scope_turn(
                turn.progress.clone(),
                turn.approvals.clone(),
                held,
            )))
            .await
    }

    /// `reply` followed by the definitions of the tools its call made
    /// callable on this surface, which the client then reaches through
    /// `tool.call`. Read after the call, which is what registered them.
    fn with_loads(&self, reply: McpReply, loads: &[String]) -> McpReply {
        if loads.is_empty() {
            return reply;
        }
        let tools = self.site.surface.now();
        let reachable: Vec<String> = loads.iter().filter(|l| tools.has(l)).cloned().collect();
        match (reachable.is_empty(), reply.content.as_slice()) {
            (false, [McpContent::Text { text }]) => {
                let (text, _) = loaded_result(&tools, text.clone(), &reachable);
                McpReply::text(text, reply.is_error)
            }
            _ => reply,
        }
    }

    async fn hold(
        &self,
        turn: &McpTurn,
        tools: &ToolRegistry,
        tool: &Arc<dyn Tool>,
        name: &str,
        input: &serde_json::Value,
    ) -> Held {
        match self.gate_and_route(tools, tool, name, input).await {
            Gated::Settled(held) => held,
            Gated::Routed { approval, .. } => {
                match &turn.routed {
                    Some(routed) => hand_off(routed, approval),
                    None => tracing::warn!(
                        request = %approval.request_id,
                        agent = %approval.agent_id,
                        "routed approval has no delivery queue: the agent is in no registry; its approvers will not be messaged"
                    ),
                }
                Held {
                    exec: ToolExecution::routed(),
                    decision: "require_approval",
                }
            }
            Gated::Ask { reason, options } => {
                self.ask(turn, tools, name, input, reason, options).await
            }
        }
    }

    /// Put a self-serve prompt to the person on the turn's card and run the
    /// call once they allow it; with no ledger, nobody could answer.
    async fn ask(
        &self,
        turn: &McpTurn,
        tools: &ToolRegistry,
        name: &str,
        input: &serde_json::Value,
        reason: String,
        options: PermissionPromptOptions,
    ) -> Held {
        let site = &self.site;
        let refused =
            |refusal: Refusal, reason| Held::refused(refusal.tool_result().into(), reason);
        let Some(ledger) = &turn.approvals else {
            return refused(Refusal::Unattended, reason);
        };
        let call = GatedCall {
            tool: name.to_string(),
            input: input.clone(),
        };
        let prompt = match ledger.open(call) {
            Ok(prompt) => prompt,
            Err(refusal) => return refused(refusal, reason),
        };
        let detail = approval_detail(tools, name, input);
        let decision = await_approval(
            ApprovalAsk {
                progress: turn.progress.as_ref(),
                agent_id: &site.audit.agent_id,
                tool: name,
                detail: &detail,
                reason: &reason,
                round: 0,
                options,
                writer: turn.writer.as_deref(),
                requester: Some(&site.audit.identity_id),
            },
            ledger,
            prompt,
            turn.patience,
        )
        .await;
        if let Err(refusal) = decision.verdict() {
            return refused(refusal, reason);
        }
        let exec =
            execute_approved(&site.gates, &site.policy, tools, &site.access, name, input).await;
        if exec.is_error {
            ledger.close(decision.call, Refusal::Failed);
        }
        Held::ran(exec)
    }
}

#[async_trait]
impl ToolCallExecutor for GatedExecutor {
    async fn call(
        &self,
        tool: Arc<dyn Tool>,
        name: &str,
        input: serde_json::Value,
    ) -> eyre::Result<McpReply> {
        Ok(match &self.mode {
            ApprovalMode::Park {
                pending, routed, ..
            } => McpReply::from_result(
                self.ctx
                    .clone()
                    .scope(self.run_parked(&tool, name, input, pending, routed))
                    .await,
            ),
            ApprovalMode::Hold { turn, ends } => {
                self.run_held(turn, ends, &tool, name, input).await
            }
        })
    }
}

/// A viewed file's rendered blocks as the items an MCP client can show: text
/// and images as they are, and a document — which MCP carries no native item
/// for — named in text so the model knows it was there.
fn mcp_items(blocks: Vec<ContentBlock>) -> Vec<McpContent> {
    blocks
        .into_iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(McpContent::text(text)),
            ContentBlock::Image { source } => Some(McpContent::Image {
                data: source.data,
                mime_type: source.media_type,
            }),
            ContentBlock::Document { source } => Some(McpContent::text(format!(
                "A {} document was attached, but documents cannot be shown over MCP; \
                 read it with a tool that returns its text.",
                source.media_type
            ))),
            _ => None,
        })
        .collect()
}

/// Where a turn's media goes: files staged by `view_attachment` render
/// into the conversation — the sink itself honours the active model's
/// per-type capability — and every file a tool stages for delivery
/// (charts, diagrams, reports, exports, generated media, WASM skill
/// output) goes to the originating surface: a Slack upload, or a gateway
/// attachment for the web UI and iOS. Neither depends on which `generate_*`
/// tools are enabled.
pub(crate) fn media_sinks(service: &AgentService) -> crate::mcp_gate::MediaSinks {
    let view: Arc<dyn workforce_agent::tools::image_view::ImageViewSink> =
        Arc::new(crate::image_view_sink::HubImageViewSink::new(
            Arc::clone(&service.vision_tiers),
            service.describer.clone(),
            service.transcriber.clone(),
            *service.provider_kind.read(),
            service.video_frames(),
            service.workspace.clone(),
        ));
    let deliver = Arc::new(crate::image_sink::HubImageSink::new(
        Arc::clone(&service.surface_registry),
        service.agent_id.as_str(),
        service.image_broadcast.read().clone(),
    ));
    crate::mcp_gate::MediaSinks { view, deliver }
}

/// Prepare a freshly-built runner with the shared per-run concerns:
/// the current [`workforce_agent::tools::access::ToolAccess`] filter (if
/// any) and the media sinks. Centralised so every `AgentRunner::new` site
/// wires them identically. (Name kept for call-site stability; scope is
/// broader than tool access.)
pub(crate) fn install_tool_access(service: &AgentService, runner: AgentRunner) -> AgentRunner {
    let runner = match service.tool_access.read().clone() {
        Some(access) => runner.with_tool_access(access),
        None => runner,
    };
    let media = crate::mcp_gate::media_sinks(service);
    runner
        .with_image_view_sink(media.view)
        .with_image_sink(media.deliver)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn choice_names_follow_the_prompt_options() {
        assert_eq!(
            choice_names(&PermissionPromptOptions::ask(true)),
            ["allow_once", "deny_once", "allow_always", "deny_always"]
        );
        assert_eq!(
            choice_names(&PermissionPromptOptions::ask(false)),
            ["allow_once", "deny_once", "deny_always"]
        );
    }
}
