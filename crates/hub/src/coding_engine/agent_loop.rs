//! The hub's own agent loop as a coding engine: any model or tier the install
//! can route, with Claude Code's six base tools over the round's sandbox and
//! the hub tools the round's MCP surface would have served, called
//! in-process through the same gate. It runs below Claude Code's harness (no subagents, no
//! `CLAUDE.md` conventions), which is why a chain puts it last; it is still a
//! real coder in the same sandbox on the same tokens, and the round's
//! contract reads the same to it.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use eyre::{eyre, Result};
use tokio_util::sync::CancellationToken;
use workforce_agent::llm::types::TokenUsage;
use workforce_agent::runner::ProgressEvent;
use workforce_agent::tools::Tool;
use workforce_common::settings::DEFAULT_DRIVER_MAX_TURNS;
use workforce_common::subscription::Served;

use super::sandbox_tools::sandbox_tools;
use super::{CliRoundResult, CodingEngine, CodingRound, ModelChoices, Refusal, RoundContext};
use crate::advisor_loop::{AdvisorLoopOutcome, AdvisorLoopRequest, LoopBudget};
use crate::agent_turn::{Oversight, ProviderLoop, ToolLoop, TurnChain};
use crate::claude_code::Completion;
use crate::coding_sandbox::CodingSandbox;
use crate::mcp_turn::{exposed_tools, ClientTimeout};

pub const LABEL: &str = "model";
/// Held back from the round's wall clock for the reply that ends it.
const WRITE_UP_RESERVE: Duration = Duration::from_secs(90);
const STDERR_TAIL_CHARS: usize = 2000;

#[derive(Debug, Default, Clone, Copy)]
pub struct AgentLoopEngine;

/// Tokens across every call of a round.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct Usage {
    input: u64,
    output: u64,
    cache_creation: u64,
    cache_read: u64,
}

impl Usage {
    fn add(&mut self, usage: &TokenUsage) {
        self.input += u64::from(usage.input_tokens);
        self.output += u64::from(usage.output_tokens);
        self.cache_creation += u64::from(usage.cache_creation_input_tokens);
        self.cache_read += u64::from(usage.cache_read_input_tokens);
    }
}

/// The reply gets the reserve, or a quarter of a short round.
fn write_up_reserve(timeout: Duration) -> Duration {
    WRITE_UP_RESERVE.min(timeout / 4)
}

fn loop_budget(timeout: Duration, now: Instant) -> LoopBudget {
    let reserve = write_up_reserve(timeout);
    LoopBudget {
        research_deadline: now + timeout.saturating_sub(reserve),
        write_up_reserve: reserve,
    }
}

/// How the loop is introduced to the workspace, ahead of the round's own
/// contract.
fn system_prompt(workdir: &Path, hub_tools: &[String], contract: Option<&str>) -> String {
    let hub_tools = match hub_tools {
        [] => String::new(),
        names => format!(
            " You also have the hub's own tools ({}) for context outside the workspace.",
            names.join(", ")
        ),
    };
    let contract = contract.map(|c| format!("\n\n{c}")).unwrap_or_default();
    format!(
        "You are a coding agent working in the workspace at {}. Read, Write, Edit, Glob, Grep and \
         Bash act inside it, and every path is relative to it.{hub_tools} Work in small verified \
         steps: read before you edit, run the narrowest check that proves a change, and report \
         only what you observed.{contract}",
        workdir.display()
    )
}

/// The hub tools the round may call, gated exactly once: on a CLI route
/// (`routes`) the tools themselves, which that route serves and gates as
/// the round's principal; else bound as every engine binds them, each call
/// through the round's gate as its principal, in its session, wherever the
/// in-process loop runs it.
fn hub_tools(
    ctx: &RoundContext<'_>,
    sandbox: &dyn CodingSandbox,
    cancel: Option<&CancellationToken>,
    routes: Option<&TurnChain>,
) -> Vec<Arc<dyn Tool>> {
    match routes {
        Some(_) => {
            let patterns = sandbox.hub_tool_patterns();
            match patterns.is_empty() {
                true => Vec::new(),
                false => exposed_tools(ctx.service, ctx.principal(), patterns.into()),
            }
        }
        None => ctx
            .round_tools(sandbox, cancel, ClientTimeout::IN_PROCESS)
            .map(|tools| tools.in_process())
            .unwrap_or_default(),
    }
}

