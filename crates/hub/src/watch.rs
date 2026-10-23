//! `watch` — keep an eye on something and report back when it changes (or when
//! a condition is met). A thin, agent-facing wrapper over a `Change`/`Condition`
//! monitor job: it polls a read tool on an interval and messages the originating
//! channel when the watched value changes (or `until` becomes true).

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use eyre::Result;
use serde::Deserialize;
use serde_json::{json, Value};
use workforce_agent::tools::{Tool, ToolResult};

use crate::job_runner::{
    JobConfig, JobManifest, JobType, MonitorJobConfig, NotifyOn, MIN_POLL_INTERVAL_SECS,
};
use crate::serve::AgentService;

const DEFAULT_INTERVAL_SECS: u64 = 60;
const DEFAULT_MAX_POLLS: u64 = 240;

/// The **allowlist** of tools `watch` may poll. A watch poll runs in the
/// background with NO user identity in scope and does NOT pass through the
/// capability gate (it is a raw `execute_tool`, not a runner turn), so a watched
/// tool runs *unscoped and ungated* on a timer. A deny-list cannot be made safe
/// over the unbounded `mcp__*` namespace (every MCP tool — including writes —
/// returns `None` from the capability registry) nor over ungated state-changing
/// built-ins (`monitor`, `messaging.send`, `node_invoke`, `scheduler`), so this
/// is a closed, default-deny set of genuinely **read-only, side-effect-free**
/// built-ins. New tools and ALL MCP tools are non-watchable until added here on
/// purpose. Credential/log/config-disclosure reads (`vault.*`, `secret.list`,
/// `system.logs`, `settings`) are deliberately excluded — `watch` is
/// for status polling, and those would run unscoped against shared state.
const WATCH_ALLOWED_TOOLS: &[&str] = &[
    // AWS read-only describe/get/list (no AWS mutation tool exists).
    "aws.cloudwatch.get_metric_data",
    "aws.deploy.status",
    "aws.ecr.describe_images",
    "aws.ecs.describe_container_instances",
    "aws.ecs.describe_services",
    "aws.ecs.describe_task_definition",
    "aws.ecs.describe_tasks",
    "aws.ecs.list_tasks",
    "aws.elbv2.describe_target_health",
    "aws.rds.describe_db_clusters",
    "aws.rds.describe_events",
    "aws.s3.get",
    "aws.s3.head",
    "aws.s3.list",
    "aws.sts.get_caller_identity",
    // Code / repo / filesystem reads (confined to allowed_paths).
    "code.read",
    "code.search",
    "code.tree",
    "code.symbols",
    "code.diff",
    "code.find",
    "repo.read",
    "repo.grep",
    "repo.glob",
    "repo.list",
    "filesystem.read",
    "filesystem.list",
    "filesystem.grep",
    "filesystem.glob",
    // Knowledge-base reads.
    "knowledge.ask",
    "knowledge.explain",
    "knowledge.graph",
    "knowledge.impact",
    "knowledge.overview",
    "knowledge.symbol",
    // Status / list / metrics reads.
    "system.llm_status",
    "system.routing_status",
    "system.skill_status",
    "system.version",
    "system.list_models",
    "system.list_skills",
    "system.list_tools",
    "stats.cache_status",
    "stats.my_usage",
    "stats.routing_history",
    "stats.task_history",
    "stats.tool_costs",
    "costs.analyze",
    "costs.forecast",
    "pricing.list",
    // Web reads.
    "web.fetch",
    "web.search",
];

/// Whether `watch` may poll `tool`: it must be on the read-only allowlist AND
/// (defence in depth, in case the list ever gains a mistake) carry no capability
/// mapping and not be destructive-by-name.
fn is_watchable(tool: &str, params: &Value) -> bool {
    WATCH_ALLOWED_TOOLS.contains(&tool)
        && crate::authz::tool_capability(tool, params).is_none()
        && !workforce_policy::rules::is_destructive(tool)
}

