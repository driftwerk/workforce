//! One governed tool call on behalf of a monitor — reading its source, or
//! running a `run_skill` action — as the monitor's owner, through the gauntlet
//! the runner applies to a model-emitted call: the never-grantable families,
//! the policy engine, the capability gate scored against that user, argument
//! validation, the registry's access snapshot, and the tool's own deadline.
//! Nothing on this path can prompt, so a call that needs approval is refused.
//!
//! Two things an agent call has that this path does not: a system monitor
//! runs as no person, so the capability and cascade gates — which score a
//! user — do not apply to it, and only an administrator may save one; and no
//! session is in scope, so a session-bound tool (messaging, artifacts) run
//! through `run_skill` sees none of a conversation's context.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use workforce_agent::runner::CURRENT_AGENT_ID;
use workforce_agent::tools::access::ToolAccessSnapshot;
use workforce_agent::tools::dispatch::{
    execute_gated, gate_tool_call, unattended_refusal, GateOutcome, ToolGates,
};
use workforce_agent::tools::registry::ToolRegistry;
use workforce_common::session::TurnPrincipal;
use workforce_policy::engine::PolicyEngine;
use workforce_wasm_runtime::manifest::is_denied_tool;

use crate::wasm_tool_invoker::AdapterLoader;

/// Beyond the families no skill may be granted, a monitor may not author
/// monitors.
const MONITOR_DENIED: &[&str] = &["monitor"];

/// Why `tool` can never be read or run by a monitor, if it cannot.
pub fn denied_tool(tool: &str) -> Option<String> {
    (is_denied_tool(tool) || MONITOR_DENIED.contains(&tool)).then(|| {
        format!(
            "'{tool}' is in a family a monitor can never call (admin, vault, execution and \
             orchestration surfaces stay out of reach)"
        )
    })
}

/// One call: whose credentials it runs with, who it is recorded as, and what.
pub struct ToolCall<'a> {
    pub owner: Option<&'a str>,
    pub actor: &'a str,
    pub tool: &'a str,
    pub input: Value,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InvokeError {
    /// Refused before it ran; asking again changes nothing.
    Denied(String),
    /// Ran, or may have, and failed.
    Failed(String),
    /// Refused upstream for rate limiting; nothing executed.
    RateLimited(String),
}

impl InvokeError {
    pub fn message(&self) -> &str {
        match self {
            Self::Denied(m) | Self::Failed(m) | Self::RateLimited(m) => m,
        }
    }
}

#[async_trait]
pub trait ToolInvoke: Send + Sync {
    /// Whether `tool` is registered, loading its adapter when that registers
    /// lazily.
    fn resolves(&self, tool: &str) -> bool;

    async fn invoke(&self, call: ToolCall<'_>) -> Result<Value, InvokeError>;
}

pub type GatesFn = Arc<dyn Fn() -> ToolGates + Send + Sync>;
pub type AccessFn = Arc<dyn Fn() -> ToolAccessSnapshot + Send + Sync>;

/// The production invoker over one agent's registry and gates.
pub struct GovernedInvoker {
    tools: Arc<ToolRegistry>,
    policy: Arc<PolicyEngine>,
    gates: GatesFn,
    access: AccessFn,
    adapters: Option<AdapterLoader>,
}

impl GovernedInvoker {
    pub fn new(
        tools: Arc<ToolRegistry>,
        policy: Arc<PolicyEngine>,
        gates: GatesFn,
        access: AccessFn,
        adapters: Option<AdapterLoader>,
    ) -> Self {
        Self {
            tools,
            policy,
            gates,
            access,
            adapters,
        }
    }

    /// The invoker for `service`: its registry, policy, gates and access
    /// snapshot read at call time, so a changed capability matrix or tool
    /// filter applies to the next poll.
    pub fn for_service(
        service: Arc<crate::serve::AgentService>,
        adapters: Option<AdapterLoader>,
    ) -> Self {
        let gates = Arc::clone(&service);
        Self::new(
            Arc::clone(&service.tools),
            Arc::clone(&service.policy),
            Arc::new(move || gates.tool_gates()),
            Arc::new(move || service.tool_access_snapshot()),
            adapters,
        )
    }