fn stderr_tail(text: &str) -> String {
    let count = text.chars().count();
    text.chars()
        .skip(count.saturating_sub(STDERR_TAIL_CHARS))
        .collect()
}

/// The round's result from how the loop ended: the reply it converged on, the
/// reply the cap or the clock forced, or a provider that could not serve it.
fn result_of(
    outcome: AdvisorLoopOutcome,
    usage: Usage,
    fallback_model: Option<&str>,
    duration: Duration,
) -> Result<CliRoundResult> {
    let tokens = |result: CliRoundResult| CliRoundResult {
        input_tokens: usage.input,
        output_tokens: usage.output,
        cache_creation_tokens: usage.cache_creation,
        cache_read_tokens: usage.cache_read,
        duration,
        engine: LABEL.to_string(),
        ..result
    };
    match outcome {
        AdvisorLoopOutcome::Success {
            text,
            model,
            truncated,
            capped,
            rounds_used,
            ..
        } => Ok(tokens(CliRoundResult {
            text,
            model: Some(model),
            num_turns: rounds_used,
            completion: match (truncated, capped) {
                (true, _) => Completion::Timeout,
                (false, true) => Completion::TurnCap,
                (false, false) => Completion::Success,
            },
            ..Default::default()
        })),
        AdvisorLoopOutcome::Cancelled => Err(eyre!("coding round cancelled")),
        AdvisorLoopOutcome::ProviderError(detail) => Ok(tokens(CliRoundResult {
            text: detail.clone(),
            model: fallback_model.map(str::to_string),
            exit_code: 1,
            completion: Completion::Failed,
            stderr_tail: stderr_tail(&detail),
            ..Default::default()
        })),
    }
}

