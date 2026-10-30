//! Governed tool dispatch for WASM skills — the hub-side implementation of
//! [`SkillToolInvoker`] behind `wf_tool_invoke`.
//!
//! By the time a call reaches this pipeline the host has already enforced the
//! manifest declaration, the user's build-pinned tools grant, the per-
//! invocation budget, and the secret-marker ban. This side runs everything an
//! agent-originated call would face — the capability gate under the calling
//! user's identity, the policy engine, sentinel audit — plus the two rules
//! specific to skill callers: the denied-family recheck (including
//! skill-to-skill calls) and the per-permission URL-host constraint. An
//! `Ask`-tier decision denies: a synchronous WASM call cannot block on a
//! human, so approval-gated tools need a matrix pre-allow instead.

use std::sync::Arc;

use serde_json::Value;
use workforce_agent::tools::registry::ToolRegistry;
use workforce_agent::tools::ToolResult;
use workforce_core::policy::{PolicyContext, PolicyDecision, PolicyRequest};
use workforce_core::types::AgentId;
use workforce_identity::store::IdentityStore;
use workforce_policy::engine::PolicyEngine;
use workforce_wasm_runtime::manifest::{
    host_matches_allowlist, is_denied_tool, ManifestPermission,
};
use workforce_wasm_runtime::types::{SkillCallContext, SkillToolInvoker};

use crate::authz::{authorize, tool_capability, CapabilityMatrixHandle, Decision};
use crate::cap_gate::{fallback_principal, resolve_principal};

/// Longest a skill-invoked platform tool may run.
const TOOL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// Total attempts for a rate-limited call: the original plus two retries.
const RATE_LIMIT_ATTEMPTS: u32 = 3;

/// Base backoff before a rate-limited retry; doubles per attempt (2s, 4s).
const RATE_LIMIT_BACKOFF: std::time::Duration = std::time::Duration::from_secs(2);

/// Longest a rate-limited retry will wait, however large the server's
/// Retry-After hint is.
const MAX_RATE_LIMIT_WAIT: std::time::Duration = std::time::Duration::from_secs(60);

/// In-flight calls a `wf_tool_invoke_many` batch may hold at once PER TOOL
/// FAMILY (the `datadog` in `datadog.logs_search`). Bounded so a fan-out
/// doesn't burst-fire every element into the same upstream rate window at
/// t=0; scoped per family so a throttled service serializes only its own
/// elements — the cross-service fan-out the API exists for is unaffected.
const MAX_BATCH_CONCURRENCY: usize = 2;

/// Loads the native adapter behind a tool family (`jira`, `linear`, …) that
/// registers lazily, so a skill granted one of its tools can call it before
/// anything else in the process has loaded it.
pub type AdapterLoader = Arc<dyn Fn(&str) -> eyre::Result<()> + Send + Sync>;

pub struct HubSkillToolInvoker {
    registry: Arc<ToolRegistry>,
    policy: Arc<PolicyEngine>,
    identity_store: Arc<parking_lot::RwLock<Option<Arc<dyn IdentityStore>>>>,
    matrix: CapabilityMatrixHandle,
    handle: tokio::runtime::Handle,
    rate_limit_backoff: std::time::Duration,
    adapter_loader: Option<AdapterLoader>,
}

impl HubSkillToolInvoker {
    pub fn new(
        registry: Arc<ToolRegistry>,
        policy: Arc<PolicyEngine>,
        identity_store: Arc<parking_lot::RwLock<Option<Arc<dyn IdentityStore>>>>,
        matrix: CapabilityMatrixHandle,
        handle: tokio::runtime::Handle,
    ) -> Self {
        Self {
            registry,
            policy,
            identity_store,
            matrix,
            handle,
            rate_limit_backoff: RATE_LIMIT_BACKOFF,
            adapter_loader: None,
        }
    }
}

/// Whether `tool` belongs to an installed WASM skill (`{skill}.{tool}`).
/// Skills may not invoke other skills — recursion and privilege laundering.
fn is_wasm_skill_tool(tool: &str) -> bool {
    crate::wasm_skill_tools::global_skill_store().is_some_and(|store| {
        store
            .list_installed()
            .iter()
            .any(|s| tool.starts_with(&format!("{}.", s.manifest.name)))
    })
}

