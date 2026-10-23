//! `knowledge.refresh_index` — agent-callable trigger for the hub-wide
//! maintenance pass (pull every standalone + project repo, then index).

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use eyre::Result;
use serde::Deserialize;
use workforce_agent::tools::{Tool, ToolResult};

use super::global_index::{IndexMode, MaintenanceRunner};
use super::pass_state::PassRunOutcome;

/// Tool name registered by [`KnowledgeRefreshIndexTool`].
pub const TOOL_NAME: &str = "knowledge.refresh_index";

/// Default policy: `allow`. Pulling and indexing repos is routine
/// agent work; gating it on human approval breaks unattended
/// maintenance flows. Operators who want to throttle expensive
/// rebuilds can override with a higher-priority YAML rule (e.g. a
/// condition on `full=true` to gate full rebuilds only).
pub fn default_policy() -> workforce_policy::rules::PolicyRule {
    use workforce_policy::rules::{PolicyRule, PolicyScope, RuleAction};
    PolicyRule {
        name: format!("{TOOL_NAME}-allow-default"),
        scope: PolicyScope::Global,
        tool: TOOL_NAME.to_string(),
        action: RuleAction::Allow,
        conditions: Vec::new(),
        priority: 10,
        constitutional: true,
    }
}

#[derive(Deserialize)]
struct Input {
    /// `false` (default) → incremental; `true` → full rebuild.
    #[serde(default)]
    full: bool,
}

pub struct KnowledgeRefreshIndexTool {
    runner: Arc<MaintenanceRunner>,
}

impl KnowledgeRefreshIndexTool {
    pub fn new(runner: Arc<MaintenanceRunner>) -> Self {
        Self { runner }
    }
}

#[async_trait]
impl Tool for KnowledgeRefreshIndexTool {
    fn name(&self) -> &str {
        TOOL_NAME
    }
    fn description(&self) -> &str {
        "Start a hub-wide indexing pass in the background and return immediately. Returns `outcome: \"started\"` with the initial queue snapshot — the pass is NOT finished on return: poll knowledge.status to track progress and confirm completion/failures, and do not report success until status shows the pass has finished. Returns `outcome: \"already_in_flight\"` with a live queue snapshot when a pass is already running (this call does not start a second one). For project-scoped indexing prefer projects.index."
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "full": {
                    "type": "boolean",
                    "description": "Force a full rebuild. Default: incremental."
                }
            }
        })
    }
    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();
        let params: Input = match serde_json::from_value(input) {
            Ok(p) => p,
            Err(e) => {
                return Ok(ToolResult::failure(
                    e.to_string(),
                    start.elapsed().as_millis() as u64,
                ))
            }
        };
        let mode = if params.full {
            IndexMode::Full
        } else {
            IndexMode::Incremental
        };
        let outcome = self.runner.start_pass(mode).await?;
        Ok(ToolResult::success(
            outcome_to_json(&outcome),
            start.elapsed().as_millis() as u64,
        ))
    }
}

/// Project the `PassRunOutcome` into the documented response shape.
/// The `Completed` branch returns flattened pass-report fields plus
/// a convenience `failures` array (subset of `targets` whose outcome
/// isn't `Indexed`) so the agent gets the "indexed N / failed M"
/// answer in one read.
fn outcome_to_json(outcome: &PassRunOutcome) -> serde_json::Value {
    match outcome {
        PassRunOutcome::Completed(report) => {
            let failures: Vec<_> = report
                .targets
                .iter()
                .filter(|t| {
                    !matches!(
                        t.outcome,
                        super::global_index::TargetOutcome::Indexed { .. }
                    )
                })
                .cloned()
                .collect();
            serde_json::json!({
                "outcome": "completed",
                "mode": report.mode,
                "target_count": report.target_count,
                "duration_ms": report.duration_ms,
                "targets": report.targets,
                "failures": failures,
            })
        }
        PassRunOutcome::Started { message, snapshot } => serde_json::json!({
            "outcome": "started",
            "message": message,
            "mode": snapshot.mode,
            "target_count": snapshot.targets.len(),
            "targets": snapshot.targets,
        }),
        PassRunOutcome::AlreadyInFlight { message, in_flight } => serde_json::json!({
            "outcome": "already_in_flight",
            "message": message,
            "in_flight": in_flight,
        }),
    }
}
