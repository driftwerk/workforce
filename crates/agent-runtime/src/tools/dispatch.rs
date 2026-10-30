//! Gated tool dispatch shared by the runner loops and out-of-turn callers
//! (the hub's remote MCP server). One place decides whether a call may run
//! and in what form, so every entry point authorises identically:
//! policy → capability gate → cascade → the tool's own approval requirement
//! → arguments against the tool's schema → secret markers → sentinel →
//! access snapshot → deadline-bounded execution.

use std::sync::Arc;

use serde_json::Value;
use tracing::{debug, info, warn};
use workforce_common::secret_marker::ConsumedSecret;
use workforce_common::security_scanner::{parameter_block, scan_parameters, ParameterBlock};
use workforce_core::policy::{PolicyContext, PolicyDecision, PolicyRequest, PolicyResponse};
use workforce_core::types::AgentId;
use workforce_policy::engine::PolicyEngine;

use crate::runner::{
    combine_with_cascade, execute_with_deadline, tool_not_found_message, CapabilityGate,
    PermissionGate, RunnerConfig, CURRENT_SESSION_KEY, CURRENT_USER_ID,
};
use crate::security::secret_sinks::substitute_secret_markers;
use crate::tools::access::ToolAccessSnapshot;
use crate::tools::arguments::{
    check_arguments, render_argument_problems, summarise_argument_problems, ArgumentValidation,
};
use crate::tools::registry::ToolRegistry;
use crate::tools::{Destination, RateLimit, SurfaceEffect};
use workforce_common::channels::CHAT_SURFACE_PLATFORMS;

/// The writes each person has said an agent may always make from their
/// Desk turns without asking, each one tool's to one destination: the
/// surface gate's own allow list, kept apart from the permission system so
/// it holds with that system off.
pub trait SurfaceAllows: Send + Sync {
    fn allows(&self, person: &str, agent: &str, tool: &str, to: &Destination) -> bool;
    fn allow(&self, person: &str, agent: &str, tool: &str, to: &Destination) -> eyre::Result<()>;
}

/// The authorisation inputs a tool call is gated with, detached from the
/// rest of [`RunnerConfig`] so callers without a runner can gate too.
#[derive(Clone)]
pub struct ToolGates {
    pub agent_id: AgentId,
    pub team_id: Option<String>,
    pub role: Option<String>,
    pub capability_gate: Option<Arc<dyn CapabilityGate>>,
    pub permission_gate: Option<Arc<dyn PermissionGate>>,
    pub surface_allows: Option<Arc<dyn SurfaceAllows>>,
    pub arguments: ArgumentValidation,
}

impl From<&RunnerConfig> for ToolGates {
    fn from(config: &RunnerConfig) -> Self {
        Self {
            agent_id: config.agent_id.clone(),
            team_id: config.team_id.clone(),
            role: config.role.clone(),
            capability_gate: config.capability_gate.clone(),
            permission_gate: config.permission_gate.clone(),
            surface_allows: config.surface_allows.clone(),
            arguments: config.arguments,
        }
    }
}

/// What the gates decided for one call. The caller's own copy of the input
/// is the record form (markers intact) in every variant; only
/// `Execute::exec_input` may carry cleartext, and it goes nowhere but the
/// tool call. `consumed` is reported once substitution succeeded — a
/// sentinel block still surfaces it, an unknown-marker block never does.
#[derive(Debug)]
pub enum GateOutcome {
    Execute {
        exec_input: Option<Value>,
        consumed: Vec<ConsumedSecret>,
    },
    Blocked {
        message: String,
        reason: String,
        consumed: Vec<ConsumedSecret>,
    },
    RequireApproval {
        reason: String,
        allow_save_allow: bool,
        /// The destination of a write the surface gate holds: "Always
        /// allow" keeps it on the person's allow list, not as a rule.
        holds: Option<Destination>,
    },
}

/// Run every pre-execution check the runner applies to a model-emitted tool
/// call, in the runner's order.
pub fn gate_tool_call(
    gates: &ToolGates,
    policy: &PolicyEngine,
    tools: &ToolRegistry,
    tool_name: &str,
    input: &Value,
) -> GateOutcome {
    let request = PolicyRequest {
        agent_id: gates.agent_id.clone(),
        tool_name: tool_name.to_string(),
        parameters: input.clone(),
        context: PolicyContext {
            node_id: None,
            team_id: gates.team_id.clone(),
            task_id: None,
            role: gates.role.clone(),
        },
    };
    let response = policy.evaluate(&request);
    let response = apply_capability_gate(gates, response, tool_name, input);
    let response = apply_cascade_gate(gates, response, tool_name);
    let response = apply_tool_requirement(tools, response, tool_name);
    let effect = surface_effect_of(tools, tool_name, input);
    let to = effect.as_ref().and_then(SurfaceEffect::destination);
    let mut held = None;
    let response = apply_surface_gate(
        response,
        effect.as_ref(),
        crate::runner::current_desk().as_ref(),
        input,
        || {
            *held.insert(kept(
                gates.surface_allows.as_deref(),
                current_user_id().as_deref(),
                gates.agent_id.as_str(),
                tool_name,
                to.as_ref(),
            ))
        },
    );
    let holds = to.filter(|_| held == Some(Kept::NotYet));
    if let PolicyDecision::Deny { reason } = response.decision {
        return GateOutcome::Blocked {
            message: format!("Policy denied: {reason}"),
            reason,
            consumed: Vec::new(),
        };
    }
    if let Some(blocked) = argument_gate(
        gates.arguments,
        gates.agent_id.as_str(),
        tools,
        tool_name,
        input,
    ) {
        return blocked;
    }
    match response.decision {
        PolicyDecision::RequireApproval {
            reason,
            allow_save_allow,
        } => match tools
            .get(tool_name)
            .and_then(|tool| missing_required_fields(&tool.input_schema(), input))
        {
            None => GateOutcome::RequireApproval {
                reason,
                allow_save_allow,
                holds,
            },
            Some(missing) => incomplete_call(tool_name, &missing),
        },
        _ => prepare_allowed(gates, tool_name, input),
    }
}

/// The outcome for a call whose arguments its tool's schema rejects, when
/// `mode` enforces; `None` lets the call carry on. Shadow logs what enforcing
/// would have done. A rejection carries the tool's full schema.
pub fn argument_gate(
    mode: ArgumentValidation,
    agent: &str,
    tools: &ToolRegistry,
    tool_name: &str,
    input: &Value,
) -> Option<GateOutcome> {
    if mode == ArgumentValidation::Off {
        return None;
    }
    let schema = tools.get(tool_name)?.input_schema();
    let report = check_arguments(&schema, input)?;
    if !report.unknown_keys.is_empty() {
        debug!(target: "tool_args", %agent, tool = %tool_name, keys = ?report.unknown_keys, "tool_args_unknown_keys");
    }
    if report.problems.is_empty() {
        return None;
    }
    let reason = summarise_argument_problems(&report.problems);
    if mode == ArgumentValidation::Shadow {
        info!(target: "tool_args", %agent, tool = %tool_name, %reason, "shadow: would reject tool call arguments");
        return None;
    }
    info!(target: "tool_args", %agent, tool = %tool_name, %reason, "Rejected tool call arguments");
    Some(GateOutcome::Blocked {
        message: render_argument_problems(tool_name, &report.problems, &schema),
        reason,
        consumed: Vec::new(),
    })
}

/// The fields `schema` requires that `input` leaves out, or sets to `null`
/// where the field's declared type rules `null` out. `None` when the call is
/// complete or the schema lists no required fields.
fn missing_required_fields(schema: &Value, input: &Value) -> Option<Vec<String>> {
    let missing: Vec<String> = schema
        .get("required")?
        .as_array()?
        .iter()
        .filter_map(Value::as_str)
        .filter(|field| {
            input.get(*field).is_none_or(|value| {
                value.is_null() && rules_out_null(&schema["properties"][*field])
            })
        })
        .map(str::to_string)
        .collect();
    (!missing.is_empty()).then_some(missing)
}