    /// Registered, or loadable: the adapter loader runs with the hub's own
    /// vaults, outside any principal, as it does for a skill's first call.
    fn resolve(&self, tool: &str) -> bool {
        self.tools.get(tool).is_some()
            || self.adapters.as_ref().is_some_and(|load| {
                tool.split('.')
                    .next()
                    .is_some_and(|family| match load(family) {
                        Ok(()) => self.tools.get(tool).is_some(),
                        Err(e) => {
                            tracing::warn!(tool = %tool, family = %family, error = %e, "adapter for a monitor's tool could not be loaded");
                            false
                        }
                    })
            })
    }

    async fn run(&self, actor: &str, tool: &str, input: Value) -> Result<Value, InvokeError> {
        if let Some(refusal) = unattended_refusal(&self.tools, tool) {
            return Err(InvokeError::Denied(refusal));
        }
        let gates = (self.gates)();
        let exec_input = match gate_tool_call(&gates, &self.policy, &self.tools, tool, &input) {
            GateOutcome::Execute { exec_input, .. } => exec_input,
            GateOutcome::Blocked { reason, .. } => {
                tracing::warn!(
                    target: "sentinel.audit",
                    actor = %actor, tool = %tool, reason = %reason,
                    "monitor tool call denied"
                );
                return Err(InvokeError::Denied(reason));
            }
            GateOutcome::RequireApproval { reason, .. } => {
                tracing::warn!(
                    target: "sentinel.audit",
                    actor = %actor, tool = %tool, reason = %reason,
                    "monitor tool call denied: approval required"
                );
                return Err(InvokeError::Denied(format!(
                    "{tool} requires approval ({reason}), and nothing on a monitor's path can ask for one"
                )));
            }
        };
        tracing::info!(target: "sentinel.audit", actor = %actor, tool = %tool, "monitor tool call dispatched");
        let run = CURRENT_AGENT_ID
            .scope(
                gates.agent_id.as_str().to_string(),
                execute_gated(&self.tools, &(self.access)(), tool, &input, exec_input),
            )
            .await;
        if let Some(reason) = run.deny_reason {
            return Err(InvokeError::Denied(reason));
        }
        if run.success {
            return Ok(run.output);
        }
        Err(match run.rate_limited {
            Some(_) => InvokeError::RateLimited(run.output_text),
            None => InvokeError::Failed(run.output_text),
        })
    }
}

#[async_trait]
impl ToolInvoke for GovernedInvoker {
    fn resolves(&self, tool: &str) -> bool {
        denied_tool(tool).is_none() && self.resolve(tool)
    }