/// Every string anywhere in `value` that parses as an absolute http(s) URL.
fn urls_in(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::String(s) => {
            if let Ok(url) = reqwest::Url::parse(s) {
                if matches!(url.scheme(), "http" | "https") {
                    if let Some(host) = url.host_str() {
                        out.push(host.to_string());
                    }
                }
            }
        }
        Value::Array(items) => items.iter().for_each(|v| urls_in(v, out)),
        Value::Object(map) => map.values().for_each(|v| urls_in(v, out)),
        _ => {}
    }
}

/// The platform result envelope handed to the guest: tool-level failures are
/// data the skill can branch on, not policy denials.
fn envelope(success: bool, output: Value, error: Option<String>) -> Value {
    serde_json::json!({ "success": success, "output": output, "error": error })
}

/// [`envelope`] plus the typed rate-limit refusal, so skills can branch
/// without sniffing prose.
fn result_envelope(result: ToolResult) -> Value {
    let mut value = envelope(result.success, result.output, result.error);
    if let Some(limit) = result.rate_limited {
        value["rate_limited"] = serde_json::json!({ "retry_after_secs": limit.retry_after_secs });
    }
    value
}

/// The wait before retry `attempt`: exponential backoff from `base`, raised
/// to the server's Retry-After hint when the refusal carries one, capped at
/// [`MAX_RATE_LIMIT_WAIT`]. A fixed 2s/4s ladder alone loses whenever the
/// upstream window resets later than that — every retry then burns inside the
/// same throttled window.
fn retry_wait(
    base: std::time::Duration,
    attempt: u32,
    retry_after_secs: Option<u64>,
) -> std::time::Duration {
    let backoff = base * 2u32.pow(attempt.saturating_sub(1));
    retry_after_secs
        .map(std::time::Duration::from_secs)
        .map_or(backoff, |hint| hint.max(backoff))
        .min(MAX_RATE_LIMIT_WAIT)
}

impl HubSkillToolInvoker {
    /// The synchronous gate every skill-originated call must pass before
    /// dispatch: never-grantable families (including skill-to-skill), the
    /// permission's URL-host constraint, the calling user's capability
    /// matrix, and the policy engine. `Err` is the denial text.
    fn prepare(
        &self,
        ctx: &SkillCallContext,
        permission: &ManifestPermission,
        args: &Value,
    ) -> Result<(), String> {
        let tool = permission.tool.as_str();
        let actor = format!("wasm-skill:{}", ctx.skill_name);

        if is_denied_tool(tool) || is_wasm_skill_tool(tool) {
            tracing::warn!(
                target: "sentinel.audit",
                actor = %actor, tool = %tool, user = ?ctx.user_id,
                "skill tool invocation denied: tool family is never grantable"
            );
            return Err(format!(
                "Tool '{tool}' is in a family that can never be granted to a skill \
                 (admin/vault/execution surface and other skills are out of reach)"
            ));
        }

        if !permission.allowed_hosts.is_empty() {
            let mut hosts = Vec::new();
            urls_in(args, &mut hosts);
            if let Some(bad) = hosts
                .iter()
                .find(|h| !host_matches_allowlist(&permission.allowed_hosts, h))
            {
                tracing::warn!(
                    target: "sentinel.audit",
                    actor = %actor, tool = %tool, host = %bad,
                    "skill tool invocation denied: URL host outside the permission's allowed_hosts"
                );
                return Err(format!(
                    "Argument URL host '{bad}' is outside this permission's allowed_hosts \
                     ({}) — declared in Skill.toml [[permissions]]",
                    permission.allowed_hosts.join(", ")
                ));
            }
        }

        if let Some(cap) = tool_capability(tool, args) {
            let user_id = ctx.user_id.as_deref().unwrap_or("");
            let principal = resolve_principal(self.identity_store.read().as_ref(), user_id)
                .unwrap_or_else(fallback_principal);
            let matrix = self.matrix.read().clone();
            match authorize(&principal, cap, &matrix) {
                Decision::Allow => {}
                Decision::Deny => {
                    tracing::warn!(
                        target: "sentinel.audit",
                        actor = %actor, tool = %tool, user = %user_id, capability = cap.key(),
                        "skill tool invocation denied: caller lacks the capability"
                    );
                    crate::permissions_audit::emit_capability_deny(
                        ctx.user_id.as_deref(),
                        tool,
                        cap.key(),
                    );
                    return Err(format!(
                        "The calling user lacks the '{}' capability required by {tool} — \
                         a skill acts with the invoking user's permissions, never more",
                        cap.key()
                    ));
                }
                Decision::Ask => {
                    tracing::warn!(
                        target: "sentinel.audit",
                        actor = %actor, tool = %tool, user = %user_id, capability = cap.key(),
                        "skill tool invocation denied: capability requires interactive approval"
                    );
                    return Err(format!(
                        "{tool} requires interactive approval ('{}'), which a skill cannot \
                         prompt for — an admin can pre-allow it for this user's role in the \
                         capability matrix",
                        cap.key()
                    ));
                }
            }
        }

        // A write a skill makes outside the Desk conversation a person is in
        // needs their say, as the model's own call would: a destination they
        // always allow passes, and otherwise, as a skill cannot ask, it is
        // refused like any call that needs an approval.
        let effect =
            workforce_agent::tools::dispatch::surface_effect_of(&self.registry, tool, args);
        let policy_response = workforce_agent::tools::dispatch::apply_surface_gate(
            self.policy.evaluate(&PolicyRequest {
                agent_id: AgentId::new(&ctx.agent_id),
                tool_name: tool.to_string(),
                parameters: args.clone(),
                context: PolicyContext::empty(),
            }),
            effect.as_ref(),
            workforce_agent::runner::current_desk().as_ref(),
            args,
            || {
                workforce_agent::tools::dispatch::kept(
                    crate::desk_surface::allows().as_deref(),
                    ctx.user_id.as_deref(),
                    &ctx.agent_id,
                    effect.as_ref().and_then(|e| e.destination()).as_ref(),
                )
            },
        );
        match policy_response.decision {
            PolicyDecision::Allow => {}
            PolicyDecision::Deny { reason } => {
                tracing::warn!(
                    target: "sentinel.audit",
                    actor = %actor, tool = %tool, reason = %reason,
                    "skill tool invocation denied by policy"
                );
                return Err(format!("Denied by policy: {reason}"));
            }
            PolicyDecision::RequireApproval { reason, .. } => {
                tracing::warn!(
                    target: "sentinel.audit",
                    actor = %actor, tool = %tool, reason = %reason,
                    "skill tool invocation denied: policy requires approval"
                );
                return Err(format!(
                    "Policy requires approval for {tool} ({reason}), which a skill cannot \
                     prompt for"
                ));
            }
        }
        Ok(())
    }

