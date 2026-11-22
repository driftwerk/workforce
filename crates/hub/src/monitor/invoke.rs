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
//!
//! A tool its agent has not loaded yet (a hub just restarted, a skill never
//! enabled) is loaded as the agent would load it itself: a call to
//! `system.enable_skill` for the skill that names that tool is put through
//! the same gauntlet as the monitor's owner — so whether they may enable it
//! decides here too — and the skill is then loaded within their scope, so
//! their own credentials load. The load only adds: nothing already loaded is
//! unloaded, so one owner's monitor never takes another's tools away. A
//! system monitor (no owner) is gated as no person, as its calls are. The
//! call is admitted without running, so whether the agent itself carries
//! `system.enable_skill` does not matter: who may enable a skill is what the
//! gauntlet scores, and the owner's call is scored either way.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};
use workforce_agent::runner::{CURRENT_AGENT_ID, CURRENT_AUTOMATED_ORIGIN};
use workforce_agent::tools::access::ToolAccessSnapshot;
use workforce_agent::tools::dispatch::{execute_gated, unattended_refusal, GateOutcome, ToolGates};
use workforce_agent::tools::registry::ToolRegistry;
use workforce_common::session::TurnPrincipal;
use workforce_policy::engine::PolicyEngine;
use workforce_wasm_runtime::manifest::is_denied_tool;

/// What a tool a monitor calls is told it descends from.
const MONITOR_ORIGIN: &str = "monitor";

/// How a monitor's missing tool is loaded: the agent's own path to it.
const ENABLE_SKILL: &str = "system.enable_skill";

/// How long a load that left a tool missing is answered from memory: what
/// fixes it (an account added to the vault, a capability granted) may
/// register nothing, so it is tried again after this.
const FAILED_LOAD_RETRY: std::time::Duration = std::time::Duration::from_secs(300);

/// Loads a built-in skill into the agent's registry within the scope in
/// force, adding to what is there; what the load said about itself (its
/// context hint), or why it failed.
pub type SkillLoad = Arc<dyn Fn(&str) -> eyre::Result<Option<String>> + Send + Sync>;

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
    /// The agent has no such tool and could not load it for this owner;
    /// asking again changes nothing until a tool registers.
    Unavailable(String),
}

impl InvokeError {
    pub fn message(&self) -> &str {
        match self {
            Self::Denied(m) | Self::Failed(m) | Self::RateLimited(m) | Self::Unavailable(m) => m,
        }
    }
}

/// How a monitor's tool was found on its agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolved {
    /// Already loaded.
    Loaded,
    /// Loaded by enabling `skill` for the agent, as the monitor's owner.
    Enabled { skill: String },
}

impl Resolved {
    /// What a save tells its author about it, if anything.
    pub fn note(&self, tool: &str) -> Option<String> {
        match self {
            Self::Loaded => None,
            Self::Enabled { skill } => Some(format!(
                "Enabled the {skill} skill for this monitor's agent, so it can poll {tool}."
            )),
        }
    }
}

#[async_trait]
pub trait ToolInvoke: Send + Sync {
    /// Find `tool` on the agent as a call by `owner` would, loading its skill
    /// as `owner` when the agent has not loaded it; why not, with the fix.
    /// `actor` is who the load is recorded as.
    async fn resolve(
        &self,
        tool: &str,
        owner: Option<&str>,
        actor: &str,
    ) -> Result<Resolved, String>;

    /// Whether `tool` is loaded on the agent now: a cheap look, loading
    /// nothing, so a monitor that failed for a missing tool can be polled
    /// again as soon as it registers.
    fn has(&self, tool: &str) -> bool;

    /// The tools loaded on the agent now; none where the invoker can't say.
    fn loaded(&self) -> Vec<String> {
        Vec::new()
    }

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
    load: Option<SkillLoad>,
    /// The loads that left a tool missing, by (skill, owner): when, and why.
    /// Until `load_retry` has passed the same load is answered from here, so
    /// a monitor missing its tool does not load the skill on every poll.
    /// Held across a load, so two monitors never load one skill at once.
    failed_loads: tokio::sync::Mutex<FailedLoads>,
    load_retry: std::time::Duration,
}