/// Whether a property's schema declares a type that excludes `null`. A shape
/// without a plain `type`, such as `anyOf` or `$ref`, is taken to allow it.
fn rules_out_null(property: &Value) -> bool {
    let typed = match &property["type"] {
        Value::String(kind) => kind != "null",
        Value::Array(kinds) => !kinds.iter().any(|kind| kind == "null"),
        _ => false,
    };
    typed && property["nullable"] != true
}

/// A person is only asked about a call that can run: one missing required
/// input goes back to the model to correct instead.
fn incomplete_call(tool_name: &str, missing: &[String]) -> GateOutcome {
    let fields = missing
        .iter()
        .map(|field| format!("`{field}`"))
        .collect::<Vec<_>>()
        .join(", ");
    let reason = format!("called without required input {fields}");
    GateOutcome::Blocked {
        message: format!(
            "'{tool_name}' was {reason}, so it did not run and no approval was requested. \
             Call it again with every required field."
        ),
        reason,
        consumed: Vec::new(),
    }
}

/// The execution-time half of the gate for a call the policy allowed or a
/// human approved: secret-marker substitution and the parameter scan.
pub fn prepare_allowed(gates: &ToolGates, tool_name: &str, input: &Value) -> GateOutcome {
    let session_key = CURRENT_SESSION_KEY
        .try_with(|k| k.clone())
        .unwrap_or_default();
    let sub = substitute_secret_markers(tool_name, input.clone(), &session_key);
    if !sub.unknown.is_empty() {
        let reason = sub.unknown_reason();
        warn!(target: "sentinel", agent = %gates.agent_id, tool = %tool_name, %reason, "Secret marker substitution failed");
        return GateOutcome::Blocked {
            message: format!("BLOCKED: {reason}"),
            reason,
            consumed: Vec::new(),
        };
    }
    let exec_input = sub.original.is_some().then_some(sub.input);
    let findings = scan_parameters(tool_name, input);
    if let Some(ParameterBlock { reason, message }) = parameter_block(&findings) {
        warn!(target: "sentinel", agent = %gates.agent_id, tool = %tool_name, %reason, "Parameter scan blocked tool");
        return GateOutcome::Blocked {
            message,
            reason,
            consumed: sub.consumed,
        };
    }
    if !findings.is_empty() {
        let summary: Vec<_> = findings.iter().map(|f| f.description).collect();
        info!(target: "sentinel", tool = %tool_name, findings = ?summary, "Non-critical parameter scan findings");
    }
    GateOutcome::Execute {
        exec_input,
        consumed: sub.consumed,
    }
}

/// Consult the [`CapabilityGate`] (if wired) **before** the cascade gate and
/// any approval prompt. When the gate hard-denies, the PolicyEngine response is
/// replaced with a [`PolicyDecision::Deny`] carrying the gate's reason, so the
/// runner's existing match short-circuits to the deny arm and no prompt is ever
/// emitted. Returns the unmodified response when the gate is unwired, no user is
/// in scope (system jobs / CLI — trusted, not member-gated), or the gate passes
/// the call through.
pub fn apply_capability_gate(
    gates: &ToolGates,
    response: PolicyResponse,
    tool_name: &str,
    input: &Value,
) -> PolicyResponse {
    let Some(gate) = gates.capability_gate.as_ref() else {
        return response;
    };
    let Some(user_id) = current_user_id() else {
        return response;
    };
    let Some(reason) = gate.deny_reason(&user_id, gates.agent_id.as_str(), tool_name, input) else {
        return response;
    };
    warn!(
        target: "sentinel.audit",
        user = %user_id,
        agent = %gates.agent_id.as_str(),
        tool = %tool_name,
        "capability gate denied tool (pre-dispatch)",
    );
    PolicyResponse {
        decision: PolicyDecision::Deny { reason },
        ..response
    }
}

/// Consult the cascade gate (if wired) and combine its verdict with
/// the PolicyEngine response. No-op when the gate is unwired
/// (`[permissions] enabled = false`) or no user is in scope (system
/// jobs / CLI). Returns the unmodified response in either case so the
/// runner can keep its existing match logic.
///
/// Emits a structured `tracing::info!` on the `permissions.gate`
/// target whenever the cascade tightens the PolicyEngine decision —
/// operator observability for staged rollouts.
pub fn apply_cascade_gate(
    gates: &ToolGates,
    response: PolicyResponse,
    tool_name: &str,
) -> PolicyResponse {
    let Some(gate) = gates.permission_gate.as_ref() else {
        return response;
    };
    let Some(user_id) = current_user_id() else {
        return response;
    };
    let cascade = gate.evaluate(&user_id, gates.agent_id.as_str(), tool_name);
    let before = decision_kind(&response.decision);
    let before_allow_save = decision_allow_save_allow(&response.decision);
    let combined = combine_with_cascade(response, cascade);
    let after = decision_kind(&combined.decision);
    let after_allow_save = decision_allow_save_allow(&combined.decision);
    if before != after || before_allow_save != after_allow_save {
        info!(
            target: "permissions.gate",
            user = %user_id,
            agent = %gates.agent_id.as_str(),
            tool = %tool_name,
            cascade = cascade.as_str(),
            from = before,
            to = after,
            from_allow_save = ?before_allow_save,
            to_allow_save = ?after_allow_save,
            "cascade tightened policy decision",
        );
    }
    combined
}

/// Compose a tool's own approval requirement with the decision policy, the
/// capability gate and the cascade have already reached. Total and pure, and
/// deliberately without an `Allow` arm: every arm either keeps the decision
/// or tightens it, so a tool can demand more approval than policy asks for
/// and never less.
pub fn combine_with_tool_requirement(
    response: PolicyResponse,
    requirement: Option<String>,
) -> PolicyResponse {
    use PolicyDecision as D;
    let PolicyResponse {
        decision,
        matched_rule,
        evaluation_time_us,
    } = response;
    let decision = match (decision, requirement) {
        // A denial is terminal; nothing here can reopen it.
        (d @ D::Deny { .. }, _) | (d, None) => d,
        (D::Allow, Some(reason)) => D::RequireApproval {
            reason,
            allow_save_allow: false,
        },
        // Policy already prompts: it owns the wording, and the tool's
        // requirement only withdraws the Always-Allow option — a saved
        // "allow every time" must never stand in for this decision.
        (D::RequireApproval { reason, .. }, Some(_)) => D::RequireApproval {
            reason,
            allow_save_allow: false,
        },
    };
    PolicyResponse {
        decision,
        matched_rule,
        evaluation_time_us,
    }
}

/// Apply the requirement the live registered tool declares, re-read on every
/// call so an uninstall or a rebuild that drops the gate takes effect at once.
/// Runs last in [`gate_tool_call`], and unconditionally: a system job with no
/// user in scope is gated exactly like a member's turn.
pub fn apply_tool_requirement(
    tools: &ToolRegistry,
    response: PolicyResponse,
    tool_name: &str,
) -> PolicyResponse {
    let Some(requirement) = tools
        .get(tool_name)
        .and_then(|tool| tool.approval_requirement())
    else {
        return response;
    };
    let before = decision_kind(&response.decision);
    let before_allow_save = decision_allow_save_allow(&response.decision);
    let combined = combine_with_tool_requirement(response, Some(requirement));
    let after = decision_kind(&combined.decision);
    let after_allow_save = decision_allow_save_allow(&combined.decision);
    if before != after || before_allow_save != after_allow_save {
        info!(
            target: "permissions.gate",
            tool = %tool_name,
            from = before,
            to = after,
            from_allow_save = ?before_allow_save,
            to_allow_save = ?after_allow_save,
            "tool's own approval requirement tightened the decision",
        );
    }
    combined
}

