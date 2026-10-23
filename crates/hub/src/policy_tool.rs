//! Policy proposal tools — let agents propose policy changes for human review.
//!
//! Tools:
//! - `policy.propose` — submit a policy rule proposal
//! - `policy.proposals` — list existing proposals and their status

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use serde::Deserialize;

use workforce_agent::tools::{Tool, ToolResult};
use workforce_common::telemetry::TelemetryStore;

/// Shared context for policy tools.
#[derive(Clone)]
pub struct PolicyToolContext {
    pub store: Arc<dyn TelemetryStore>,
    pub agent_id: String,
}

/// Create all policy tools.
pub fn policy_tools(ctx: PolicyToolContext) -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(PolicyProposeTool::new(ctx.clone())),
        Arc::new(PolicyListProposalsTool::new(ctx)),
    ]
}

// ─── policy.propose ─────────────────────────────────────────────────────────

pub struct PolicyProposeTool {
    ctx: PolicyToolContext,
}

impl PolicyProposeTool {
    pub fn new(ctx: PolicyToolContext) -> Self {
        Self { ctx }
    }
}

#[derive(Deserialize)]
struct ProposeInput {
    /// The tool name pattern (e.g., "git.*", "filesystem.read").
    tool: String,
    /// Scope: "global" or an object like {"team": "backend"} or {"agent": "dev-1"}.
    #[serde(default = "default_scope")]
    scope: serde_json::Value,
    /// Action: "allow", "deny", or "require_approval".
    action: String,
    /// Human-readable justification for the proposal.
    justification: String,
    /// Optional rule name (auto-generated if omitted).
    name: Option<String>,
    /// Optional priority (default 0).
    #[serde(default)]
    priority: i32,
}

fn default_scope() -> serde_json::Value {
    serde_json::Value::String("global".to_string())
}

#[async_trait]
impl Tool for PolicyProposeTool {
    fn name(&self) -> &str {
        "policy.propose"
    }

    fn description(&self) -> &str {
        "Propose a new policy rule for human review"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "tool": {
                    "type": "string",
                    "description": "Tool name pattern to match"
                },
                "scope": {
                    "description": "Scope",
                },
                "action": {
                    "type": "string",
                    "description": "Action when rule matches",
                    "enum": ["allow", "deny", "require_approval"]
                },
                "justification": {
                    "type": "string",
                    "description": "Why this rule should be added"
                },
                "name": {
                    "type": "string",
                    "description": "Optional rule name. Auto-generated if omitted."
                },
                "priority": {
                    "type": "integer",
                    "description": "Rule priority (higher = evaluated first). Default: 0"
                }
            },
            "required": ["tool", "action", "justification"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> eyre::Result<ToolResult> {
        let start = Instant::now();
        let params: ProposeInput = match serde_json::from_value(input) {
            Ok(p) => p,
            Err(e) => {
                return Ok(ToolResult {
                    output: serde_json::json!({"error": format!("Invalid input: {e}")}),
                    success: false,
                    error: Some(format!("Invalid input: {e}")),
                    rate_limited: None,
                    duration_ms: start.elapsed().as_millis() as u64,
                    loads: Vec::new(),
                });
            }
        };

        // Validate action
        if !["allow", "deny", "require_approval"].contains(&params.action.as_str()) {
            return Ok(ToolResult {
                output: serde_json::json!({"error": "action must be 'allow', 'deny', or 'require_approval'"}),
                success: false,
                error: Some("Invalid action".to_string()),
                rate_limited: None,
                duration_ms: start.elapsed().as_millis() as u64,
                loads: Vec::new(),
            });
        }

        // Validate tool pattern is non-empty
        if params.tool.is_empty() {
            return Ok(ToolResult {
                output: serde_json::json!({"error": "tool pattern must not be empty"}),
                success: false,
                error: Some("Empty tool pattern".to_string()),
                rate_limited: None,
                duration_ms: start.elapsed().as_millis() as u64,
                loads: Vec::new(),
            });
        }

        // Build the rule name
        let rule_name = params.name.unwrap_or_else(|| {
            format!(
                "proposed-{}-{}-{}",
                params.action,
                params.tool.replace('.', "-").replace('*', "all"),
                chrono::Utc::now().format("%Y%m%d%H%M%S")
            )
        });

        // Build rule JSON (compatible with PolicyRule serde format)
        let rule_json = serde_json::json!({
            "name": rule_name,
            "scope": params.scope,
            "tool": params.tool,
            "action": params.action,
            "conditions": [],
            "priority": params.priority,
        });

        let rule_str = serde_json::to_string(&rule_json)?;

        let proposal_id = self.ctx.store.create_policy_proposal(
            &self.ctx.agent_id,
            &rule_str,
            &params.justification,
        )?;

        Ok(ToolResult {
            output: serde_json::json!({
                "proposal_id": proposal_id,
                "status": "pending",
                "rule_name": rule_name,
                "message": "Policy proposal submitted for human review."
            }),
            success: true,
            error: None,
            rate_limited: None,
            duration_ms: start.elapsed().as_millis() as u64,
            loads: Vec::new(),
        })
    }
}

// ─── policy.proposals ───────────────────────────────────────────────────────

pub struct PolicyListProposalsTool {
    ctx: PolicyToolContext,
}

impl PolicyListProposalsTool {
    pub fn new(ctx: PolicyToolContext) -> Self {
        Self { ctx }
    }
}

#[derive(Deserialize)]
struct ListProposalsInput {
    /// Filter by status: "pending", "approved", "rejected", or omit for all.
    status: Option<String>,
    /// Max number of proposals to return (default 20).
    #[serde(default = "default_proposal_limit")]
    limit: usize,
}

fn default_proposal_limit() -> usize {
    20
}

#[async_trait]
impl Tool for PolicyListProposalsTool {
    fn name(&self) -> &str {
        "policy.proposals"
    }

    fn description(&self) -> &str {
        "List policy proposals and their review status"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "status": {
                    "type": "string",
                    "description": "Filter by status: 'pending', 'approved', 'rejected'",
                    "enum": ["pending", "approved", "rejected"]
                },
                "limit": {
                    "type": "integer",
                    "description": "Max proposals to return. Default: 20"
                }
            }
        ,
            "required": []})
    }

    async fn execute(&self, input: serde_json::Value) -> eyre::Result<ToolResult> {
        let start = Instant::now();
        let params: ListProposalsInput =
            serde_json::from_value(input).unwrap_or(ListProposalsInput {
                status: None,
                limit: default_proposal_limit(),
            });

        let proposals = self
            .ctx
            .store
            .list_policy_proposals(params.status.as_deref(), params.limit)?;

        let output = serde_json::json!({
            "total": proposals.len(),
            "filter": params.status,
            "proposals": proposals,
        });

        Ok(ToolResult {
            output,
            success: true,
            error: None,
            rate_limited: None,
            duration_ms: start.elapsed().as_millis() as u64,
            loads: Vec::new(),
        })
    }
}