type FailedLoads = HashMap<(String, Option<String>), (std::time::Instant, String)>;

impl GovernedInvoker {
    pub fn new(
        tools: Arc<ToolRegistry>,
        policy: Arc<PolicyEngine>,
        gates: GatesFn,
        access: AccessFn,
        load: Option<SkillLoad>,
    ) -> Self {
        Self {
            tools,
            policy,
            gates,
            access,
            load,
            failed_loads: tokio::sync::Mutex::default(),
            load_retry: FAILED_LOAD_RETRY,
        }
    }

    /// How long a failed load is answered from memory.
    pub fn with_load_retry(mut self, after: std::time::Duration) -> Self {
        self.load_retry = after;
        self
    }

    /// The invoker for `service`: its registry, policy, gates and access
    /// snapshot read at call time, so a changed capability matrix or tool
    /// filter applies to the next poll; and its vaults to load a skill from.
    pub fn for_service(service: Arc<crate::serve::AgentService>) -> Self {
        let gates = Arc::clone(&service);
        let access = Arc::clone(&service);
        let load: SkillLoad = Arc::new(move |skill: &str| {
            let Some(vault) = service.vault.as_ref() else {
                eyre::bail!("this agent has no vault to load {skill} from");
            };
            let loaded = crate::skill_loader::load_skill_from_vault(
                skill,
                Arc::clone(vault),
                service.global_vault.clone(),
                &service.config,
                &service.tools,
            )?;
            // What every turn reads of the skill stays the agent's: this
            // load adds its tools to those already listed, and its hint (one
            // owner's view) replaces the standing one only when it added any.
            service.skill_state.merge_loaded(
                skill,
                &loaded.tool_names,
                "vault",
                loaded.context_hint.clone(),
            );
            Ok(loaded.context_hint)
        });
        Self::new(
            Arc::clone(&gates.tools),
            Arc::clone(&gates.policy),
            Arc::new(move || gates.tool_gates(crate::desk_surface::allows())),
            // A monitor is no program: the board sync's byte readers are off it.
            Arc::new(move || access.tool_access_snapshot().for_a_turn()),
            Some(load),
        )
    }

    /// The agent's `tool`, loading the skill that names it as `owner` when
    /// the agent has not loaded it; why not, with the fix.
    async fn resolve_for(
        &self,
        tool: &str,
        owner: Option<&str>,
        actor: &str,
    ) -> Result<Resolved, String> {
        if self.tools.get(tool).is_some() {
            return Ok(Resolved::Loaded);
        }
        let agent = (self.gates)().agent_id.as_str().to_string();
        let Some(skill) = crate::skill_loader::skill_of_tool(tool) else {
            return Err(format!(
                "no tool named '{tool}' is registered for agent '{agent}', and no skill provides \
                 one (system.list_tools lists the tools it has)"
            ));
        };
        let key = (skill.to_string(), owner.map(str::to_string));
        let mut failed = self.failed_loads.lock().await;
        // Another monitor's load may have brought it while this one waited.
        if self.tools.get(tool).is_some() {
            failed.remove(&key);
            return Ok(Resolved::Loaded);
        }
        failed.retain(|_, (at, _)| at.elapsed() < self.load_retry);
        if let Some((_, why)) = failed.get(&key) {
            return Err(why.clone());
        }
        let who = owner.map_or_else(|| "the hub".to_string(), |o| format!("user {o}"));
        let result = match self.enable(skill, owner, actor).await {
            Ok(_) if self.tools.get(tool).is_some() => {
                tracing::info!(%agent, tool, skill, owner = owner.unwrap_or(""), "enabled a skill for a monitor's tool");
                Ok(Resolved::Enabled {
                    skill: skill.to_string(),
                })
            }
            Ok(hint) => Err(format!(
                "{tool} isn't loaded for agent '{agent}': enabling the {skill} skill as {who} \
                 didn't add it{}",
                hint.map(|h| format!(" ({})", h.trim())).unwrap_or_default()
            )),
            Err(why) => Err(format!(
                "{tool} isn't loaded for agent '{agent}', and enabling the {skill} skill as {who} \
                 failed: {why}. Fix: enable it from a turn that may (system.enable_skill {skill})"
            )),
        };
        if let Err(why) = &result {
            failed.insert(key, (std::time::Instant::now(), why.clone()));
        }
        result
    }