/// What a call does to a messaging surface. A tool's own declaration
/// wins; without one, any tool in the namespace of a chat platform
/// ([`CHAT_SURFACE_PLATFORMS`]) writes to it, so a Slack tool added later is
/// held at the gate until it says it only reads. Anything else touches no
/// surface.
pub fn surface_effect_of(
    tools: &ToolRegistry,
    tool_name: &str,
    input: &Value,
) -> Option<SurfaceEffect> {
    tools
        .get(tool_name)
        .and_then(|tool| tool.surface_effect(input))
        .or_else(|| {
            let namespace = tool_name.split('.').next()?;
            CHAT_SURFACE_PLATFORMS
                .contains(&namespace)
                .then(|| SurfaceEffect::Writes {
                    platform: namespace.to_string(),
                    destination: input["channel"].as_str().unwrap_or_default().to_string(),
                })
        })
}

/// The longest stretch of what a held write would say that its prompt shows.
const PREVIEW_CHARS: usize = 200;

/// What the person has said of a write's destination, for the surface gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kept {
    /// They always allow it.
    Allowed,
    /// Not yet; their "Always allow" would be kept.
    NotYet,
    /// Nothing can be kept for it: no allow list, no one to keep it for, or
    /// a write that names no destination.
    Unkeepable,
}

/// What `person` has kept on `allows` for `agent` writing to `to` with
/// `tool`.
pub fn kept(
    allows: Option<&dyn SurfaceAllows>,
    person: Option<&str>,
    agent: &str,
    tool: &str,
    to: Option<&Destination>,
) -> Kept {
    match (allows, person, to) {
        (Some(allows), Some(person), Some(to)) => match allows.allows(person, agent, tool, to) {
            true => Kept::Allowed,
            false => Kept::NotYet,
        },
        _ => Kept::Unkeepable,
    }
}

/// Hold a write to any surface but Desk, made during a Desk turn a person
/// is attending, for that person's say: an agent asked in Desk posts on
/// Slack or sends an email only when they want it to. The person's allow
/// list is the authority for these writes: a destination they always allow
/// passes, and "Always allow" is offered where it can be kept. Only ever
/// escalates an `Allow`; a run nobody attends, and any turn not from Desk,
/// is untouched. `kept` is asked only of a write the gate would hold.
pub fn apply_surface_gate(
    response: PolicyResponse,
    effect: Option<&SurfaceEffect>,
    desk: Option<&workforce_common::channels::desk::DeskScope>,
    input: &Value,
    kept: impl FnOnce() -> Kept,
) -> PolicyResponse {
    let (
        PolicyDecision::Allow,
        Some(SurfaceEffect::Writes {
            platform,
            destination,
        }),
        Some(scope),
    ) = (&response.decision, effect, desk)
    else {
        return response;
    };
    if !scope.attended || platform == workforce_common::channels::desk::DESK_PLATFORM {
        return response;
    }
    match kept() {
        Kept::Allowed => response,
        kept => {
            let preview: String = preview_of(input).chars().take(PREVIEW_CHARS).collect();
            PolicyResponse {
                decision: PolicyDecision::RequireApproval {
                    reason: format!(
                        "Posts outside Desk, to {platform} {destination}: \"{preview}\""
                    ),
                    allow_save_allow: kept == Kept::NotYet,
                },
                ..response
            }
        }
    }
}

/// What a write says, as its prompt previews it: the first of its text,
/// message or body fields (in any case: `TextBody` is Postmark's), else its
/// subject or content.
fn preview_of(input: &Value) -> &str {
    let fields = input.as_object();
    ["text", "message", "body", "textbody", "subject", "content"]
        .iter()
        .find_map(|wanted| {
            fields?
                .iter()
                .find(|(key, value)| key.eq_ignore_ascii_case(wanted) && value.is_string())
                .and_then(|(_, value)| value.as_str())
        })
        .unwrap_or_default()
}

/// The refusal a tool that requires human approval gets on a path with no
/// human on it — a workflow tool step, a job runner. `None` leaves the call
/// alone, so an ungated tool is unaffected.
pub fn unattended_refusal(tools: &ToolRegistry, tool_name: &str) -> Option<String> {
    tools
        .get(tool_name)
        .is_some_and(|tool| tool.approval_requirement().is_some())
        .then(|| {
            format!(
                "'{tool_name}' requires a human to approve every call, and nothing on this \
                 path can ask for one. Run it in a conversation with the agent, where an \
                 approver is prompted."
            )
        })
}

fn current_user_id() -> Option<String> {
    CURRENT_USER_ID
        .try_with(|u| u.clone())
        .ok()
        .filter(|u| !u.is_empty())
}

fn decision_kind(d: &PolicyDecision) -> &'static str {
    match d {
        PolicyDecision::Allow => "allow",
        PolicyDecision::Deny { .. } => "deny",
        PolicyDecision::RequireApproval { .. } => "require_approval",
    }
}

fn decision_allow_save_allow(d: &PolicyDecision) -> Option<bool> {
    match d {
        PolicyDecision::RequireApproval {
            allow_save_allow, ..
        } => Some(*allow_save_allow),
        _ => None,
    }
}

/// The record of one gated execution, in the shape the runner's transcript
/// and progress events consume. `deny_reason` is set only when the call was
/// refused before it ran; every other failure is the tool's own.
#[derive(Debug)]
pub struct ToolExecution {
    pub output_text: String,
    pub output: Value,
    pub success: bool,
    pub duration_ms: u64,
    pub is_error: bool,
    pub deny_reason: Option<String>,
    /// Tools the call made callable in full (see [`ToolResult::loads`]).
    ///
    /// [`ToolResult::loads`]: crate::tools::ToolResult::loads
    pub loads: Vec<String>,
    /// The upstream refused the call for rate limiting before executing it,
    /// so a retry cannot duplicate side effects (see [`ToolResult::rate_limited`]).
    ///
    /// [`ToolResult::rate_limited`]: crate::tools::ToolResult::rate_limited
    pub rate_limited: Option<RateLimit>,
}

impl ToolExecution {
    pub fn decision(&self) -> &'static str {
        if self.deny_reason.is_some() {
            "deny"
        } else {
            "allow"
        }
    }

    /// A call refused before it ran: the model reads `message`, the record
    /// keeps `reason`.
    pub fn blocked(message: String, reason: String) -> Self {
        Self {
            output_text: message,
            output: Value::Null,
            success: false,
            duration_ms: 0,
            is_error: true,
            deny_reason: Some(reason),
            loads: Vec::new(),
            rate_limited: None,
        }
    }

    /// A call handed to designated approvers: not run, and not an error, so
    /// the model reads that it waits on them rather than that it failed.
    pub fn routed() -> Self {
        let text = crate::runner::ROUTED_APPROVAL_TOOL_RESULT;
        Self {
            output_text: text.to_string(),
            output: Value::String(text.to_string()),
            success: false,
            duration_ms: 0,
            is_error: false,
            deny_reason: None,
            loads: Vec::new(),
            rate_limited: None,
        }
    }

    fn denied(reason: String) -> Self {
        Self::blocked(reason.clone(), reason)
    }

    /// A call that could not run for a reason of its own making.
    pub fn failed(output_text: String) -> Self {
        Self {
            output_text,
            output: Value::Null,
            success: false,
            duration_ms: 0,
            is_error: true,
            deny_reason: None,
            loads: Vec::new(),
            rate_limited: None,
        }
    }
}

/// Execute a call the gates admitted: the access snapshot may still refuse
/// it, then the tool runs under its soft deadline. Must run inside the
/// session's task-local scope.
pub async fn execute_gated(
    tools: &ToolRegistry,
    access: &ToolAccessSnapshot,
    tool_name: &str,
    input: &Value,
    exec_input: Option<Value>,
) -> ToolExecution {
    if let Some(reason) = access.block_reason(tool_name) {
        return ToolExecution::denied(reason.to_string());
    }
    let Some(tool) = tools.get(tool_name) else {
        return ToolExecution::failed(tool_not_found_message(tool_name, tools));
    };
    let call_input = exec_input.unwrap_or_else(|| input.clone());
    match execute_with_deadline(&tool, call_input).await {
        Ok(result) => {
            let (output_text, is_error) = if result.success {
                (
                    serde_json::to_string(&result.output).unwrap_or_else(|_| "{}".to_string()),
                    false,
                )
            } else {
                (
                    result.error.unwrap_or_else(|| "Tool failed".to_string()),
                    true,
                )
            };
            ToolExecution {
                output_text,
                output: result.output,
                success: result.success,
                duration_ms: result.duration_ms,
                is_error,
                deny_reason: None,
                loads: result.loads,
                rate_limited: result.rate_limited,
            }
        }
        Err(e) => ToolExecution::failed(format!("Tool error: {e}")),
    }
}

