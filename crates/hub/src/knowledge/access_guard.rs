//! Cascade-aware gate for knowledge / codegraph tools (phase 9 of the
//! User RBAC + Permission Cascade plan).
//!
//! The knowledge service is org-wide by design (one shared index per
//! repo). Per-document team metadata is the prerequisite for true
//! row-level slicing and isn't on disk today — that's the "index
//! slicing per-team where applicable" the plan flagged as conditional.
//!
//! What this module *does* provide is a defence-in-depth layer the
//! tools call before reading from the shared index: every invocation
//! checks `cascade::evaluate` against the calling
//! [`CURRENT_USER_ID`](workforce_agent::runner::CURRENT_USER_ID), and a
//! `Deny` from the cascade short-circuits the tool with a structured
//! failure result. The hot path is a single LRU-cached evaluator call
//! per (user, agent, tool) triple — see [`PermissionEvaluator`].
//!
//! ## Why not the runner gate?
//!
//! The runner currently delegates to `PolicyEngine::evaluate` for its
//! Allow / Deny / RequireApproval check (`crates/agent-runtime/src/runner.rs`).
//! Switching that to consult the cascade is the right structural move
//! but lands as a phase-3 finish-up, not phase 9 — it touches every
//! tool gate, not just knowledge. This module is the bridge until that
//! switch happens.
//!
//! ## Audit shape
//!
//! Every check emits an [`AuditOperation::PermissionDecision`] event
//! with the cascade state in the `owner_identity_id` slot (matching the
//! `/api/permissions/cascade` handler), so existing dashboards that
//! watch the operation surface knowledge-tool calls without schema work.

use std::sync::Arc;
use std::time::Instant;

use workforce_agent::runner::CURRENT_USER_ID;
use workforce_agent::tools::ToolResult;
use workforce_permissions::cache::PermissionEvaluator;
use workforce_permissions::cascade::CascadeRequest;
use workforce_permissions::types::PermissionState;

use crate::permissions_audit::emit_decision;

/// Outcome of a cascade check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuardOutcome {
    /// Cascade is disabled (no evaluator wired) or returned a non-Deny
    /// state — the tool is free to execute. The runner's PolicyEngine
    /// gate is the source of truth for Ask / AlwaysAsk prompts; by the
    /// time the tool's `execute` runs, that gate has already passed.
    Allowed,
    /// Cascade refused. The tool must short-circuit and not touch the
    /// shared index.
    Denied,
}

/// Per-tool cascade gate. Cheap to construct — clones the `Arc` and a
/// few strings. One per tool because the `tool_name` is fixed per tool
/// type; the `agent_id` is the agent the service is attached to.
#[derive(Clone)]
pub struct KnowledgeAccessGuard {
    evaluator: Arc<PermissionEvaluator>,
    agent_id: String,
    tool_name: String,
}

impl KnowledgeAccessGuard {
    pub fn new(
        evaluator: Arc<PermissionEvaluator>,
        agent_id: impl Into<String>,
        tool_name: impl Into<String>,
    ) -> Self {
        Self {
            evaluator,
            agent_id: agent_id.into(),
            tool_name: tool_name.into(),
        }
    }

    /// Check the cascade for the current `(user, agent, tool)` triple.
    /// Returns [`GuardOutcome::Allowed`] when no `CURRENT_USER_ID` is
    /// in scope (CLI / system jobs) — gating system callers would
    /// break maintenance flows.
    pub fn check(&self) -> GuardOutcome {
        let Some(user_id) = CURRENT_USER_ID
            .try_with(|u| u.clone())
            .ok()
            .filter(|u| !u.is_empty())
        else {
            return GuardOutcome::Allowed;
        };

        let teams: Vec<String> = Vec::new();
        let request = CascadeRequest {
            user_id: &user_id,
            agent_id: &self.agent_id,
            team_ids: &teams,
            tool_name: &self.tool_name,
        };

        let state = match self.evaluator.evaluate(&request) {
            Ok(state) => state,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    tool = %self.tool_name,
                    agent = %self.agent_id,
                    user = %user_id,
                    "Knowledge cascade check failed; falling back to Allow",
                );
                return GuardOutcome::Allowed;
            }
        };
        emit_decision(&user_id, &self.tool_name, state);
        if state == PermissionState::Deny {
            GuardOutcome::Denied
        } else {
            GuardOutcome::Allowed
        }
    }

    /// Optional helper for tools that want a one-call gate: returns
    /// `Some(ToolResult::failure(...))` when the cascade said `Deny`,
    /// `None` otherwise. Tools wrap their `execute` body in
    /// `if let Some(denied) = guard.guard(start) { return Ok(denied); }`.
    pub fn guard(&self, start: Instant) -> Option<ToolResult> {
        match self.check() {
            GuardOutcome::Allowed => None,
            GuardOutcome::Denied => Some(ToolResult::failure(
                format!(
                    "{} refused by permission cascade for the active user (tool gated to deny in workforce-permissions)",
                    self.tool_name,
                ),
                start.elapsed().as_millis() as u64,
            )),
        }
    }
}