    /// Enable `skill` for the agent as `owner`: the `system.enable_skill`
    /// call that would do it goes through the gauntlet as theirs would, and
    /// the skill is then loaded within their scope, adding to what the agent
    /// has. What the load said about itself, or why it was refused or failed.
    async fn enable(
        &self,
        skill: &str,
        owner: Option<&str>,
        actor: &str,
    ) -> Result<Option<String>, String> {
        let Some(load) = self.load.clone() else {
            return Err("this agent loads no skills".to_string());
        };
        let principal = TurnPrincipal {
            run_as: owner.map(str::to_string),
            bill_to: None,
        };
        crate::user_config::as_principal(
            &principal,
            self.admit(actor, ENABLE_SKILL, json!({ "skill": skill })),
        )
        .await
        .map_err(|e| e.message().to_string())?;
        let (skill, user) = (skill.to_string(), owner.unwrap_or_default().to_string());
        // A load reads the vault synchronously: off the async workers, in
        // the owner's scope as the loader reads it.
        tokio::task::spawn_blocking(move || {
            workforce_agent::runner::CURRENT_USER_ID.sync_scope(user, || load(&skill))
        })
        .await
        .map_err(|e| format!("the load stopped: {e}"))?
        .map_err(|e| format!("{e:#}"))
    }