/// Whether a call to `tool_name` with `input` would run right now, without
/// prompting anyone: every check [`gate_tool_call`] and [`execute_gated`]
/// apply, minus the execution.
pub fn would_execute(
    gates: &ToolGates,
    policy: &PolicyEngine,
    tools: &ToolRegistry,
    access: &ToolAccessSnapshot,
    tool_name: &str,
    input: &Value,
) -> bool {
    access.block_reason(tool_name).is_none()
        && tools.get(tool_name).is_some()
        && matches!(
            gate_tool_call(gates, policy, tools, tool_name, input),
            GateOutcome::Execute { .. }
        )
}

/// Execute a call a person approved. The gate is consulted again, so a
/// capability or rule withdrawn while the prompt waited still holds, with the
/// approval standing in for the prompt; secret markers are substituted and
/// parameters scanned now, as for any allowed call. Must run inside the
/// session's task-local scope.
pub async fn execute_approved(
    gates: &ToolGates,
    policy: &PolicyEngine,
    tools: &ToolRegistry,
    access: &ToolAccessSnapshot,
    tool_name: &str,
    input: &Value,
) -> ToolExecution {
    let regate = ToolGates {
        arguments: ArgumentValidation::Off,
        ..gates.clone()
    };
    let gated = match gate_tool_call(&regate, policy, tools, tool_name, input) {
        GateOutcome::RequireApproval { .. } => prepare_allowed(gates, tool_name, input),
        settled => settled,
    };
    match gated {
        GateOutcome::Execute { exec_input, .. } => {
            execute_gated(tools, access, tool_name, input, exec_input).await
        }
        GateOutcome::Blocked {
            message, reason, ..
        } => ToolExecution::blocked(message, reason),
        GateOutcome::RequireApproval { reason, .. } => {
            ToolExecution::blocked(format!("Approval required: {reason}"), reason)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    use super::*;
    use crate::runner::CascadeState;
    use crate::tools::approval::testing::{RecordingTool, TOOL};
    use crate::tools::{Tool, ToolResult};

    #[derive(Default)]
    struct Calls(Mutex<Vec<&'static str>>);

    impl Calls {
        fn record(&self, name: &'static str) {
            self.0.lock().unwrap().push(name);
        }
        fn seen(&self) -> Vec<&'static str> {
            self.0.lock().unwrap().clone()
        }
    }

    struct RecordingCapabilityGate {
        calls: Arc<Calls>,
        deny: Option<&'static str>,
    }

    impl CapabilityGate for RecordingCapabilityGate {
        fn deny_reason(&self, _u: &str, _a: &str, _t: &str, _i: &Value) -> Option<String> {
            self.calls.record("capability");
            self.deny.map(str::to_string)
        }
    }

    struct RecordingPermissionGate {
        calls: Arc<Calls>,
        state: CascadeState,
    }

    impl PermissionGate for RecordingPermissionGate {
        fn evaluate(&self, _u: &str, _a: &str, _t: &str) -> CascadeState {
            self.calls.record("cascade");
            self.state
        }
    }

    fn gates(calls: &Arc<Calls>, deny: Option<&'static str>, cascade: CascadeState) -> ToolGates {
        ToolGates {
            agent_id: AgentId::new("agent"),
            team_id: None,
            role: None,
            capability_gate: Some(Arc::new(RecordingCapabilityGate {
                calls: Arc::clone(calls),
                deny,
            })),
            permission_gate: Some(Arc::new(RecordingPermissionGate {
                calls: Arc::clone(calls),
                state: cascade,
            })),
            surface_allows: None,
            arguments: ArgumentValidation::default(),
        }
    }

    fn permissive_policy() -> PolicyEngine {
        PolicyEngine::from_yaml(
            "rules:\n  - name: allow-all\n    scope: global\n    tool: \"*\"\n    action: allow\n",
        )
        .expect("policy parse")
    }

    fn as_user<T>(f: impl FnOnce() -> T) -> T {
        CURRENT_USER_ID.sync_scope("alice".to_string(), f)
    }

    #[test]
    fn gates_run_in_order_capability_then_cascade() {
        let calls = Arc::new(Calls::default());
        let g = gates(&calls, None, CascadeState::Allow);
        let out = as_user(|| {
            gate_tool_call(
                &g,
                &permissive_policy(),
                &registry(),
                "notes",
                &serde_json::json!({}),
            )
        });
        assert!(matches!(
            out,
            GateOutcome::Execute {
                exec_input: None,
                ..
            }
        ));
        assert_eq!(calls.seen(), vec!["capability", "cascade"]);
    }

    #[test]
    fn capability_deny_wins_over_the_cascade() {
        let calls = Arc::new(Calls::default());
        let g = gates(&calls, Some("members may not"), CascadeState::Allow);
        let out = as_user(|| {
            gate_tool_call(
                &g,
                &permissive_policy(),
                &registry(),
                "notes",
                &serde_json::json!({}),
            )
        });
        match out {
            GateOutcome::Blocked {
                message, reason, ..
            } => {
                assert_eq!(message, "Policy denied: members may not");
                assert_eq!(reason, "members may not");
            }
            other => panic!("expected Blocked, got {other:?}"),
        }
        assert_eq!(calls.seen(), vec!["capability", "cascade"]);
    }

    #[test]
    fn cascade_ask_becomes_require_approval() {
        let calls = Arc::new(Calls::default());
        let g = gates(&calls, None, CascadeState::AlwaysAsk);
        let out = as_user(|| {
            gate_tool_call(
                &g,
                &permissive_policy(),
                &registry(),
                "notes",
                &serde_json::json!({}),
            )
        });
        assert!(matches!(
            out,
            GateOutcome::RequireApproval {
                allow_save_allow: false,
                ..
            }
        ));
    }

    #[test]
    fn a_call_would_execute_only_when_no_gate_stops_or_asks() {
        let calls = Arc::new(Calls::default());
        let tools = registry();
        let open = ToolAccessSnapshot::default();
        let input = serde_json::json!({});
        let runs = |gates: &ToolGates, access: &ToolAccessSnapshot, tool: &str| {
            as_user(|| would_execute(gates, &permissive_policy(), &tools, access, tool, &input))
        };
        let allowed = gates(&calls, None, CascadeState::Allow);

        assert!(runs(&allowed, &open, "echo"));
        assert!(!runs(&allowed, &open, "not.registered"));
        let hidden =
            ToolAccessSnapshot::new(HashMap::from([("echo".to_string(), "hidden".to_string())]));
        assert!(!runs(&allowed, &hidden, "echo"));
        let denied = gates(&calls, Some("members may not"), CascadeState::Allow);
        assert!(!runs(&denied, &open, "echo"));
        let asks = gates(&calls, None, CascadeState::AlwaysAsk);
        assert!(!runs(&asks, &open, "echo"), "a prompt is not a yes");
    }

    #[test]
    fn unknown_secret_marker_blocks_with_the_runner_literal() {
        use workforce_common::secret_marker::{
            clear_global_cache, set_global_cache, Marker, MarkerId,
        };
        let _g = crate::security::test_support::SECRET_CACHE_TEST_LOCK
            .lock()
            .unwrap();
        clear_global_cache();
        set_global_cache(crate::security::test_support::stub_secret_cache(&[]));
        let bogus = MarkerId::fresh();
        let input = serde_json::json!({
            "path": "skills.github.token",
            "value": Marker { id: bogus.clone(), label: "GITHUB_PAT".into(), preview: "ghp_...xxxx".into() }.render(),
        });
        let calls = Arc::new(Calls::default());
        let g = gates(&calls, None, CascadeState::Allow);
        let out =
            as_user(|| gate_tool_call(&g, &permissive_policy(), &registry(), "vault.set", &input));
        clear_global_cache();
        match out {
            GateOutcome::Blocked {
                message,
                reason,
                consumed,
            } => {
                assert_eq!(message, format!("BLOCKED: {reason}"));
                assert!(reason.contains(&bogus.to_string()), "{reason}");
                assert!(consumed.is_empty());
            }
            other => panic!("expected Blocked, got {other:?}"),
        }
    }

    #[test]
    fn critical_sentinel_finding_blocks_naming_the_field_but_not_the_secret() {
        let calls = Arc::new(Calls::default());
        let g = gates(&calls, None, CascadeState::Allow);
        let input = serde_json::json!({"command": "echo sk-abcdefghijklmnopqrstuvwxyz0123"});
        let out =
            as_user(|| gate_tool_call(&g, &permissive_policy(), &registry(), "exec.run", &input));
        match out {
            GateOutcome::Blocked {
                message, reason, ..
            } => {
                assert_eq!(
                    reason,
                    "Sentinel blocked: Potential credential or API key in parameter in `command`"
                );
                assert!(
                    message.starts_with(&format!("BLOCKED by security policy: {reason}. ")),
                    "{message}"
                );
                assert!(
                    message.contains("no approval or allowlist route"),
                    "{message}"
                );
                assert!(!message.contains("sk-abc"), "{message}");
            }
            other => panic!("expected Blocked, got {other:?}"),
        }
    }

    struct FailingTool;

    #[async_trait::async_trait]
    impl Tool for FailingTool {
        fn name(&self) -> &str {
            "fails"
        }
        fn description(&self) -> &str {
            "fails"
        }
        fn input_schema(&self) -> Value {
            serde_json::json!({"type": "object"})
        }
        async fn execute(&self, _input: Value) -> eyre::Result<ToolResult> {
            Ok(ToolResult::failure("boom".to_string(), 3))
        }
    }

    /// A tool that holds itself behind a human decision, as a WASM skill tool
    /// with `requires_approval = true` does.
    struct GatedTool(Arc<AtomicUsize>);

    #[async_trait::async_trait]
    impl Tool for GatedTool {
        fn name(&self) -> &str {
            "record-keeper.apply_change"
        }
        fn description(&self) -> &str {
            "apply a change"
        }
        fn input_schema(&self) -> Value {
            serde_json::json!({"type": "object"})
        }
        fn approval_requirement(&self) -> Option<String> {
            Some("skill gate".to_string())
        }
        async fn execute(&self, input: Value) -> eyre::Result<ToolResult> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(ToolResult::success(input, 1))
        }
    }

    struct EchoTool;

    #[async_trait::async_trait]
    impl Tool for EchoTool {
        fn name(&self) -> &str {
            "echo"
        }
        fn description(&self) -> &str {
            "echo"
        }
        fn input_schema(&self) -> Value {
            serde_json::json!({"type": "object"})
        }
        async fn execute(&self, input: Value) -> eyre::Result<ToolResult> {
            Ok(ToolResult::success(input, 2))
        }
    }

    struct TypedTool;

    #[async_trait::async_trait]
    impl Tool for TypedTool {
        fn name(&self) -> &str {
            "typed"
        }
        fn description(&self) -> &str {
            "typed"
        }
        fn input_schema(&self) -> Value {
            serde_json::json!({
                "type": "object",
                "properties": { "count": { "type": "integer" } },
                "required": ["count"],
            })
        }
        async fn execute(&self, input: Value) -> eyre::Result<ToolResult> {
            Ok(ToolResult::success(input, 1))
        }
    }

    #[test]
    fn enforcing_blocks_a_call_its_schema_rejects_and_says_what_to_fix() {
        let tools = registry();
        tools.register(Arc::new(TypedTool));
        let bad = serde_json::json!({ "count": "three" });
        match argument_gate(ArgumentValidation::Enforce, "agent", &tools, "typed", &bad) {
            Some(GateOutcome::Blocked {
                message, reason, ..
            }) => {
                assert!(message.starts_with("'typed' did not run"));
                assert!(message.contains("`/count`"));
                assert!(message.contains("Its full input schema:"));
                assert!(!message.contains("three"));
                assert!(reason.starts_with("invalid arguments: /count"));
            }
            other => panic!("expected a block, got {other:?}"),
        }
        let good = serde_json::json!({ "count": 3 });
        assert!(
            argument_gate(ArgumentValidation::Enforce, "agent", &tools, "typed", &good).is_none()
        );
    }

    #[test]
    fn keys_the_schema_does_not_declare_do_not_block_a_call() {
        let tools = registry();
        tools.register(Arc::new(TypedTool));
        let extra = serde_json::json!({ "count": 3, "total": 3 });
        assert!(argument_gate(
            ArgumentValidation::Enforce,
            "agent",
            &tools,
            "typed",
            &extra
        )
        .is_none());
    }

    #[test]
    fn shadow_and_off_let_a_rejected_call_through() {
        let tools = registry();
        tools.register(Arc::new(TypedTool));
        let bad = serde_json::json!({});
        assert!(
            argument_gate(ArgumentValidation::Shadow, "agent", &tools, "typed", &bad).is_none()
        );
        assert!(argument_gate(ArgumentValidation::Off, "agent", &tools, "typed", &bad).is_none());
        assert!(argument_gate(
            ArgumentValidation::Enforce,
            "agent",
            &tools,
            "unknown",
            &bad
        )
        .is_none());
    }

    #[test]
    fn a_deny_is_reported_before_the_arguments_are_checked() {
        let tools = registry();
        tools.register(Arc::new(TypedTool));
        let gate_with = |deny: Option<&'static str>| {
            let g = ToolGates {
                arguments: ArgumentValidation::Enforce,
                ..gates(&Arc::new(Calls::default()), deny, CascadeState::Allow)
            };
            as_user(|| {
                gate_tool_call(
                    &g,
                    &permissive_policy(),
                    &tools,
                    "typed",
                    &serde_json::json!({}),
                )
            })
        };
        match gate_with(Some("no")) {
            GateOutcome::Blocked { message, .. } => assert!(message.starts_with("Policy denied")),
            other => panic!("expected the deny, got {other:?}"),
        }
        match gate_with(None) {
            GateOutcome::Blocked { message, .. } => {
                assert!(message.starts_with("'typed' did not run"))
            }
            other => panic!("expected the argument block, got {other:?}"),
        }
    }

    fn registry() -> ToolRegistry {
        let tools = ToolRegistry::new();
        tools.register(Arc::new(FailingTool));
        tools.register(Arc::new(EchoTool));
        tools.register(Arc::new(GatedTool(Arc::new(AtomicUsize::new(0)))));
        tools
    }

    const GATED: &str = "record-keeper.apply_change";

    fn response(decision: PolicyDecision) -> PolicyResponse {
        PolicyResponse {
            decision,
            matched_rule: Some("rule-7".to_string()),
            evaluation_time_us: 42,
        }
    }

    fn ask(allow_save_allow: bool) -> PolicyDecision {
        PolicyDecision::RequireApproval {
            reason: "Required by permission cascade".to_string(),
            allow_save_allow,
        }
    }

    #[test]
    fn a_requirement_upgrades_allow_and_never_offers_always_allow() {
        let combined =
            combine_with_tool_requirement(response(PolicyDecision::Allow), Some("gate".into()));
        assert!(matches!(
            combined.decision,
            PolicyDecision::RequireApproval { ref reason, allow_save_allow: false }
                if reason == "gate"
        ));
        assert_eq!(combined.matched_rule.as_deref(), Some("rule-7"));
        assert_eq!(combined.evaluation_time_us, 42);
    }

    #[test]
    fn a_requirement_never_relaxes_a_denial() {
        let denied = PolicyDecision::Deny {
            reason: "policy says no".to_string(),
        };
        let combined = combine_with_tool_requirement(response(denied), Some("gate".into()));
        assert!(matches!(
            combined.decision,
            PolicyDecision::Deny { ref reason } if reason == "policy says no"
        ));
    }

    #[test]
    fn a_requirement_keeps_the_policy_prompt_and_strips_always_allow() {
        let combined = combine_with_tool_requirement(response(ask(true)), Some("gate".into()));
        assert!(matches!(
            combined.decision,
            PolicyDecision::RequireApproval { ref reason, allow_save_allow: false }
                if reason == "Required by permission cascade"
        ));
    }

    #[test]
    fn no_requirement_passes_the_response_through_untouched() {
        for decision in [
            PolicyDecision::Allow,
            ask(true),
            PolicyDecision::Deny {
                reason: "no".to_string(),
            },
        ] {
            let before = response(decision);
            let kind = decision_kind(&before.decision);
            let allow_save = decision_allow_save_allow(&before.decision);
            let after = combine_with_tool_requirement(before, None);
            assert_eq!(decision_kind(&after.decision), kind);
            assert_eq!(decision_allow_save_allow(&after.decision), allow_save);
            assert_eq!(after.matched_rule.as_deref(), Some("rule-7"));
            assert_eq!(after.evaluation_time_us, 42);
        }
    }

    /// A member who chose "Allow every time" for this tool reaches the gate
    /// with a cascade of `Allow`. The requirement is composed after it, so
    /// the prompt still stands.
    #[test]
    fn a_saved_always_allow_cannot_defeat_the_requirement() {
        let calls = Arc::new(Calls::default());
        let g = gates(&calls, None, CascadeState::Allow);
        let out = as_user(|| {
            gate_tool_call(
                &g,
                &permissive_policy(),
                &registry(),
                GATED,
                &serde_json::json!({}),
            )
        });
        assert!(matches!(
            out,
            GateOutcome::RequireApproval {
                allow_save_allow: false,
                ..
            }
        ));
        assert_eq!(calls.seen(), vec!["capability", "cascade"]);
    }

    /// The requirement is applied after the cascade, so the cascade's own
    /// prompt wording survives and only the Always-Allow option is withdrawn.
    #[test]
    fn the_requirement_is_applied_after_the_cascade() {
        let calls = Arc::new(Calls::default());
        let g = gates(&calls, None, CascadeState::Ask);
        let out = as_user(|| {
            gate_tool_call(
                &g,
                &permissive_policy(),
                &registry(),
                GATED,
                &serde_json::json!({}),
            )
        });
        match out {
            GateOutcome::RequireApproval {
                reason,
                allow_save_allow,
                ..
            } => {
                assert_eq!(reason, "Required by permission cascade");
                assert!(!allow_save_allow);
            }
            other => panic!("expected RequireApproval, got {other:?}"),
        }
    }

    #[test]
    fn a_denial_still_wins_over_the_requirement() {
        let calls = Arc::new(Calls::default());
        for g in [
            gates(&calls, Some("members may not"), CascadeState::Allow),
            gates(&calls, None, CascadeState::Deny),
        ] {
            let out = as_user(|| {
                gate_tool_call(
                    &g,
                    &permissive_policy(),
                    &registry(),
                    GATED,
                    &serde_json::json!({}),
                )
            });
            assert!(matches!(out, GateOutcome::Blocked { .. }), "{out:?}");
        }
    }

    #[test]
    fn only_a_gated_tool_is_refused_on_an_unattended_path() {
        let tools = registry();
        let refusal = unattended_refusal(&tools, GATED).expect("gated tool is refused");
        assert!(refusal.contains(GATED), "{refusal}");
        assert!(refusal.contains("approve"), "{refusal}");
        assert_eq!(unattended_refusal(&tools, "echo"), None);
        assert_eq!(unattended_refusal(&tools, "not-registered"), None);
    }

    fn catalogue() -> (ToolRegistry, Arc<RecordingTool>) {
        let tool = Arc::new(RecordingTool::default());
        let tools = registry();
        tools.register(Arc::clone(&tool) as Arc<dyn Tool>);
        (tools, tool)
    }

    #[test]
    fn an_ask_for_a_call_missing_a_required_field_is_blocked_without_a_prompt() {
        let (tools, _) = catalogue();
        let calls = Arc::new(Calls::default());
        let g = gates(&calls, None, CascadeState::Ask);
        for input in [serde_json::json!({}), serde_json::json!({"name": null})] {
            match as_user(|| gate_tool_call(&g, &permissive_policy(), &tools, TOOL, &input)) {
                GateOutcome::Blocked {
                    message,
                    reason,
                    consumed,
                } => {
                    assert_eq!(reason, "called without required input `name`");
                    assert_eq!(
                        message,
                        "'catalogue.remove_entry' was called without required input `name`, so it \
                         did not run and no approval was requested. Call it again with every \
                         required field."
                    );
                    assert!(consumed.is_empty());
                }
                other => panic!("expected Blocked for {input}, got {other:?}"),
            }
        }
        let complete = serde_json::json!({"name": "figma"});
        assert!(matches!(
            as_user(|| gate_tool_call(&g, &permissive_policy(), &tools, TOOL, &complete)),
            GateOutcome::RequireApproval { .. }
        ));
    }

    #[test]
    fn only_a_field_left_out_or_null_against_its_declared_type_is_missing() {
        use serde_json::json;
        let requiring = |property: Value| json!({ "type": "object", "properties": { "name": property }, "required": ["name"] });
        let missing = Some(vec!["name".to_string()]);
        let cases = [
            (
                requiring(json!({"type": "string"})),
                json!({}),
                missing.clone(),
            ),
            (
                requiring(json!({"type": "string"})),
                json!({"name": null}),
                missing,
            ),
            (
                requiring(json!({"type": "string"})),
                json!({"name": ""}),
                None,
            ),
            (
                requiring(json!({"type": ["string", "null"]})),
                json!({"name": null}),
                None,
            ),
            (
                requiring(json!({"type": "string", "nullable": true})),
                json!({"name": null}),
                None,
            ),
            (
                requiring(json!({"anyOf": [{"type": "string"}, {"type": "null"}]})),
                json!({"name": null}),
                None,
            ),
            (json!({"type": "object", "required": []}), json!({}), None),
            (
                json!({"type": "object", "required": "name"}),
                json!({}),
                None,
            ),
            (json!({"oneOf": [{"required": ["name"]}]}), json!({}), None),
        ];
        for (schema, input, expected) in cases {
            assert_eq!(
                missing_required_fields(&schema, &input),
                expected,
                "{schema} with {input}"
            );
        }
    }

    #[test]
    fn an_allowed_call_missing_a_required_field_still_reaches_the_tool() {
        let (tools, _) = catalogue();
        let calls = Arc::new(Calls::default());
        let g = gates(&calls, None, CascadeState::Allow);
        let out = as_user(|| {
            gate_tool_call(
                &g,
                &permissive_policy(),
                &tools,
                TOOL,
                &serde_json::json!({}),
            )
        });
        assert!(matches!(out, GateOutcome::Execute { .. }), "{out:?}");
    }

    #[test]
    fn a_denial_wins_over_an_incomplete_call() {
        let (tools, _) = catalogue();
        let calls = Arc::new(Calls::default());
        for g in [
            gates(&calls, Some("members may not"), CascadeState::Ask),
            gates(&calls, None, CascadeState::Deny),
        ] {
            let out = as_user(|| {
                gate_tool_call(
                    &g,
                    &permissive_policy(),
                    &tools,
                    TOOL,
                    &serde_json::json!({}),
                )
            });
            match out {
                GateOutcome::Blocked { message, .. } => {
                    assert!(message.starts_with("Policy denied: "), "{message}")
                }
                other => panic!("expected Blocked, got {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn execute_approved_gates_the_call_again() {
        let (tools, tool) = catalogue();
        let calls = Arc::new(Calls::default());
        let access = ToolAccessSnapshot::default();
        let approved = |g: ToolGates, input: Value| {
            let tools = &tools;
            let access = &access;
            CURRENT_USER_ID.scope("alice".to_string(), async move {
                execute_approved(&g, &permissive_policy(), tools, access, TOOL, &input).await
            })
        };

        let ran = approved(
            gates(&calls, None, CascadeState::Ask),
            serde_json::json!({"name": "figma"}),
        )
        .await;
        assert!(ran.success, "the approval stands in for the prompt");
        assert_eq!(ran.decision(), "allow");

        let revoked = approved(
            gates(&calls, Some("members may not"), CascadeState::Ask),
            serde_json::json!({"name": "probe"}),
        )
        .await;
        assert_eq!(revoked.output_text, "Policy denied: members may not");
        assert_eq!(revoked.deny_reason.as_deref(), Some("members may not"));
        assert_eq!(revoked.decision(), "deny");

        let scanned = approved(
            gates(&calls, None, CascadeState::Ask),
            serde_json::json!({"name": "${CATALOGUE_TOKEN}"}),
        )
        .await;
        assert!(
            scanned
                .output_text
                .starts_with("BLOCKED by security policy"),
            "{}",
            scanned.output_text
        );
        assert!(scanned.is_error);

        assert_eq!(tool.runs(), [serde_json::json!({"name": "figma"})]);
    }

    #[tokio::test]
    async fn access_block_is_returned_without_executing() {
        let access = ToolAccessSnapshot::new(HashMap::from([(
            "echo".to_string(),
            "hidden for you".to_string(),
        )]));
        let out = execute_gated(&registry(), &access, "echo", &serde_json::json!({}), None).await;
        assert_eq!(out.deny_reason.as_deref(), Some("hidden for you"));
        assert_eq!(out.output_text, "hidden for you");
        assert_eq!(out.decision(), "deny");
        assert!(out.is_error);
        assert!(!out.success);
    }

    #[tokio::test]
    async fn tool_failure_is_an_error_result_not_a_denial() {
        let out = execute_gated(
            &registry(),
            &ToolAccessSnapshot::default(),
            "fails",
            &serde_json::json!({}),
            None,
        )
        .await;
        assert!(out.is_error);
        assert!(!out.success);
        assert_eq!(out.output_text, "boom");
        assert_eq!(out.duration_ms, 3);
        assert_eq!(out.decision(), "allow");
        assert!(out.deny_reason.is_none());
    }

    #[tokio::test]
    async fn exec_input_is_what_the_tool_receives() {
        let record = serde_json::json!({"v": "[[secret:x]]"});
        let out = execute_gated(
            &registry(),
            &ToolAccessSnapshot::default(),
            "echo",
            &record,
            Some(serde_json::json!({"v": "cleartext"})),
        )
        .await;
        assert!(out.success);
        assert_eq!(out.output, serde_json::json!({"v": "cleartext"}));
        assert_eq!(out.output_text, r#"{"v":"cleartext"}"#);
    }

    #[tokio::test]
    async fn unknown_tool_is_reported_as_a_tool_error() {
        let out = execute_gated(
            &registry(),
            &ToolAccessSnapshot::default(),
            "nope",
            &serde_json::json!({}),
            None,
        )
        .await;
        assert!(out.is_error);
        assert_eq!(out.output_text, tool_not_found_message("nope", &registry()));
    }

    /// A tool that says what it does to its surface, or says nothing.
    struct Surfaced {
        name: &'static str,
        effect: Option<SurfaceEffect>,
    }

    #[async_trait::async_trait]
    impl Tool for Surfaced {
        fn name(&self) -> &str {
            self.name
        }
        fn description(&self) -> &str {
            "surfaced"
        }
        fn input_schema(&self) -> Value {
            serde_json::json!({"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"]})
        }
        fn surface_effect(&self, _input: &Value) -> Option<SurfaceEffect> {
            self.effect.clone()
        }
        async fn execute(&self, input: Value) -> eyre::Result<ToolResult> {
            Ok(ToolResult::success(input, 1))
        }
    }

    fn surfaced_tools() -> ToolRegistry {
        let tools = ToolRegistry::new();
        let writes = |platform: &str| {
            Some(SurfaceEffect::Writes {
                platform: platform.into(),
                destination: "C1".into(),
            })
        };
        for (name, effect) in [
            ("messaging.send", writes("slack")),
            ("messaging.read", Some(SurfaceEffect::Reads)),
            ("slack.list_channels", Some(SurfaceEffect::Reads)),
            ("slack.update_message", None),
            ("slack.send", None),
            ("email.send", writes("email")),
            ("notes.write", None),
        ] {
            tools.register(Arc::new(Surfaced { name, effect }));
        }
        tools
    }

    fn desk(attended: bool) -> workforce_common::channels::desk::DeskScope {
        use workforce_common::channels::desk::{DeskPlace, DeskTarget};
        workforce_common::channels::desk::DeskScope {
            target: DeskTarget::of_turn(&DeskPlace::Dm, "desk", None, false),
            owner: "person-1".into(),
            attended,
        }
    }

    fn allowed() -> PolicyResponse {
        PolicyResponse {
            decision: PolicyDecision::Allow,
            matched_rule: None,
            evaluation_time_us: 0,
        }
    }

    #[test]
    fn a_chat_platform_s_tool_writes_unless_it_says_it_reads() {
        let tools = surfaced_tools();
        let input = serde_json::json!({"channel": "C9", "text": "hi"});
        let effect = |name| surface_effect_of(&tools, name, &input);
        assert_eq!(
            effect("slack.send"),
            Some(SurfaceEffect::Writes {
                platform: "slack".into(),
                destination: "C9".into()
            })
        );
        assert!(matches!(
            effect("slack.update_message"),
            Some(SurfaceEffect::Writes { .. })
        ));
        assert_eq!(effect("slack.list_channels"), Some(SurfaceEffect::Reads));
        assert_eq!(effect("messaging.read"), Some(SurfaceEffect::Reads));
        assert_eq!(effect("notes.write"), None);
        assert!(matches!(
            effect("slack.anything_added_later"),
            Some(SurfaceEffect::Writes { .. })
        ));
    }

    #[test]
    fn a_write_off_desk_in_an_attended_desk_turn_needs_the_person_s_say() {
        let input = serde_json::json!({"channel": "C1", "text": "the release is out"});
        let to = |platform: &str| SurfaceEffect::Writes {
            platform: platform.into(),
            destination: "C1".into(),
        };
        let gated = apply_surface_gate(
            allowed(),
            Some(&to("slack")),
            Some(&desk(true)),
            &input,
            || Kept::NotYet,
        );
        match gated.decision {
            PolicyDecision::RequireApproval {
                reason,
                allow_save_allow,
            } => {
                assert_eq!(
                    reason,
                    "Posts outside Desk, to slack C1: \"the release is out\""
                );
                assert!(allow_save_allow);
            }
            other => panic!("expected an approval, got {other:?}"),
        }
        let email = apply_surface_gate(
            allowed(),
            Some(&to("email")),
            Some(&desk(true)),
            &input,
            || Kept::NotYet,
        );
        assert!(matches!(
            email.decision,
            PolicyDecision::RequireApproval { .. }
        ));
        for (effect, scope) in [
            (Some(to("desk")), Some(desk(true))),
            (Some(to("slack")), Some(desk(false))),
            (Some(to("slack")), None),
            (Some(SurfaceEffect::Reads), Some(desk(true))),
            (None, Some(desk(true))),
        ] {
            let decision =
                apply_surface_gate(allowed(), effect.as_ref(), scope.as_ref(), &input, || {
                    Kept::NotYet
                })
                .decision;
            assert_eq!(decision, PolicyDecision::Allow, "{effect:?} {scope:?}");
        }
        let denied = PolicyResponse {
            decision: PolicyDecision::Deny {
                reason: "no".into(),
            },
            ..allowed()
        };
        assert!(matches!(
            apply_surface_gate(
                denied,
                Some(&to("slack")),
                Some(&desk(true)),
                &input,
                || Kept::NotYet
            )
            .decision,
            PolicyDecision::Deny { .. }
        ));
    }

    /// An allow list keyed as the store would: person, agent, tool,
    /// destination.
    #[derive(Default)]
    struct Allows(Mutex<std::collections::HashSet<(String, String, String, Destination)>>);

    impl SurfaceAllows for Allows {
        fn allows(&self, person: &str, agent: &str, tool: &str, to: &Destination) -> bool {
            self.0
                .lock()
                .unwrap()
                .contains(&(person.into(), agent.into(), tool.into(), to.clone()))
        }

        fn allow(
            &self,
            person: &str,
            agent: &str,
            tool: &str,
            to: &Destination,
        ) -> eyre::Result<()> {
            self.0
                .lock()
                .unwrap()
                .insert((person.into(), agent.into(), tool.into(), to.clone()));
            Ok(())
        }
    }

    #[test]
    fn a_held_write_shows_what_it_says_however_its_tool_names_it() {
        assert_eq!(
            preview_of(&serde_json::json!({"text": "hi", "subject": "s"})),
            "hi"
        );
        assert_eq!(
            preview_of(&serde_json::json!({"Subject": "Q3", "TextBody": "numbers attached"})),
            "numbers attached"
        );
        assert_eq!(preview_of(&serde_json::json!({"Subject": "Q3"})), "Q3");
        assert_eq!(preview_of(&serde_json::json!({"text": 3})), "");
    }

    #[test]
    fn an_email_is_to_everyone_it_addresses_however_its_tool_names_them() {
        let to = |input: Value| SurfaceEffect::email(&input).destination();
        let alice = to(serde_json::json!({"to": "Alice@x.com", "subject": "hi"}));
        assert_eq!(
            alice.as_ref().map(|d| d.target.as_str()),
            Some("alice@x.com")
        );
        assert_ne!(
            to(serde_json::json!({"to": "alice@x.com", "bcc": "anyone@evil.example"})),
            alice,
            "a blind copy is a recipient too"
        );
        assert_eq!(
            to(serde_json::json!({
                "To": "Alice <alice@x.com>, bob@x.com",
                "Cc": "carol@x.com"
            }))
            .map(|d| d.target),
            Some("alice@x.com, bob@x.com, carol@x.com".to_string())
        );
        assert_eq!(
            to(serde_json::json!({
                "messages": [{"to": [{"email": "bob@x.com"}]}, {"to": "alice@x.com"}]
            }))
            .map(|d| d.target),
            Some("alice@x.com, bob@x.com".to_string())
        );
        assert_eq!(
            to(serde_json::json!({"cc": ["dan@x.com"], "to": {"email": "Carol@x.com", "name": "C"}}))
                .map(|d| d.target),
            Some("carol@x.com, dan@x.com".to_string())
        );
        assert_eq!(to(serde_json::json!({"subject": "nobody"})), None);
        assert_eq!(
            SurfaceEffect::email_api(&serde_json::json!({
                "method": "POST", "path": "/email", "body": {"To": "bob@x.com"}
            }))
            .and_then(|e| e.destination())
            .map(|d| d.target),
            Some("bob@x.com".to_string()),
            "a raw send names its recipients in its body"
        );
        assert_eq!(
            SurfaceEffect::email_api(&serde_json::json!({"method": "get", "path": "/servers"})),
            None
        );
    }

    #[test]
    fn a_destination_is_one_however_a_call_writes_it() {
        let ops = Destination::of("Slack", "#Ops").unwrap();
        assert_eq!(Destination::of("slack", " ops "), Some(ops.clone()));
        assert_ne!(Destination::of("slack", "#ops-2"), Some(ops.clone()));
        assert_ne!(Destination::of("teams", "ops"), Some(ops.clone()));
        assert_ne!(
            Destination::of("slack", "@ops"),
            Some(ops),
            "a person is not the channel of their name"
        );
        assert_eq!(Destination::of("slack", " # "), None);
        assert_eq!(Destination::of("slack", "@"), None);
    }

    #[tokio::test]
    async fn a_destination_the_person_always_allows_passes_and_only_that_one() {
        let calls = Arc::new(Calls::default());
        let allows = Arc::new(Allows::default());
        let g = ToolGates {
            surface_allows: Some(allows.clone()),
            ..gates(&calls, None, CascadeState::Allow)
        };
        let gate_with = |g: &ToolGates, user: &'static str, tool: &'static str| {
            let post = serde_json::json!({"channel": "C1", "text": "hi"});
            let g = g.clone();
            crate::runner::with_desk(Some(desk(true)), async move {
                CURRENT_USER_ID.sync_scope(user.to_string(), || {
                    gate_tool_call(&g, &permissive_policy(), &surfaced_tools(), tool, &post)
                })
            })
        };
        let gate = |g: &ToolGates, user: &'static str| gate_with(g, user, "messaging.send");
        let first = gate(&g, "alice").await;
        let GateOutcome::RequireApproval {
            allow_save_allow,
            holds,
            ..
        } = first
        else {
            panic!("held: {first:?}")
        };
        assert!(allow_save_allow, "Always allow is offered where it is kept");
        let to = holds.expect("the destination Always allow would keep");
        assert_eq!(to, Destination::of("slack", "C1").unwrap());
        allows
            .allow("alice", "agent", "messaging.send", &to)
            .unwrap();
        assert!(matches!(
            gate(&g, "alice").await,
            GateOutcome::Execute { .. }
        ));
        assert!(
            matches!(
                gate_with(&g, "alice", "slack.update_message").await,
                GateOutcome::RequireApproval { .. }
            ),
            "nor another tool's writes there"
        );
        assert!(
            matches!(gate(&g, "bob").await, GateOutcome::RequireApproval { .. }),
            "another person's allow is not theirs"
        );
        let other_agent = ToolGates {
            agent_id: AgentId::new("other"),
            ..g.clone()
        };
        assert!(
            matches!(
                gate(&other_agent, "alice").await,
                GateOutcome::RequireApproval { .. }
            ),
            "nor another agent's"
        );
        let unkept = gate(&gates(&calls, None, CascadeState::Allow), "alice").await;
        assert!(
            matches!(
                unkept,
                GateOutcome::RequireApproval {
                    allow_save_allow: false,
                    holds: None,
                    ..
                }
            ),
            "with no allow list, Always allow is not offered: {unkept:?}"
        );
    }

    #[test]
    fn a_kept_allow_never_relaxes_another_gate() {
        let input = serde_json::json!({"channel": "C1", "text": "hi"});
        let to = SurfaceEffect::Writes {
            platform: "slack".into(),
            destination: "C1".into(),
        };
        let asked = PolicyResponse {
            decision: PolicyDecision::RequireApproval {
                reason: "Required by permission cascade".into(),
                allow_save_allow: true,
            },
            ..allowed()
        };
        assert_eq!(
            apply_surface_gate(asked.clone(), Some(&to), Some(&desk(true)), &input, || {
                Kept::Allowed
            })
            .decision,
            asked.decision
        );
        let mut asked_of = 0;
        apply_surface_gate(allowed(), Some(&to), None, &input, || {
            asked_of += 1;
            Kept::NotYet
        });
        assert_eq!(
            asked_of, 0,
            "the list is read only for a write the gate would hold"
        );
    }

    #[tokio::test]
    async fn the_gate_holds_a_slack_post_from_a_desk_turn_and_nothing_else() {
        let tools = surfaced_tools();
        let calls = Arc::new(Calls::default());
        let g = gates(&calls, None, CascadeState::Allow);
        let post = serde_json::json!({"channel": "C1", "text": "hi"});
        let in_desk = crate::runner::with_desk(Some(desk(true)), async {
            as_user(|| gate_tool_call(&g, &permissive_policy(), &tools, "slack.send", &post))
        })
        .await;
        assert!(
            matches!(in_desk, GateOutcome::RequireApproval { .. }),
            "{in_desk:?}"
        );
        let read = crate::runner::with_desk(Some(desk(true)), async {
            as_user(|| {
                gate_tool_call(
                    &g,
                    &permissive_policy(),
                    &tools,
                    "slack.list_channels",
                    &post,
                )
            })
        })
        .await;
        assert!(matches!(read, GateOutcome::Execute { .. }), "{read:?}");
        let from_slack =
            as_user(|| gate_tool_call(&g, &permissive_policy(), &tools, "slack.send", &post));
        assert!(
            matches!(from_slack, GateOutcome::Execute { .. }),
            "{from_slack:?}"
        );
    }
}
