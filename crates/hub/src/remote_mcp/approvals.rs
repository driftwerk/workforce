//! `approval.*` — the bridge that lets a remote MCP client answer the
//! approval prompts its own calls raise. A self-serve prompt inside an
//! `agent.chat` turn or on a direct tool call is answered with
//! `approval.decide`; a prompt routed to designated approvers is resolved by
//! them in Slack and only inspected here.

use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;

use workforce_agent::runner::CURRENT_AGENT_ID;
use workforce_agent::tools::{Tool, ToolResult};
use workforce_core::policy::PermissionChoice;

use crate::mcp_gate::{choice_name, offered, PendingCall, DECIDE_NEXT};

use super::agent_tools::{
    caller, reach_agent, turn_result, unwrap_or_result, AgentToolsContext, ChatTurn,
};

pub fn not_offered(choice: PermissionChoice) -> ToolResult {
    ToolResult::failure(
        format!("{} is not offered for this prompt", choice_name(choice)),
        0,
    )
}

// ── approval.decide ───────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct DecideArgs {
    request_id: String,
    choice: PermissionChoice,
    #[serde(default)]
    turn_id: Option<String>,
    #[serde(default)]
    wait_seconds: Option<u64>,
}

/// The caller's turn holding `request_id`: the named one when a turn_id is
/// given, else the first that awaits it.
fn turn_awaiting(
    ctx: &AgentToolsContext,
    uid: &str,
    turn_id: Option<&str>,
    request_id: &str,
) -> Option<(String, Arc<ChatTurn>)> {
    match turn_id {
        Some(id) => ctx
            .turns
            .get(id, uid)
            .filter(|t| t.awaits(request_id))
            .map(|t| (id.to_string(), t)),
        None => ctx.turns.find(uid, |t| t.awaits(request_id)),
    }
}

struct DecideTool {
    ctx: Arc<AgentToolsContext>,
}

impl DecideTool {
    async fn run(&self, args: DecideArgs) -> Result<ToolResult, ToolResult> {
        let uid = caller()?;
        if let Some((turn_id, turn)) =
            turn_awaiting(&self.ctx, &uid, args.turn_id.as_deref(), &args.request_id)
        {
            reach_agent(&self.ctx, &uid, Some(&turn.agent_id))?;
            turn.answer(&args.request_id, args.choice, &uid)?;
            turn.wait(self.ctx.turn_wait(args.wait_seconds)).await;
            return Ok(turn_result(&self.ctx, &turn, Some(turn_id)));
        }
        if let Some(call) = self.ctx.pending_calls.get(&args.request_id, &uid) {
            return self
                .decide_call(&args.request_id, call, args.choice, &uid)
                .await;
        }
        Err(match routed_row(&self.ctx, &args.request_id, &uid) {
            Some(_) => ToolResult::failure(
                "this request was routed to designated approvers; approval.status reports their decision",
                0,
            ),
            None => ToolResult::failure("unknown request", 0),
        })
    }

    async fn decide_call(
        &self,
        request_id: &str,
        call: Arc<PendingCall>,
        choice: PermissionChoice,
        uid: &str,
    ) -> Result<ToolResult, ToolResult> {
        if !offered(&call.options, choice) {
            return Err(not_offered(choice));
        }
        let acting_agent = CURRENT_AGENT_ID.try_with(|a| a.clone()).unwrap_or_default();
        if acting_agent != call.agent_id() {
            return Err(ToolResult::failure(
                format!(
                    "this request belongs to agent {}; decide it through that agent's endpoint",
                    call.agent_id()
                ),
                0,
            ));
        }
        let call = self
            .ctx
            .pending_calls
            .take(request_id, uid)
            .ok_or_else(|| ToolResult::failure("unknown request", 0))?;
        if choice.persists() {
            self.persist_choice(uid, &call, choice).await;
        }
        Ok(if choice.is_allow() {
            call.run().await
        } else {
            ToolResult::success(
                serde_json::json!({
                    "status": "denied",
                    "request_id": request_id,
                    "tool": call.tool_name,
                }),
                0,
            )
        })
    }

    async fn persist_choice(&self, uid: &str, call: &PendingCall, choice: PermissionChoice) {
        let writer = self
            .ctx
            .gateway
            .registry
            .get(call.agent_id())
            .and_then(|svc| svc.permission_writer.read().clone());
        let Some(writer) = writer else {
            return;
        };
        if let Err(e) = writer
            .persist_user_choice(uid, &call.tool_name, choice)
            .await
        {
            tracing::warn!(error = %e, tool = %call.tool_name, "failed to persist a remote MCP permission choice");
        }
    }
}

#[async_trait]
impl Tool for DecideTool {
    fn name(&self) -> &str {
        "approval.decide"
    }