    /// Execute a gated call and shape the platform envelope. Rate-limited
    /// failures (the request was refused, so nothing executed) are retried
    /// with exponential backoff; the guest's epoch deadline is re-armed
    /// after the host call returns, so the added wall time never starves it.
    /// Attach the loader that brings a lazily registered adapter into the
    /// registry on a skill's first call to one of its tools.
    pub fn with_adapter_loader(mut self, loader: AdapterLoader) -> Self {
        self.adapter_loader = Some(loader);
        self
    }

    /// The registered tool, loading its family's adapter first when nothing
    /// in the process has yet.
    fn resolve(&self, tool: &str) -> Option<Arc<dyn workforce_agent::tools::Tool>> {
        self.registry.get(tool).or_else(|| {
            let loader = self.adapter_loader.as_ref()?;
            let family = tool.split('.').next()?;
            match loader(family) {
                Ok(()) => self.registry.get(tool),
                Err(e) => {
                    tracing::warn!(tool = %tool, family = %family, error = %e, "adapter for a skill tool could not be loaded");
                    None
                }
            }
        })
    }

    async fn run(&self, ctx: &SkillCallContext, tool: &str, args: Value) -> Value {
        let actor = format!("wasm-skill:{}", ctx.skill_name);
        tracing::info!(
            target: "sentinel.audit",
            actor = %actor, tool = %tool, user = ?ctx.user_id,
            fingerprint = %ctx.grant_fingerprint,
            "skill tool invocation dispatched"
        );

        let Some(registered) = self.resolve(tool) else {
            return envelope(
                false,
                Value::Null,
                Some(format!("no tool named '{tool}' is registered")),
            );
        };
        let mut attempt = 1;
        let outcome = loop {
            let started = std::time::Instant::now();
            let result = tokio::time::timeout(TOOL_TIMEOUT, registered.execute(args.clone())).await;
            let elapsed_ms = started.elapsed().as_millis() as u64;
            // A timeout may have executed, so it is never retried; only the
            // typed refusal signal is — prose about rate limits proves nothing
            // about whether side effects committed.
            let result = match result {
                Err(_) => ToolResult::failure(
                    format!("{tool} timed out after {}s", TOOL_TIMEOUT.as_secs()),
                    elapsed_ms,
                ),
                Ok(Err(report)) => ToolResult::from_report(&report, elapsed_ms),
                Ok(Ok(tool_result)) => tool_result,
            };
            let refusal = result.rate_limited.filter(|_| !result.success);
            if let Some(limit) = refusal {
                if attempt < RATE_LIMIT_ATTEMPTS {
                    let wait = retry_wait(self.rate_limit_backoff, attempt, limit.retry_after_secs);
                    // Batch elements hit the same org-wide limit together; jitter
                    // de-synchronises their retries so they don't re-collide. The
                    // cap bounds the jittered total, keeping MAX_RATE_LIMIT_WAIT's
                    // promise, and the audit line reports the sleep actually taken.
                    let jitter =
                        wait.mul_f64(rand::Rng::gen_range(&mut rand::thread_rng(), 0.0..0.25));
                    let sleep = (wait + jitter).min(MAX_RATE_LIMIT_WAIT);
                    tracing::info!(
                        target: "sentinel.audit",
                        actor = %actor, tool = %tool, attempt, wait_ms = sleep.as_millis() as u64,
                        "skill tool call rate-limited; backing off before retry"
                    );
                    tokio::time::sleep(sleep).await;
                    attempt += 1;
                    continue;
                }
            }
            break result_envelope(result);
        };
        outcome
    }
}