#[async_trait]
impl CodingEngine for AgentLoopEngine {
    fn label(&self) -> &'static str {
        LABEL
    }

    /// Any model or tier the install can route, at Claude's effort names.
    fn model_choices(&self) -> ModelChoices {
        ModelChoices {
            models: Vec::new(),
            efforts: workforce_common::settings::CLAUDE_CODE_EFFORTS,
            raw_id_prefixes: &[],
        }
    }

    fn metered(&self) -> bool {
        true
    }

    fn preflight(&self, _: &RoundContext<'_>) -> Result<(), Refusal> {
        Ok(())
    }

    async fn run(
        &self,
        sandbox: Arc<dyn CodingSandbox>,
        round: &CodingRound,
        ctx: &RoundContext<'_>,
        cancel: Option<CancellationToken>,
        _progress: Option<tokio::sync::mpsc::Sender<ProgressEvent>>,
    ) -> Result<CliRoundResult> {
        let started_at = chrono::Utc::now();
        let started = Instant::now();
        let routes =
            crate::agent_turn::tool_loop::loop_routes(ctx.service, round.model.as_deref()).await;
        let hub = hub_tools(ctx, sandbox.as_ref(), cancel.as_ref(), routes.as_ref());
        let hub_tool_names: Vec<String> = hub.iter().map(|t| t.name().to_string()).collect();
        let tools: Vec<Arc<dyn Tool>> = sandbox_tools(Arc::clone(&sandbox))
            .into_iter()
            .chain(hub)
            .collect();
        let request = AdvisorLoopRequest {
            system_prompt: system_prompt(
                sandbox.workdir(),
                &hub_tool_names,
                round.system_prompt.as_deref(),
            ),
            user_message: round.task.clone(),
            tools,
            model: round.model.clone(),
            max_rounds: round.max_turns.unwrap_or(DEFAULT_DRIVER_MAX_TURNS),
            budget: Some(loop_budget(round.timeout, started)),
            cancel,
            label: "coding",
        };
        let usage = Mutex::new(Usage::default());
        let served = Mutex::new(None);
        let on_usage = |reported: &TokenUsage, _: &str, paid_by: Option<&Served>| {
            usage.lock().expect("usage lock").add(reported);
            *served.lock().expect("served lock") = paid_by.cloned();
        };
        let outcome = workforce_agent::runner::CURRENT_USER_ID
            .scope(ctx.principal().to_string(), async {
                match routes {
                    Some(chain) => {
                        chain
                            .run_loop(
                                ctx.service,
                                request,
                                &on_usage,
                                Oversight::Attended(ctx.resume),
                            )
                            .await
                    }
                    None => {
                        ProviderLoop(Arc::clone(&ctx.service.llm))
                            .run(request, &on_usage)
                            .await
                    }
                }
            })
            .await;
        let usage = *usage.lock().expect("usage lock");
        let result = result_of(outcome, usage, round.model.as_deref(), started.elapsed())?;
        let ledger_model = result
            .model
            .clone()
            .unwrap_or_else(|| "default".to_string());
        let cost_usd = ctx.service.cost_tracker.calculate_cost(
            &ledger_model,
            result.input_tokens,
            result.output_tokens,
            result.cache_creation_tokens,
            result.cache_read_tokens,
        );
        let result = CliRoundResult { cost_usd, ..result };
        ctx.record(
            LABEL,
            &ledger_model,
            &result,
            started_at,
            served.into_inner().expect("served lock"),
        );
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage() -> Usage {
        Usage {
            input: 100,
            output: 20,
            cache_creation: 5,
            cache_read: 50,
        }
    }

    #[test]
    fn the_reply_reserve_shrinks_with_a_short_round() {
        assert_eq!(
            write_up_reserve(Duration::from_secs(1200)),
            WRITE_UP_RESERVE
        );
        assert_eq!(
            write_up_reserve(Duration::from_secs(120)),
            Duration::from_secs(30)
        );
        let now = Instant::now();
        let budget = loop_budget(Duration::from_secs(1200), now);
        assert_eq!(budget.write_up_reserve, WRITE_UP_RESERVE);
        assert_eq!(budget.research_deadline, now + Duration::from_secs(1110));
    }

    #[test]
    fn the_prompt_names_the_workspace_the_hub_tools_and_carries_the_contract() {
        let bare = system_prompt(Path::new("/work"), &[], None);
        assert!(bare.starts_with("You are a coding agent working in the workspace at /work."));
        assert!(!bare.contains("hub's own tools"));
        let full = system_prompt(
            Path::new("/work"),
            &["jira.get_issue".into()],
            Some("STATUS: DONE"),
        );
        assert!(full.contains("hub's own tools (jira.get_issue)"));
        assert!(full.ends_with("\n\nSTATUS: DONE"));
    }

    #[test]
    fn outcomes_map_onto_completions_with_the_rounds_tokens() {
        let success = |truncated, capped| AdvisorLoopOutcome::Success {
            text: "STATUS: DONE".into(),
            model: "claude-sonnet-5".into(),
            duration_ms: 1,
            truncated,
            capped,
            rounds_used: 7,
        };
        let done = result_of(success(false, false), usage(), None, Duration::from_secs(9)).unwrap();
        assert_eq!(done.completion, Completion::Success);
        assert_eq!(done.exit_code, 0);
        assert_eq!(done.engine, LABEL);
        assert_eq!(done.model.as_deref(), Some("claude-sonnet-5"));
        assert_eq!(done.num_turns, 7);
        assert_eq!(
            (
                done.input_tokens,
                done.output_tokens,
                done.cache_creation_tokens,
                done.cache_read_tokens
            ),
            (100, 20, 5, 50)
        );
        assert_eq!(done.duration, Duration::from_secs(9));
        assert_eq!(
            result_of(success(false, true), usage(), None, Duration::ZERO)
                .unwrap()
                .completion,
            Completion::TurnCap
        );
        assert_eq!(
            result_of(success(true, true), usage(), None, Duration::ZERO)
                .unwrap()
                .completion,
            Completion::Timeout,
            "the clock outranks the cap"
        );

        let failed = result_of(
            AdvisorLoopOutcome::ProviderError("API Error (429) rate limit".into()),
            usage(),
            Some("openrouter:anthropic:cheap"),
            Duration::ZERO,
        )
        .unwrap();
        assert_eq!(failed.completion, Completion::Failed);
        assert_eq!(failed.exit_code, 1);
        assert!(failed.text.contains("429"));
        assert_eq!(failed.model.as_deref(), Some("openrouter:anthropic:cheap"));

        let cancelled = result_of(AdvisorLoopOutcome::Cancelled, usage(), None, Duration::ZERO);
        assert!(cancelled.is_err_and(|e| e.to_string().contains("cancelled")));
    }

    struct DenyShell;

    impl workforce_agent::runner::CapabilityGate for DenyShell {
        fn deny_reason(
            &self,
            user: &str,
            _: &str,
            tool: &str,
            _: &serde_json::Value,
        ) -> Option<String> {
            (user == "alice" && tool == "probe.shell").then(|| "needs ShellExec".to_string())
        }
    }

    fn context<'a>(service: &'a crate::serve::AgentService) -> RoundContext<'a> {
        RoundContext {
            service,
            identity: Some("alice"),
            stand_in: workforce_common::subscription::SCHEDULER,
            ticket_id: None,
            description: "coding.session",
            resume: crate::mcp_gate::RouteResume::Nowhere,
        }
    }

    fn sandbox(patterns: &[&str]) -> Arc<dyn CodingSandbox> {
        crate::coding_sandbox::adopt_workdir(
            std::env::temp_dir(),
            patterns.iter().map(|p| p.to_string()).collect(),
            None,
        )
    }

    /// The loop runs each call in a task of its own, which inherits no scope:
    /// a call still runs gated, as the round's principal, in its session.
    #[tokio::test]
    async fn model_engine_hub_tools_are_gated_and_scoped() {
        use crate::mcp_turn::testing::{service, Probe};
        let service = service().await;
        service.tools.register(Probe::named("probe.whoami"));
        let shell = Probe::named("probe.shell");
        service.tools.register(shell.clone());
        service.set_capability_gate(Arc::new(DenyShell));
        let session = workforce_agent::runner::SessionContext {
            key: Some("slack:T1:C1:100.1".into()),
            billing_identity: Some("system:billing".into()),
            ..Default::default()
        };
        let tools = session
            .scope(async {
                hub_tools(
                    &context(&service),
                    sandbox(&["probe.*"]).as_ref(),
                    None,
                    None,
                )
            })
            .await;
        let run = |name: &'static str| {
            let tool = tools
                .iter()
                .find(|t| t.name() == name)
                .cloned()
                .expect("offered");
            async move {
                tokio::spawn(async move { tool.execute(serde_json::json!({})).await })
                    .await
                    .unwrap()
                    .unwrap()
            }
        };

        let seen = run("probe.whoami").await;
        assert!(seen.success, "{:?}", seen.error);
        let seen: serde_json::Value =
            serde_json::from_str(seen.output.as_str().expect("text")).unwrap();
        assert_eq!(seen["identity"], "alice");
        assert_eq!(seen["session_key"], "slack:T1:C1:100.1");
        assert_eq!(seen["billing"], "system:billing");
        assert!(seen["call"].is_string(), "under a call of its own");

        let denied = run("probe.shell").await;
        assert!(!denied.success);
        assert_eq!(
            denied.error.as_deref(),
            Some("Policy denied: needs ShellExec")
        );
        assert_eq!(shell.runs(), 0, "a denied call never runs");
    }

    #[tokio::test]
    async fn model_engine_never_offers_nested_round_tools() {
        use crate::mcp_turn::testing::{service, Probe};
        let service = service().await;
        for name in [
            "coding.session",
            "coding.fanout",
            "context.status",
            "jira.get_issue",
        ] {
            service.tools.register(Probe::named(name));
        }
        let offered: Vec<String> =
            hub_tools(&context(&service), sandbox(&["*"]).as_ref(), None, None)
                .iter()
                .map(|t| t.name().to_string())
                .collect();
        assert!(offered.contains(&"jira.get_issue".to_string()));
        for name in ["coding.session", "coding.fanout", "context.status"] {
            assert!(!offered.contains(&name.to_string()), "{name} is offered");
        }
    }
}