    /// Put a call to `tool` through the gauntlet without running it: the
    /// input it would run with, or why not.
    async fn admit(
        &self,
        actor: &str,
        tool: &str,
        input: Value,
    ) -> Result<Option<Value>, InvokeError> {
        if let Some(refusal) = unattended_refusal(&self.tools, tool) {
            return Err(InvokeError::Denied(refusal));
        }
        let gates = (self.gates)();
        // A monitor asks nobody: what the computer gate would ask about runs
        // or is refused as its agent's `[computer] unattended` says.
        let (outcome, decided) = workforce_agent::tools::dispatch::gate_tool_call_unattended(
            &gates,
            &self.policy,
            &self.tools,
            tool,
            &input,
        );
        workforce_agent::tools::computer::settle_gated(
            gates.computer_gate.as_deref(),
            tool,
            decided.as_ref(),
            &input,
            &outcome,
        );
        let exec_input = match outcome {
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
        if let Some(reason) = (self.access)().block_reason(tool) {
            tracing::warn!(target: "sentinel.audit", actor = %actor, tool = %tool, reason = %reason, "monitor tool call denied");
            return Err(InvokeError::Denied(reason.to_string()));
        }
        Ok(exec_input)
    }

    async fn run(&self, actor: &str, tool: &str, input: Value) -> Result<Value, InvokeError> {
        let exec_input = self.admit(actor, tool, input.clone()).await?;
        let gates = (self.gates)();
        tracing::info!(target: "sentinel.audit", actor = %actor, tool = %tool, "monitor tool call dispatched");
        let run = CURRENT_AGENT_ID
            .scope(
                gates.agent_id.as_str().to_string(),
                CURRENT_AUTOMATED_ORIGIN.scope(
                    MONITOR_ORIGIN.to_string(),
                    execute_gated(&self.tools, &(self.access)(), tool, &input, exec_input),
                ),
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
    async fn resolve(
        &self,
        tool: &str,
        owner: Option<&str>,
        actor: &str,
    ) -> Result<Resolved, String> {
        if let Some(reason) =
            denied_tool(tool).or_else(|| workforce_agent::tools::access::program_only_refusal(tool))
        {
            return Err(reason);
        }
        self.resolve_for(tool, owner, actor).await
    }

    fn has(&self, tool: &str) -> bool {
        self.tools.get(tool).is_some()
    }

    fn loaded(&self) -> Vec<String> {
        self.tools.names()
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
        self.resolve_for(call.tool, call.owner, call.actor)
            .await
            .map_err(InvokeError::Unavailable)?;
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
                surface_allows: None,
                computer_gate: None,
                arguments: ArgumentValidation::Off,
                content_guard: None,
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
                surface_allows: None,
                computer_gate: None,
                arguments: ArgumentValidation::Off,
                content_guard: None,
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
        assert_eq!(
            invoker.resolve("jira.search", None, "monitor:test").await,
            Ok(Resolved::Loaded)
        );
        assert!(invoker
            .resolve("nothing.at_all", None, "monitor:test")
            .await
            .unwrap_err()
            .contains("no skill provides"));
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
            assert!(invoker
                .resolve(tool, Some("alice"), "monitor:test")
                .await
                .unwrap_err()
                .contains("never call"));
        }
        let err = invoker
            .invoke(call(Some("alice"), "jira.search", json!({})))
            .await
            .unwrap_err();
        assert!(
            matches!(&err, InvokeError::Unavailable(m)
                if m.starts_with("jira.search isn't loaded for agent 'atlas'")
                    && m.contains("loads no skills")),
            "{err:?}"
        );
        assert!(probe.seen.lock().unwrap().is_empty());
    }

    /// The board sync's byte readers are a program's: a monitor never calls
    /// one, nor is offered it, though its agent's registry holds it.
    #[tokio::test]
    async fn a_monitor_never_calls_a_program_s_tools() {
        for name in ["jira.attachment_read", "linear.upload_read"] {
            let probe = probe(name, None);
            let invoker = invoker(Arc::clone(&probe));
            assert!(
                invoker
                    .resolve(name, Some("alice"), "monitor:test")
                    .await
                    .is_err(),
                "{name}"
            );
            let err = invoker
                .invoke(call(Some("alice"), name, json!({ "attachment_id": "1" })))
                .await
                .unwrap_err();
            assert_eq!(
                err,
                InvokeError::Denied(workforce_agent::tools::access::PROGRAM_ONLY.into()),
                "{name}"
            );
            assert!(probe.seen.lock().unwrap().is_empty(), "{name} never ran");
        }
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

    /// Loads `email` into `tools` for whoever is in scope, adding to what
    /// is there: `email.inbox` only for a person whose accounts it can read
    /// (alice), else the setup wizard alone — as a load outside the
    /// person's scope does. Records who each load ran as.
    fn email_loader(
        tools: &Arc<ToolRegistry>,
        ran_as: &Arc<Mutex<Vec<Option<String>>>>,
    ) -> SkillLoad {
        let (tools, ran_as) = (Arc::clone(tools), Arc::clone(ran_as));
        Arc::new(move |skill: &str| {
            assert_eq!(skill, "email", "only the tool's own skill");
            let user = CURRENT_USER_ID
                .try_with(|u| u.clone())
                .ok()
                .filter(|u| !u.is_empty());
            ran_as.lock().unwrap().push(user.clone());
            tools.register(probe("email.setup", None) as Arc<dyn Tool>);
            if user.as_deref() == Some("alice") {
                tools.register(probe("email.inbox", None) as Arc<dyn Tool>);
                return Ok(None);
            }
            Ok(Some(
                "No email accounts configured. Use email.setup to add one.".into(),
            ))
        })
    }

    struct Loads(Arc<Mutex<Vec<Option<String>>>>);

    impl Loads {
        fn len(&self) -> usize {
            self.0.lock().unwrap().len()
        }
    }

    /// An invoker for agent kat with no email loaded yet; only `allowed`
    /// may call `system.enable_skill`.
    fn enabling_invoker(allowed: &'static str) -> (GovernedInvoker, Loads) {
        let tools = Arc::new(ToolRegistry::new());
        let ran_as = Arc::new(Mutex::new(Vec::new()));
        let load = email_loader(&tools, &ran_as);
        let agent = AgentId::new("kat");
        let invoker = GovernedInvoker::new(
            tools,
            Arc::new(PolicyEngine::from_yaml(ALLOW_ALL).expect("policy")),
            Arc::new(move || ToolGates {
                agent_id: agent.clone(),
                team_id: None,
                role: None,
                capability_gate: Some(Arc::new(OnlyFor(allowed))),
                permission_gate: None,
                surface_allows: None,
                computer_gate: None,
                arguments: ArgumentValidation::Off,
                content_guard: None,
            }),
            Arc::new(ToolAccessSnapshot::default),
            Some(load),
        );
        (invoker, Loads(ran_as))
    }

    /// The incident: the agent had not loaded email (a restart), so its
    /// monitor's poll enables the skill as the monitor's owner — whose
    /// accounts then load — and reads their inbox, with no re-save.
    #[tokio::test]
    async fn a_tool_the_agent_has_not_loaded_is_loaded_as_the_owner_and_runs() {
        let (invoker, loads) = enabling_invoker("alice");
        let out = invoker
            .invoke(call(Some("alice"), "email.inbox", json!({})))
            .await
            .unwrap();
        assert_eq!(out["ran_as"], "alice");
        assert_eq!(*loads.0.lock().unwrap(), vec![Some("alice".into())]);

        // Loaded now: a save resolves it without loading it again.
        assert_eq!(
            invoker
                .resolve("email.inbox", Some("alice"), "monitor:test")
                .await,
            Ok(Resolved::Loaded)
        );
        assert_eq!(loads.len(), 1);
    }

    #[tokio::test]
    async fn a_save_says_which_skill_it_enabled() {
        let (invoker, _) = enabling_invoker("alice");
        let resolved = invoker
            .resolve("email.inbox", Some("alice"), "monitor:test")
            .await
            .unwrap();
        assert_eq!(
            resolved,
            Resolved::Enabled {
                skill: "email".into()
            }
        );
        assert!(resolved
            .note("email.inbox")
            .unwrap()
            .contains("email skill"));
    }

    /// Who may enable a skill decides here as it does for their own call.
    #[tokio::test]
    async fn a_tool_whose_skill_the_owner_may_not_enable_is_refused_with_the_fix() {
        let (invoker, loads) = enabling_invoker("alice");
        let why = invoker
            .resolve("email.inbox", Some("bob"), "monitor:test")
            .await
            .unwrap_err();
        assert!(
            why.starts_with("email.inbox isn't loaded for agent 'kat', and enabling the email skill as user bob failed: bob may not call system.enable_skill"),
            "{why}"
        );
        assert!(why.contains("system.enable_skill email"), "{why}");
        assert_eq!(loads.len(), 0, "never loaded");
    }

    /// A skill that loads without the tool (no account) says why, and is
    /// not loaded again on every poll, whatever else registers meanwhile;
    /// once someone's load brings the tool, it is there for them too.
    #[tokio::test]
    async fn a_load_that_leaves_the_tool_missing_says_why_and_is_not_repeated() {
        let (invoker, loads) = enabling_invoker("carol");
        let why = invoker
            .resolve("email.inbox", Some("carol"), "monitor:test")
            .await
            .unwrap_err();
        assert_eq!(
            why,
            "email.inbox isn't loaded for agent 'kat': enabling the email skill as user carol \
             didn't add it (No email accounts configured. Use email.setup to add one.)"
        );
        let err = invoker
            .invoke(call(Some("carol"), "email.inbox", json!({})))
            .await
            .unwrap_err();
        assert_eq!(err, InvokeError::Unavailable(why));
        invoker
            .tools
            .register(probe("jira.search", None) as Arc<dyn Tool>);
        assert!(invoker
            .resolve("email.inbox", Some("carol"), "monitor:test")
            .await
            .is_err());
        assert_eq!(loads.len(), 1, "loaded once");

        invoker
            .tools
            .register(probe("email.inbox", None) as Arc<dyn Tool>);
        assert!(invoker.has("email.inbox"));
        assert_eq!(
            invoker
                .resolve("email.inbox", Some("carol"), "monitor:test")
                .await,
            Ok(Resolved::Loaded)
        );
        assert_eq!(loads.len(), 1);
    }

    /// A system monitor (no owner) is gated as no person, as its calls
    /// are: the capability gate scores users, and only an administrator may
    /// save one.
    #[tokio::test]
    async fn a_system_monitor_loads_as_no_person() {
        let (invoker, loads) = enabling_invoker("alice");
        let why = invoker
            .resolve("email.inbox", None, "monitor:system/inbox")
            .await
            .unwrap_err();
        assert!(
            why.contains("enabling the email skill as the hub didn't add it"),
            "{why}"
        );
        assert_eq!(*loads.0.lock().unwrap(), vec![None]);
    }

    /// Only the skill that names the tool is loaded: a name its family does
    /// not list loads nothing.
    #[tokio::test]
    async fn a_tool_no_skill_names_loads_nothing() {
        let (invoker, loads) = enabling_invoker("alice");
        let why = invoker
            .resolve("email.inbx", Some("alice"), "monitor:test")
            .await
            .unwrap_err();
        assert!(why.contains("no skill provides one"), "{why}");
        assert_eq!(loads.len(), 0);
    }

    /// What fixes a failed load may register nothing (an account added to
    /// the vault): it is tried again once its time is up all the same.
    #[tokio::test]
    async fn a_failed_load_is_tried_again_in_time_though_nothing_registers() {
        let (invoker, loads) = enabling_invoker("carol");
        let invoker = invoker.with_load_retry(std::time::Duration::ZERO);
        for _ in 0..2 {
            assert!(invoker
                .resolve("email.inbox", Some("carol"), "monitor:test")
                .await
                .is_err());
        }
        assert_eq!(loads.len(), 2);
    }

    /// The incident on a real agent: the person's email account is theirs
    /// alone (`users/<them>/…`), the agent's email was loaded outside their
    /// scope (setup wizard only), and the monitor's path — the agent's own
    /// registry — enables the skill again as them, so their inbox tool is
    /// there to poll.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_persons_own_email_account_loads_on_the_agent_for_their_monitor() {
        let tmp = tempfile::tempdir().unwrap();
        let service = crate::serve::testing::plain_service(tmp.path()).await;
        let vault = service.vault.clone().expect("agent vault");
        for (key, value) in [
            ("username", "me@example.com"),
            ("password", "pw"),
            ("imap_host", "127.0.0.1"),
            ("smtp_host", "127.0.0.1"),
        ] {
            vault
                .set(
                    &format!("users/alice/skills/email/accounts/default/{key}"),
                    value,
                )
                .unwrap();
        }
        // As the hub left it: email loaded outside anyone's scope, where no
        // account of theirs is seen, so with its setup wizard alone.
        crate::skill_loader::load_and_track(
            "email",
            Arc::clone(&vault),
            service.global_vault.clone(),
            &service.config,
            &service.tools,
            &service.skill_state,
            "vault",
        )
        .unwrap();
        assert!(service.tools.get("email.setup").is_some());
        assert!(
            service.tools.get("email.inbox").is_none(),
            "the incident's state"
        );
        let invoker = GovernedInvoker::for_service(Arc::clone(&service));

        let resolved = invoker
            .resolve("email.inbox", Some("alice"), "monitor:alice/inbox")
            .await;
        assert_eq!(
            resolved,
            Ok(Resolved::Enabled {
                skill: "email".into()
            })
        );
        assert!(service.tools.get("email.inbox").is_some());

        // Another owner's monitors leave alice's tools be: one on a tool the
        // skill doesn't name, and one with no account of their own, which
        // finds the tool loaded and runs it as themselves.
        assert!(invoker
            .resolve("email.inbx", Some("bob"), "monitor:bob/typo")
            .await
            .is_err());
        assert_eq!(
            invoker
                .resolve("email.inbox", Some("bob"), "monitor:bob/inbox")
                .await,
            Ok(Resolved::Loaded)
        );
        assert!(service.tools.get("email.inbox").is_some(), "alice's stays");
    }
}