    async fn invoke(&self, call: ToolCall<'_>) -> Result<Value, InvokeError> {
        if let Some(reason) = denied_tool(call.tool) {
            tracing::warn!(
                target: "sentinel.audit",
                actor = %call.actor, tool = %call.tool,
                "monitor tool call denied: never-grantable family"
            );
            return Err(InvokeError::Denied(reason));
        }
        if !self.resolve(call.tool) {
            return Err(InvokeError::Denied(format!(
                "no tool named '{}' is registered",
                call.tool
            )));
        }
        let principal = TurnPrincipal {
            run_as: call.owner.map(str::to_string),
            bill_to: None,
        };
        crate::user_config::as_principal(&principal, self.run(call.actor, call.tool, call.input))
            .await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use serde_json::json;
    use workforce_agent::runner::CURRENT_USER_ID;
    use workforce_agent::tools::arguments::ArgumentValidation;
    use workforce_agent::tools::{Tool, ToolResult};
    use workforce_core::types::AgentId;

    use super::*;

    /// Reports the user it ran as, or refuses as its input asks.
    struct Probe {
        name: &'static str,
        approval: Option<String>,
        seen: Mutex<Vec<Option<String>>>,
    }

    #[async_trait]
    impl Tool for Probe {
        fn name(&self) -> &str {
            self.name
        }
        fn description(&self) -> &str {
            "probe"
        }
        fn input_schema(&self) -> Value {
            json!({ "type": "object", "properties": { "mode": { "type": "string" } } })
        }
        fn approval_requirement(&self) -> Option<String> {
            self.approval.clone()
        }
        async fn execute(&self, input: Value) -> eyre::Result<ToolResult> {
            let user = CURRENT_USER_ID
                .try_with(|u| u.clone())
                .ok()
                .filter(|u| !u.is_empty());
            self.seen.lock().unwrap().push(user.clone());
            Ok(match input["mode"].as_str() {
                Some("fail") => ToolResult::failure("upstream broke", 1),
                Some("limit") => ToolResult::rate_limited("too many", Some(7), 1),
                _ => ToolResult::success(json!({ "ran_as": user }), 1),
            })
        }
    }

    const ALLOW_ALL: &str =
        "rules:\n  - name: allow-all\n    scope: global\n    tool: \"*\"\n    action: allow\n";

    fn invoker_with_policy(probe: Arc<Probe>, policy: PolicyEngine) -> GovernedInvoker {
        let tools = Arc::new(ToolRegistry::new());
        tools.register(probe as Arc<dyn Tool>);
        let agent = AgentId::new("atlas");
        GovernedInvoker::new(
            tools,
            Arc::new(policy),
            Arc::new(move || ToolGates {
                agent_id: agent.clone(),
                team_id: None,
                role: None,
                capability_gate: None,
                permission_gate: None,
                arguments: ArgumentValidation::Off,
            }),
            Arc::new(ToolAccessSnapshot::default),
            None,
        )
    }

    fn invoker(probe: Arc<Probe>) -> GovernedInvoker {
        invoker_with_policy(probe, PolicyEngine::from_yaml(ALLOW_ALL).expect("policy"))
    }

    /// Denies every user but the one it names.
    struct OnlyFor(&'static str);

    impl workforce_agent::runner::CapabilityGate for OnlyFor {
        fn deny_reason(&self, user_id: &str, _: &str, tool: &str, _: &Value) -> Option<String> {
            (user_id != self.0).then(|| format!("{user_id} may not call {tool}"))
        }
    }

    fn gated_invoker(probe: Arc<Probe>, blocked: &[(&str, &str)]) -> GovernedInvoker {
        let tools = Arc::new(ToolRegistry::new());
        tools.register(probe as Arc<dyn Tool>);
        let agent = AgentId::new("atlas");
        let blocked: std::collections::HashMap<String, String> = blocked
            .iter()
            .map(|(tool, why)| (tool.to_string(), why.to_string()))
            .collect();
        GovernedInvoker::new(
            tools,
            Arc::new(PolicyEngine::from_yaml(ALLOW_ALL).expect("policy")),
            Arc::new(move || ToolGates {
                agent_id: agent.clone(),
                team_id: None,
                role: None,
                capability_gate: Some(Arc::new(OnlyFor("alice"))),
                permission_gate: None,
                arguments: ArgumentValidation::Off,
            }),
            Arc::new(move || ToolAccessSnapshot::new(blocked.clone())),
            None,
        )
    }

    fn probe(name: &'static str, approval: Option<&str>) -> Arc<Probe> {
        Arc::new(Probe {
            name,
            approval: approval.map(str::to_string),
            seen: Mutex::new(Vec::new()),
        })
    }

    fn call<'a>(owner: Option<&'a str>, tool: &'a str, input: Value) -> ToolCall<'a> {
        ToolCall {
            owner,
            actor: "monitor:test",
            tool,
            input,
        }
    }

    #[tokio::test]
    async fn a_permitted_tool_runs_as_the_owner_and_answers_with_its_output() {
        let probe = probe("jira.search", None);
        let invoker = invoker(Arc::clone(&probe));
        let out = invoker
            .invoke(call(Some("alice"), "jira.search", json!({})))
            .await
            .unwrap();
        assert_eq!(out["ran_as"], "alice");
        let out = invoker
            .invoke(call(None, "jira.search", json!({})))
            .await
            .unwrap();
        assert_eq!(out["ran_as"], Value::Null, "a system monitor runs unscoped");
        assert_eq!(
            *probe.seen.lock().unwrap(),
            vec![Some("alice".to_string()), None]
        );
        assert!(invoker.resolves("jira.search"));
        assert!(!invoker.resolves("jira.nothing"));
    }

    #[tokio::test]
    async fn denied_families_and_unknown_tools_never_reach_the_registry() {
        let probe = probe("vault.get", None);
        let invoker = invoker(Arc::clone(&probe));
        for tool in ["vault.get", "monitor", "exec.run", "workflow"] {
            let err = invoker
                .invoke(call(Some("alice"), tool, json!({})))
                .await
                .unwrap_err();
            assert!(
                matches!(&err, InvokeError::Denied(m) if m.contains("never call")),
                "{tool}: {err:?}"
            );
            assert!(!invoker.resolves(tool));
        }
        let err = invoker
            .invoke(call(Some("alice"), "jira.search", json!({})))
            .await
            .unwrap_err();
        assert_eq!(
            err,
            InvokeError::Denied("no tool named 'jira.search' is registered".into())
        );
        assert!(probe.seen.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn the_capability_gate_scores_the_owner_and_the_access_snapshot_blocks() {
        let probe = probe("jira.search", None);
        let invoker = gated_invoker(Arc::clone(&probe), &[]);
        assert!(invoker
            .invoke(call(Some("alice"), "jira.search", json!({})))
            .await
            .is_ok());
        let err = invoker
            .invoke(call(Some("bob"), "jira.search", json!({})))
            .await
            .unwrap_err();
        assert_eq!(
            err,
            InvokeError::Denied("bob may not call jira.search".into()),
            "the owner, not the saver or the agent, is scored"
        );
        assert_eq!(probe.seen.lock().unwrap().len(), 1);

        let hidden = Arc::new(Probe {
            name: "jira.search",
            approval: None,
            seen: Mutex::new(Vec::new()),
        });
        let invoker = gated_invoker(
            Arc::clone(&hidden),
            &[("jira.search", "hidden by the tool filter")],
        );
        let err = invoker
            .invoke(call(Some("alice"), "jira.search", json!({})))
            .await
            .unwrap_err();
        assert_eq!(err, InvokeError::Denied("hidden by the tool filter".into()));
        assert!(hidden.seen.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn the_policy_engine_fails_closed_before_the_tool_runs() {
        let probe = probe("jira.search", None);
        let invoker = invoker_with_policy(
            Arc::clone(&probe),
            PolicyEngine::new(workforce_policy::rules::PolicyConfig { rules: Vec::new() }),
        );
        let err = invoker
            .invoke(call(Some("alice"), "jira.search", json!({})))
            .await
            .unwrap_err();
        assert!(
            matches!(&err, InvokeError::Denied(m) if m.contains("No policy rule matches")),
            "{err:?}"
        );
        assert!(probe.seen.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_tool_that_needs_approval_is_refused_before_it_runs() {
        let probe = probe("github.pr_merge", Some("merges code"));
        let invoker = invoker(Arc::clone(&probe));
        let err = invoker
            .invoke(call(Some("alice"), "github.pr_merge", json!({})))
            .await
            .unwrap_err();
        assert!(
            matches!(&err, InvokeError::Denied(m) if m.contains("requires a human to approve")),
            "{err:?}"
        );
        assert!(probe.seen.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn failures_and_rate_limits_are_told_apart() {
        let probe = probe("jira.search", None);
        let invoker = invoker(Arc::clone(&probe));
        assert_eq!(
            invoker
                .invoke(call(
                    Some("alice"),
                    "jira.search",
                    json!({ "mode": "fail" })
                ))
                .await
                .unwrap_err(),
            InvokeError::Failed("upstream broke".into())
        );
        let err = invoker
            .invoke(call(
                Some("alice"),
                "jira.search",
                json!({ "mode": "limit" }),
            ))
            .await
            .unwrap_err();
        assert!(matches!(err, InvokeError::RateLimited(_)), "{err:?}");
        assert_eq!(err.message(), "too many");
    }
}