impl SkillToolInvoker for HubSkillToolInvoker {
    fn invoke(
        &self,
        ctx: &SkillCallContext,
        permission: &ManifestPermission,
        args: &Value,
    ) -> Result<Value, String> {
        self.prepare(ctx, permission, args)?;
        Ok(self
            .handle
            .block_on(self.run(ctx, &permission.tool, args.clone())))
    }

    fn invoke_many(
        &self,
        ctx: &SkillCallContext,
        calls: Vec<(ManifestPermission, Value)>,
    ) -> Vec<Result<Value, String>> {
        use futures::StreamExt as _;
        // Elements grouped by tool family share an upstream rate window, so
        // each family runs `buffered(MAX_BATCH_CONCURRENCY)` — an element
        // sleeping out a rate-limit retry holds its family's slot, throttling
        // only siblings behind the same window. Families run concurrently, so
        // batch wall time stays ~= the slowest service, and each element is
        // gated at dispatch (not batch admission): behind retry sleeps a
        // dispatch can trail t=0 by minutes, and the authorization decision
        // should be no staler than that.
        let total = calls.len();
        let mut families: Vec<(String, Vec<(usize, ManifestPermission, Value)>)> = Vec::new();
        for (index, (permission, args)) in calls.into_iter().enumerate() {
            let family = permission
                .tool
                .split('.')
                .next()
                .unwrap_or_default()
                .to_string();
            match families.iter_mut().find(|(f, _)| *f == family) {
                Some((_, elems)) => elems.push((index, permission, args)),
                None => families.push((family, vec![(index, permission, args)])),
            }
        }
        let outcomes: Vec<Vec<(usize, Result<Value, String>)>> =
            self.handle
                .block_on(futures::future::join_all(families.into_iter().map(
                    |(_, elems)| async {
                        futures::stream::iter(elems.into_iter().map(
                            |(index, permission, args)| async move {
                                let outcome = match self.prepare(ctx, &permission, &args) {
                                    Ok(()) => Ok(self.run(ctx, &permission.tool, args).await),
                                    Err(denial) => Err(denial),
                                };
                                (index, outcome)
                            },
                        ))
                        .buffered(MAX_BATCH_CONCURRENCY)
                        .collect()
                        .await
                    },
                )));
        let mut slots: Vec<Option<Result<Value, String>>> = (0..total).map(|_| None).collect();
        for (index, outcome) in outcomes.into_iter().flatten() {
            slots[index] = Some(outcome);
        }
        slots
            .into_iter()
            .map(|slot| slot.expect("every batch element yields exactly one outcome"))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use workforce_agent::tools::{Tool, ToolResult};

    fn invoker_with(registry: Arc<ToolRegistry>) -> HubSkillToolInvoker {
        let runtime =
            tokio::runtime::Handle::try_current().expect("tests run under a tokio runtime");
        HubSkillToolInvoker::new(
            registry,
            Arc::new(
                PolicyEngine::from_yaml(
                    "rules:\n  - name: allow-all\n    scope: global\n    tool: \"*\"\n    action: allow\n",
                )
                .unwrap(),
            ),
            Arc::new(parking_lot::RwLock::new(None)),
            Arc::new(parking_lot::RwLock::new(Arc::new(
                crate::authz::CapabilityMatrix::default(),
            ))),
            runtime,
        )
    }

    fn ctx() -> SkillCallContext {
        SkillCallContext {
            skill_name: "enabler".into(),
            grant_fingerprint: "fp".into(),
            agent_id: "allan".into(),
            user_id: None,
        }
    }

    fn permission(tool: &str, hosts: &[&str]) -> ManifestPermission {
        ManifestPermission {
            tool: tool.into(),
            description: "test".into(),
            allowed_hosts: hosts.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[tokio::test]
    async fn a_tool_whose_adapter_is_not_loaded_yet_is_loaded_on_first_call() {
        let registry = Arc::new(ToolRegistry::new());
        let loaded = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let loader: AdapterLoader = {
            let registry = Arc::clone(&registry);
            let loaded = Arc::clone(&loaded);
            Arc::new(move |family: &str| {
                assert_eq!(family, "echo");
                loaded.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                registry.register(Arc::new(Echo));
                Ok(())
            })
        };
        let invoker = invoker_with(Arc::clone(&registry)).with_adapter_loader(loader);
        let out = invoker
            .run(&ctx(), "echo.say", serde_json::json!({ "hello": "world" }))
            .await;
        assert_eq!(out["success"], true, "{out}");
        assert_eq!(loaded.load(std::sync::atomic::Ordering::SeqCst), 1);

        let out = invoker.run(&ctx(), "echo.say", serde_json::json!({})).await;
        assert_eq!(out["success"], true);
        assert_eq!(loaded.load(std::sync::atomic::Ordering::SeqCst), 1);

        let failing: AdapterLoader = Arc::new(|_| Err(eyre::eyre!("no credentials")));
        let invoker = invoker_with(Arc::new(ToolRegistry::new())).with_adapter_loader(failing);
        let out = invoker
            .run(&ctx(), "jira.search", serde_json::json!({}))
            .await;
        assert_eq!(out["success"], false);
        assert!(out["error"].as_str().unwrap().contains("no tool named"));
    }

    #[derive(Debug)]
    struct Echo;

    #[async_trait]
    impl Tool for Echo {
        fn name(&self) -> &str {
            "echo.say"
        }
        fn description(&self) -> &str {
            "echo"
        }
        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        async fn execute(&self, input: Value) -> eyre::Result<ToolResult> {
            Ok(ToolResult::success(input, 1))
        }
    }

    #[derive(Debug)]
    struct SlackPost;

    #[async_trait]
    impl Tool for SlackPost {
        fn name(&self) -> &str {
            "slack.send"
        }
        fn description(&self) -> &str {
            "post"
        }
        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        async fn execute(&self, input: Value) -> eyre::Result<ToolResult> {
            Ok(ToolResult::success(input, 1))
        }
    }

    /// A skill posting to Slack from a Desk turn needs the person's say,
    /// which a skill cannot ask for, so it is refused; from anywhere else it
    /// posts as before.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_skill_s_slack_post_from_a_desk_turn_is_refused_and_elsewhere_runs() {
        use workforce_common::channels::desk::{DeskPlace, DeskScope, DeskTarget};
        let registry = Arc::new(ToolRegistry::new());
        registry.register(Arc::new(SlackPost));
        let invoker = Arc::new(invoker_with(registry));
        let args = serde_json::json!({"channel": "C1", "text": "hi"});
        let desk = DeskScope {
            target: DeskTarget::of_turn(&DeskPlace::Dm, "desk", None, false),
            attended: true,
            owner: "person-1".into(),
        };
        let (on_desk, elsewhere) = {
            let invoker = Arc::clone(&invoker);
            tokio::task::spawn_blocking(move || {
                let permission = permission("slack.send", &[]);
                let on_desk = workforce_agent::runner::CURRENT_DESK
                    .sync_scope(Some(desk), || invoker.invoke(&ctx(), &permission, &args));
                let elsewhere = invoker.invoke(&ctx(), &permission, &args);
                (on_desk, elsewhere)
            })
            .await
            .unwrap()
        };
        let refused = on_desk.unwrap_err();
        assert!(refused.contains("which a skill cannot"), "{refused}");
        assert!(elsewhere.is_ok(), "{elsewhere:?}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn denied_family_and_skill_to_skill_are_refused() {
        let invoker = invoker_with(Arc::new(ToolRegistry::new()));
        let err = tokio::task::spawn_blocking(move || {
            invoker.invoke(
                &ctx(),
                &permission("vault.set", &[]),
                &serde_json::json!({}),
            )
        })
        .await
        .unwrap()
        .unwrap_err();
        assert!(err.contains("never be granted"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn url_args_outside_allowed_hosts_are_refused() {
        let invoker = invoker_with(Arc::new(ToolRegistry::new()));
        let args = serde_json::json!({"nested": {"link": "https://evil.example.com/x"}});
        let err = tokio::task::spawn_blocking(move || {
            invoker.invoke(&ctx(), &permission("echo.say", &["api.good.com"]), &args)
        })
        .await
        .unwrap()
        .unwrap_err();
        assert!(err.contains("evil.example.com"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn denied_family_fires_before_the_capability_gate() {
        let invoker = invoker_with(Arc::new(ToolRegistry::new()));
        let err = tokio::task::spawn_blocking(move || {
            invoker.invoke(
                &ctx(),
                &permission("agent.delete", &[]),
                &serde_json::json!({"id": "x"}),
            )
        })
        .await
        .unwrap();
        assert!(err.unwrap_err().contains("never be granted"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn capability_gated_tool_denies_for_unresolved_identity() {
        // knowledge.delete maps to KnowledgeManage (admin-only by default) and
        // is NOT in a denied family, so this exercises the capability branch.
        let invoker = invoker_with(Arc::new(ToolRegistry::new()));
        let err = tokio::task::spawn_blocking(move || {
            invoker.invoke(
                &ctx(),
                &permission("knowledge.delete", &[]),
                &serde_json::json!({}),
            )
        })
        .await
        .unwrap()
        .unwrap_err();
        assert!(err.contains("knowledge_manage"), "{err}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn declared_tool_executes_and_returns_the_envelope() {
        let registry = Arc::new(ToolRegistry::new());
        registry.register(Arc::new(Echo));
        let invoker = invoker_with(registry);
        let out = tokio::task::spawn_blocking(move || {
            invoker.invoke(
                &ctx(),
                &permission("echo.say", &[]),
                &serde_json::json!({"hello": "world"}),
            )
        })
        .await
        .unwrap()
        .unwrap();
        assert_eq!(out["success"], true);
        assert_eq!(out["output"]["hello"], "world");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn unregistered_tool_is_a_guest_visible_error_not_a_denial() {
        let invoker = invoker_with(Arc::new(ToolRegistry::new()));
        let out = tokio::task::spawn_blocking(move || {
            invoker.invoke(&ctx(), &permission("echo.say", &[]), &serde_json::json!({}))
        })
        .await
        .unwrap()
        .unwrap();
        assert_eq!(out["success"], false);
        assert!(out["error"].as_str().unwrap().contains("no tool named"));
    }

    #[test]
    fn retry_wait_takes_the_larger_of_hint_and_backoff_and_is_capped() {
        let base = std::time::Duration::from_secs(2);
        assert_eq!(retry_wait(base, 1, None), base);
        assert_eq!(retry_wait(base, 2, None), base * 2);
        // A hint above the ladder wins.
        assert_eq!(
            retry_wait(base, 1, Some(30)),
            std::time::Duration::from_secs(30)
        );
        // A hint below the ladder never shortens the backoff.
        assert_eq!(retry_wait(base, 2, Some(1)), base * 2);
        // An absurd hint is capped.
        assert_eq!(retry_wait(base, 1, Some(100_000)), MAX_RATE_LIMIT_WAIT);
        assert_eq!(retry_wait(base, 1, Some(u64::MAX)), MAX_RATE_LIMIT_WAIT);
    }

    /// Refuses with a typed rate-limit signal until the given attempt, then
    /// succeeds.
    #[derive(Debug)]
    struct RateLimited {
        calls: std::sync::atomic::AtomicU32,
        succeed_on: u32,
    }

    #[async_trait]
    impl Tool for RateLimited {
        fn name(&self) -> &str {
            "flaky.call"
        }
        fn description(&self) -> &str {
            "flaky"
        }
        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        async fn execute(&self, _input: Value) -> eyre::Result<ToolResult> {
            let call = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
            if call < self.succeed_on {
                Ok(ToolResult::rate_limited(
                    "HTTP 429 Too Many Requests",
                    Some(0),
                    1,
                ))
            } else {
                Ok(ToolResult::success(serde_json::json!({"call": call}), 1))
            }
        }
    }

    fn fast_backoff(mut invoker: HubSkillToolInvoker) -> HubSkillToolInvoker {
        invoker.rate_limit_backoff = std::time::Duration::from_millis(1);
        invoker
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn rate_limited_calls_are_retried_with_backoff() {
        let registry = Arc::new(ToolRegistry::new());
        registry.register(Arc::new(RateLimited {
            calls: Default::default(),
            succeed_on: 3,
        }));
        let invoker = fast_backoff(invoker_with(registry));
        let out = tokio::task::spawn_blocking(move || {
            invoker.invoke(
                &ctx(),
                &permission("flaky.call", &[]),
                &serde_json::json!({}),
            )
        })
        .await
        .unwrap()
        .unwrap();
        assert_eq!(out["success"], true);
        assert_eq!(out["output"]["call"], 3);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn rate_limited_retries_are_bounded() {
        let registry = Arc::new(ToolRegistry::new());
        registry.register(Arc::new(RateLimited {
            calls: Default::default(),
            succeed_on: 10,
        }));
        let invoker = fast_backoff(invoker_with(registry));
        let out = tokio::task::spawn_blocking(move || {
            invoker.invoke(
                &ctx(),
                &permission("flaky.call", &[]),
                &serde_json::json!({}),
            )
        })
        .await
        .unwrap()
        .unwrap();
        assert_eq!(out["success"], false);
        assert!(out["error"].as_str().unwrap().contains("429"));
        // The typed refusal survives into the guest envelope.
        assert_eq!(out["rate_limited"]["retry_after_secs"], 0);
    }

    /// Prose about rate limits is not the typed signal: a partial failure
    /// whose message merely mentions throttling (side effects may have
    /// committed) must never be re-executed.
    #[tokio::test(flavor = "multi_thread")]
    async fn prose_rate_limit_failures_are_never_retried() {
        #[derive(Debug, Default)]
        struct PartialFailure(std::sync::atomic::AtomicU32);
        #[async_trait]
        impl Tool for PartialFailure {
            fn name(&self) -> &str {
                "partial.call"
            }
            fn description(&self) -> &str {
                "partial"
            }
            fn input_schema(&self) -> serde_json::Value {
                serde_json::json!({"type": "object"})
            }
            async fn execute(&self, _input: Value) -> eyre::Result<ToolResult> {
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(ToolResult::failure(
                    "created 3 records, then upstream rate limited (HTTP 429)",
                    1,
                ))
            }
        }
        let counter = Arc::new(PartialFailure::default());
        let registry = Arc::new(ToolRegistry::new());
        registry.register(Arc::clone(&counter) as Arc<dyn Tool>);
        let invoker = fast_backoff(invoker_with(registry));
        let out = tokio::task::spawn_blocking(move || {
            invoker.invoke(
                &ctx(),
                &permission("partial.call", &[]),
                &serde_json::json!({}),
            )
        })
        .await
        .unwrap()
        .unwrap();
        assert_eq!(out["success"], false);
        assert!(out.get("rate_limited").is_none());
        assert_eq!(counter.0.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn non_rate_limit_failures_are_never_retried() {
        #[derive(Debug)]
        struct Boom(std::sync::atomic::AtomicU32);
        #[async_trait]
        impl Tool for Boom {
            fn name(&self) -> &str {
                "boom.call"
            }
            fn description(&self) -> &str {
                "boom"
            }
            fn input_schema(&self) -> serde_json::Value {
                serde_json::json!({"type": "object"})
            }
            async fn execute(&self, _input: Value) -> eyre::Result<ToolResult> {
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(ToolResult::failure("HTTP 500 internal error", 1))
            }
        }
        let counter = Arc::new(Boom(Default::default()));
        let registry = Arc::new(ToolRegistry::new());
        registry.register(Arc::clone(&counter) as Arc<dyn Tool>);
        let invoker = fast_backoff(invoker_with(registry));
        let out = tokio::task::spawn_blocking(move || {
            invoker.invoke(
                &ctx(),
                &permission("boom.call", &[]),
                &serde_json::json!({}),
            )
        })
        .await
        .unwrap()
        .unwrap();
        assert_eq!(out["success"], false);
        assert_eq!(counter.0.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn invoke_many_preserves_order_and_isolates_denials() {
        let registry = Arc::new(ToolRegistry::new());
        registry.register(Arc::new(Echo));
        let invoker = invoker_with(registry);
        let results = tokio::task::spawn_blocking(move || {
            invoker.invoke_many(
                &ctx(),
                vec![
                    (permission("echo.say", &[]), serde_json::json!({"i": 0})),
                    (permission("vault.set", &[]), serde_json::json!({})),
                    (permission("echo.say", &[]), serde_json::json!({"i": 2})),
                ],
            )
        })
        .await
        .unwrap();
        assert_eq!(results.len(), 3);
        assert_eq!(results[0].as_ref().unwrap()["output"]["i"], 0);
        assert!(results[1]
            .as_ref()
            .unwrap_err()
            .contains("never be granted"));
        assert_eq!(results[2].as_ref().unwrap()["output"]["i"], 2);
    }

    /// Records the high-water mark of concurrent executions across every
    /// clone, under whatever tool name it is registered as.
    #[derive(Debug, Default)]
    struct InFlight {
        name: String,
        current: Arc<std::sync::atomic::AtomicUsize>,
        max: Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait]
    impl Tool for InFlight {
        fn name(&self) -> &str {
            &self.name
        }
        fn description(&self) -> &str {
            "slow"
        }
        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        async fn execute(&self, input: Value) -> eyre::Result<ToolResult> {
            use std::sync::atomic::Ordering::SeqCst;
            let now = self.current.fetch_add(1, SeqCst) + 1;
            self.max.fetch_max(now, SeqCst);
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            self.current.fetch_sub(1, SeqCst);
            Ok(ToolResult::success(input, 1))
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn invoke_many_bounds_same_family_concurrency() {
        let tool = Arc::new(InFlight {
            name: "slow.call".into(),
            ..Default::default()
        });
        let registry = Arc::new(ToolRegistry::new());
        registry.register(Arc::clone(&tool) as Arc<dyn Tool>);
        let invoker = invoker_with(registry);
        let results = tokio::task::spawn_blocking(move || {
            invoker.invoke_many(
                &ctx(),
                (0..4)
                    .map(|i| (permission("slow.call", &[]), serde_json::json!({"i": i})))
                    .collect(),
            )
        })
        .await
        .unwrap();
        assert_eq!(results.len(), 4);
        for (i, result) in results.iter().enumerate() {
            assert_eq!(result.as_ref().unwrap()["output"]["i"], i);
        }
        // Exactly the bound: fewer would mean the batch serialized (the 50ms
        // sleeps guarantee wave overlap), more would mean the bound leaked.
        let peak = tool.max.load(std::sync::atomic::Ordering::SeqCst);
        assert_eq!(peak, MAX_BATCH_CONCURRENCY, "peak in-flight was {peak}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn invoke_many_runs_distinct_families_concurrently() {
        let current: Arc<std::sync::atomic::AtomicUsize> = Default::default();
        let max: Arc<std::sync::atomic::AtomicUsize> = Default::default();
        let registry = Arc::new(ToolRegistry::new());
        for name in ["alpha.query", "beta.query"] {
            registry.register(Arc::new(InFlight {
                name: name.into(),
                current: Arc::clone(&current),
                max: Arc::clone(&max),
            }) as Arc<dyn Tool>);
        }
        let invoker = invoker_with(registry);
        let results = tokio::task::spawn_blocking(move || {
            invoker.invoke_many(
                &ctx(),
                vec![
                    (permission("alpha.query", &[]), serde_json::json!({"i": 0})),
                    (permission("alpha.query", &[]), serde_json::json!({"i": 1})),
                    (permission("beta.query", &[]), serde_json::json!({"i": 2})),
                    (permission("beta.query", &[]), serde_json::json!({"i": 3})),
                ],
            )
        })
        .await
        .unwrap();
        for (i, result) in results.iter().enumerate() {
            assert_eq!(result.as_ref().unwrap()["output"]["i"], i);
        }
        // Both families' waves overlap: a throttled family must not
        // head-of-line block an unrelated one.
        let peak = max.load(std::sync::atomic::Ordering::SeqCst);
        assert_eq!(peak, 2 * MAX_BATCH_CONCURRENCY, "peak in-flight was {peak}");
    }
}