/// "Keep an eye on X" tool.
pub struct WatchTool {
    service: Arc<AgentService>,
}

impl WatchTool {
    pub fn new(service: Arc<AgentService>) -> Self {
        Self { service }
    }
}

#[derive(Deserialize)]
struct WatchInput {
    tool: String,
    #[serde(default)]
    params: Option<Value>,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    until: Option<String>,
    #[serde(default)]
    interval_secs: Option<u64>,
    #[serde(default)]
    max_polls: Option<u64>,
    #[serde(default)]
    label: Option<String>,
}

#[async_trait]
impl Tool for WatchTool {
    fn name(&self) -> &str {
        "watch"
    }

    fn description(&self) -> &str {
        "Keep an eye on something and report back when it changes — or when a condition is met. \
         Polls a read-only tool on an interval and messages you the moment the watched value \
         changes (give `path` to watch one field so volatile fields don't trigger false alarms), \
         or when `until` is true. Use this whenever you would otherwise say \"I'll keep an eye on \
         it\" — never promise to watch something without calling this."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "tool": { "type": "string", "description": "Read-only tool to poll (e.g. a status check)." },
                "params": { "type": "object", "description": "Parameters passed to the tool on each poll." },
                "path": { "type": "string", "description": "Dot-path into the tool result to watch for changes (e.g. 'status'). Watches the whole result if omitted." },
                "until": { "type": "string", "description": "Condition expression (e.g. 'status == \"done\"'). If given, notifies once when true instead of on every change." },
                "interval_secs": { "type": "integer", "description": "Seconds between polls (default 60, min 10)." },
                "max_polls": { "type": "integer", "description": "How many times to poll before stopping (default 240)." },
                "label": { "type": "string", "description": "Human-readable label." }
            },
            "required": ["tool"]
        })
    }

    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = Instant::now();
        let elapsed = || start.elapsed().as_millis() as u64;
        let input: WatchInput = match serde_json::from_value(input) {
            Ok(i) => i,
            Err(e) => {
                return Ok(ToolResult::failure(
                    format!("Invalid watch input: {e}"),
                    elapsed(),
                ))
            }
        };
        // A watch poll runs in the background with NO user identity in scope and
        // does NOT pass through the capability gate (it is a raw `execute_tool`,
        // not a runner turn) — so a watched tool runs *unscoped and ungated* on a
        // timer. Restrict the watched tool to the closed, read-only allowlist
        // (`WATCH_ALLOWED_TOOLS`): a privileged tool (`exec.run`), an ungated
        // state-changer (`monitor.save` — which would re-open the channel-exfil
        // gap via the userless poll, `messaging.send`, `node_invoke`), or ANY MCP
        // write would otherwise be a standing bypass of every capability gate.
        // Default-deny: anything not explicitly allowed is refused, for every
        // role (an admin's watch is just as unscoped). Recurring *actions* belong
        // in `scheduler`, which runs as a real owner through the gate.
        let watch_params = input.params.clone().unwrap_or_else(|| json!({}));
        if !is_watchable(&input.tool, &watch_params) {
            return Ok(ToolResult::failure(
                format!(
                    "`watch` only polls a vetted set of read-only tools — `{}` is not \
                     one of them (it may change state, access credentials, or be an \
                     external/MCP tool that cannot run unattended). Poll a status/read \
                     tool, or use `scheduler` for a recurring action.",
                    input.tool
                ),
                elapsed(),
            ));
        }

        let Some(runner) = self.service.job_runner.as_ref() else {
            return Ok(ToolResult::failure(
                "Background execution is not available",
                elapsed(),
            ));
        };

        // A watch reports asynchronously, so it needs a channel to report to.
        let (channel, thread) = self.service.current_messaging_context().await;
        if channel.is_none() {
            return Ok(ToolResult::failure(
                "Nothing to report changes to here — `watch` needs a messaging context (e.g. a chat channel)",
                elapsed(),
            ));
        }

        let notify_on = if input.until.is_some() {
            NotifyOn::Condition
        } else {
            NotifyOn::Change
        };
        let interval_secs = input
            .interval_secs
            .unwrap_or(DEFAULT_INTERVAL_SECS)
            .max(MIN_POLL_INTERVAL_SECS);
        let max_polls = input.max_polls.unwrap_or(DEFAULT_MAX_POLLS).max(1);
        // Keep the job alive for the whole poll budget so it ends on max-polls, not timeout.
        let timeout_secs = interval_secs.saturating_mul(max_polls + 1);

        let mut manifest = JobManifest::new(
            self.service.agent_id.as_str().to_string(),
            JobType::Monitor,
            JobConfig::Monitor(MonitorJobConfig {
                tool: input.tool.clone(),
                params: input.params.unwrap_or_else(|| json!({})),
                interval_secs,
                until: input.until.clone().unwrap_or_default(),
                max_polls,
                polls_completed: 0,
                notify_on,
                watch_path: input.path,
            }),
            input.label,
            timeout_secs,
        );
        manifest.notify_channel = channel;
        manifest.notify_thread = thread;

        match runner.submit(manifest).await {
            Ok(job_id) => {
                let trigger = match notify_on {
                    NotifyOn::Condition => "the condition is met".to_string(),
                    NotifyOn::Change => "it changes".to_string(),
                };
                Ok(ToolResult::success(
                    json!({
                        "job_id": job_id,
                        "status": "watching",
                        "tool": input.tool,
                        "message": format!("Watching `{}` — I'll report back when {trigger}.", input.tool),
                    }),
                    elapsed(),
                ))
            }
            Err(e) => Ok(ToolResult::failure(
                format!("Failed to start watch: {e}"),
                elapsed(),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowlisted_read_tools_are_watchable() {
        let p = json!({});
        for tool in [
            "aws.ecs.describe_services",
            "system.llm_status",
            "code.read",
            "knowledge.overview",
            "web.fetch",
            "stats.tool_costs",
            "pricing.list",
        ] {
            assert!(is_watchable(tool, &p), "{tool} should be watchable");
        }
    }

    #[test]
    fn privileged_and_state_changing_tools_are_not_watchable() {
        let p = json!({});
        // The punch-through (exec/code-exec), the laundering vectors (monitor /
        // messaging.send / node_invoke / scheduler), other mutators, credential
        // and log reads, and ANY MCP tool must all be refused (default-deny).
        for tool in [
            "exec.run",
            "code.cargo",
            "coding.claude",
            "monitor", // watch→monitor.save laundering (B1)
            "messaging.send",
            "node_invoke",
            "scheduler",
            "reminder",
            "watch",     // no recursive watch
            "vault.get", // credential read — excluded on purpose
            "secret.list",
            "system.logs",
            "settings",
            "agent.delete",
            "mcp__metabase__metabase_card_query", // any MCP tool
            "mcp__jira__jira_create_issue",
            "made.up.tool", // unknown ⇒ default-deny
        ] {
            assert!(
                !is_watchable(tool, &p),
                "{tool} must NOT be watchable (default-deny)"
            );
        }
    }

    #[test]
    fn allowlist_entries_never_carry_a_capability_or_are_destructive() {
        // Guard against a future mistaken addition: every allowlisted tool must
        // be ungated (no capability) and non-destructive, or `is_watchable`
        // rejects it anyway (defence in depth).
        let p = json!({});
        for tool in WATCH_ALLOWED_TOOLS {
            assert!(
                crate::authz::tool_capability(tool, &p).is_none(),
                "{tool} on the watch allowlist must not map to a capability"
            );
            assert!(
                !workforce_policy::rules::is_destructive(tool),
                "{tool} on the watch allowlist must not be destructive"
            );
        }
    }
}