    fn description(&self) -> &str {
        "Answer an approval prompt one of your own calls raised (status \"approval_required\" or \"approval_pending\" with a request_id). Only call this after the user has decided: allow_once runs the call now, deny_once refuses it, and the always variants also remember the choice for that tool. A turn's prompt resumes the turn and returns its progress; a parked direct call returns the tool's result."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "request_id": { "type": "string" },
                "choice": { "type": "string", "enum": ["allow_once", "deny_once", "allow_always", "deny_always"] },
                "turn_id": { "type": "string", "description": "The turn that raised the prompt, when you have it" },
                "wait_seconds": { "type": "integer", "minimum": 1, "description": "For a turn's prompt: how long to wait for the turn to settle before returning a turn_id" }
            },
            "required": ["request_id", "choice"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> eyre::Result<ToolResult> {
        let args: DecideArgs = match serde_json::from_value(input) {
            Ok(a) => a,
            Err(e) => return Ok(ToolResult::failure(format!("invalid arguments: {e}"), 0)),
        };
        unwrap_or_result(self.run(args).await)
    }
}

// ── approval.status ───────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct StatusArgs {
    request_id: String,
    #[serde(default)]
    turn_id: Option<String>,
}

struct StatusTool {
    ctx: Arc<AgentToolsContext>,
}

impl StatusTool {
    fn run(&self, args: StatusArgs) -> Result<ToolResult, ToolResult> {
        let uid = caller()?;
        let request_id = args.request_id.as_str();
        if let Some((turn_id, turn)) =
            turn_awaiting(&self.ctx, &uid, args.turn_id.as_deref(), request_id)
        {
            let prompt = turn.prompt(request_id);
            return Ok(ToolResult::success(
                serde_json::json!({
                    "request_id": request_id,
                    "kind": "turn",
                    "status": "pending",
                    "turn_id": turn_id,
                    "prompt": prompt,
                    "next": DECIDE_NEXT,
                }),
                0,
            ));
        }
        if let Some(call) = self.ctx.pending_calls.get(request_id, &uid) {
            return Ok(ToolResult::success(
                serde_json::json!({
                    "request_id": request_id,
                    "kind": "call",
                    "status": "pending",
                    "tool": call.tool_name,
                    "detail": call.detail,
                    "expires_in_secs": call.expires_in_secs(),
                    "next": DECIDE_NEXT,
                }),
                0,
            ));
        }
        routed_row(&self.ctx, request_id, &uid)
            .map(|row| {
                let next = match row.status.as_str() {
                    "pending" => "Designated approvers have it; check again later",
                    "approved" => "The call ran; its outcome is in your session with this agent (agent.history)",
                    _ => "The request is closed",
                };
                ToolResult::success(
                    serde_json::json!({
                        "request_id": request_id,
                        "kind": "routed",
                        "status": row.status,
                        "tool": row.tool_name,
                        "resolved_by": row.resolved_by,
                        "created_at": row.created_at,
                        "expires_at": row.expires_at,
                        "next": next,
                    }),
                    0,
                )
            })
            .ok_or_else(|| ToolResult::failure("unknown request", 0))
    }
}

/// The caller's own durable routed row, if any — the store is shared by
/// every agent, so the default agent's handle sees them all.
fn routed_row(
    ctx: &AgentToolsContext,
    request_id: &str,
    uid: &str,
) -> Option<crate::hub_core::approvals::PendingApproval> {
    let store = ctx
        .gateway
        .registry
        .default_agent()
        .and_then(|svc| svc.pending_approval_store.read().clone())?;
    store
        .get(request_id)
        .ok()
        .flatten()
        .filter(|row| row.requester_id == uid)
}

#[async_trait]
impl Tool for StatusTool {
    fn name(&self) -> &str {
        "approval.status"
    }

    fn description(&self) -> &str {
        "Where one of your approval requests stands: pending on you (answer it with approval.decide), pending on designated approvers, or approved, denied or expired by them."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "request_id": { "type": "string" },
                "turn_id": { "type": "string" }
            },
            "required": ["request_id"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> eyre::Result<ToolResult> {
        let args: StatusArgs = match serde_json::from_value(input) {
            Ok(a) => a,
            Err(e) => return Ok(ToolResult::failure(format!("invalid arguments: {e}"), 0)),
        };
        unwrap_or_result(self.run(args))
    }
}

pub fn approval_tools(ctx: Arc<AgentToolsContext>) -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(DecideTool {
            ctx: Arc::clone(&ctx),
        }),
        Arc::new(StatusTool { ctx }),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decide_args_take_the_typed_choice() {
        let args: DecideArgs = serde_json::from_value(
            serde_json::json!({ "request_id": "r", "choice": "allow_always" }),
        )
        .unwrap();
        assert_eq!(args.choice, PermissionChoice::AllowAlways);
        assert!(serde_json::from_value::<DecideArgs>(
            serde_json::json!({ "request_id": "r", "choice": "yes" })
        )
        .is_err());
    }
}