/// Convenience for tools whose guard is `Option<KnowledgeAccessGuard>`:
/// passes through to [`KnowledgeAccessGuard::guard`] when the guard is
/// wired, returns `None` otherwise.
pub fn maybe_guard(guard: Option<&KnowledgeAccessGuard>, start: Instant) -> Option<ToolResult> {
    guard.and_then(|g| g.guard(start))
}

#[cfg(test)]
mod tests {
    use super::*;
    use workforce_permissions::sqlite::SqlitePermissionStore;
    use workforce_permissions::store::PermissionStore;
    use workforce_permissions::types::{PermissionOrigin, PermissionRule, PermissionScope};

    fn evaluator_with(rules: Vec<PermissionRule>) -> Arc<PermissionEvaluator> {
        let store: Arc<dyn PermissionStore> = Arc::new(SqlitePermissionStore::in_memory().unwrap());
        for r in rules {
            store.upsert_rule(&r).unwrap();
        }
        Arc::new(PermissionEvaluator::new(store))
    }

    #[tokio::test]
    async fn no_current_user_passes_through() {
        let guard = KnowledgeAccessGuard::new(evaluator_with(vec![]), "agent-a", "knowledge.ask");
        // No CURRENT_USER_ID in scope — guard must not gate system jobs.
        assert_eq!(guard.check(), GuardOutcome::Allowed);
    }

    #[tokio::test]
    async fn user_scope_deny_blocks_the_call() {
        let evaluator = evaluator_with(vec![PermissionRule::new(
            PermissionScope::User,
            "alice",
            "knowledge.ask",
            PermissionState::Deny,
            PermissionOrigin::Admin,
        )]);
        let guard = KnowledgeAccessGuard::new(evaluator, "agent-a", "knowledge.ask");
        let outcome = CURRENT_USER_ID
            .scope("alice".to_string(), async { guard.check() })
            .await;
        assert_eq!(outcome, GuardOutcome::Denied);
    }

    #[tokio::test]
    async fn allow_state_passes_through() {
        let evaluator = evaluator_with(vec![PermissionRule::new(
            PermissionScope::User,
            "alice",
            "knowledge.ask",
            PermissionState::Allow,
            PermissionOrigin::Admin,
        )]);
        let guard = KnowledgeAccessGuard::new(evaluator, "agent-a", "knowledge.ask");
        let outcome = CURRENT_USER_ID
            .scope("alice".to_string(), async { guard.check() })
            .await;
        assert_eq!(outcome, GuardOutcome::Allowed);
    }

    #[tokio::test]
    async fn maybe_guard_returns_some_failure_on_deny() {
        let evaluator = evaluator_with(vec![PermissionRule::new(
            PermissionScope::User,
            "alice",
            "knowledge.symbol",
            PermissionState::Deny,
            PermissionOrigin::Admin,
        )]);
        let guard = KnowledgeAccessGuard::new(evaluator, "agent-a", "knowledge.symbol");
        let start = Instant::now();
        let result = CURRENT_USER_ID
            .scope("alice".to_string(), async {
                maybe_guard(Some(&guard), start)
            })
            .await;
        let denied = result.expect("guard must short-circuit on Deny");
        assert!(!denied.success);
        let err = denied.error.unwrap_or_default();
        assert!(
            err.contains("knowledge.symbol"),
            "failure error names the tool: {err}"
        );
    }

    #[tokio::test]
    async fn maybe_guard_no_guard_is_no_op() {
        assert!(maybe_guard(None, Instant::now()).is_none());
    }

    #[tokio::test]
    async fn ask_state_does_not_block_after_runner_approval() {
        // The runner's PolicyEngine prompt happens before execute() is
        // called; by the time the guard runs, the user has approved.
        // Treating Ask as a soft-allow at this layer keeps the gate
        // from double-prompting.
        let evaluator = evaluator_with(vec![PermissionRule::new(
            PermissionScope::Team,
            "team-a",
            "knowledge.ask",
            PermissionState::Ask,
            PermissionOrigin::Admin,
        )]);
        let guard = KnowledgeAccessGuard::new(evaluator, "agent-a", "knowledge.ask");
        let outcome = CURRENT_USER_ID
            .scope("alice".to_string(), async { guard.check() })
            .await;
        assert_eq!(outcome, GuardOutcome::Allowed);
    }
}
