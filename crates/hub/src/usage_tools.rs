//! Agent usage statistics tools — let agents query their own performance data
//! and set model preferences for self-optimization.
//!
//! Tools:
//! - `stats.my_usage` — aggregated usage stats by model or task type
//! - `stats.compare_models` — compare model performance (cost, latency, success)
//! - `stats.task_history` — recent task execution details
//! - `stats.set_model_preference` — set preferred model for a task type
//! - `stats.efficiency_report` — time-series efficiency trends

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use serde::Deserialize;

use workforce_agent::tools::{Tool, ToolResult};
use workforce_common::persistence::AgentModelPreference;
use workforce_common::telemetry::TelemetryStore;

/// Shared context for all usage tools.
#[derive(Clone)]
pub struct UsageToolContext {
    pub store: Arc<dyn TelemetryStore>,
    pub agent_id: String,
    pub allowed_models: Vec<String>,
}

/// Create all usage statistics tools.
pub fn usage_tools(ctx: UsageToolContext) -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(StatsMyUsageTool::new(ctx.clone())),
        Arc::new(StatsCompareModelsTool::new(ctx.clone())),
        Arc::new(StatsTaskHistoryTool::new(ctx.clone())),
        Arc::new(StatsSetModelPreferenceTool::new(ctx.clone())),
        Arc::new(StatsEfficiencyReportTool::new(ctx.clone())),
        Arc::new(StatsCacheStatusTool::new(ctx.clone())),
        Arc::new(StatsToolCostsTool::new(ctx.clone())),
        Arc::new(StatsRoutingHistoryTool::new(ctx.clone())),
        Arc::new(StatsOutlierAnalysisTool::new(ctx.clone())),
        Arc::new(StatsTicketCostTool::new(ctx)),
    ]
}

// ─── stats.my_usage ──────────────────────────────────────────────────────────

pub struct StatsMyUsageTool {
    ctx: UsageToolContext,
}

impl StatsMyUsageTool {
    pub fn new(ctx: UsageToolContext) -> Self {
        Self { ctx }
    }
}

#[derive(Deserialize)]
struct MyUsageInput {
    #[serde(default = "default_period")]
    period: String,
    #[serde(default = "default_group_by")]
    group_by: String,
}

fn default_period() -> String {
    "7d".to_string()
}
fn default_group_by() -> String {
    "model".to_string()
}

#[async_trait]
impl Tool for StatsMyUsageTool {
    fn name(&self) -> &str {
        "stats.my_usage"
    }

    fn description(&self) -> &str {
        "Get LLM usage analytics and statistics"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "period": {
                    "type": "string",
                    "description": "Time period: '1d', '7d', '30d', '90d'. Default: '7d'",
                    "enum": ["1d", "7d", "30d", "90d"]
                },
                "group_by": {
                    "type": "string",
                    "description": "Group results by 'model', 'task_type', or 'provider'",
                    "enum": ["model", "task_type", "provider"]
                }
            }
        ,
            "required": []})
    }

    async fn execute(&self, input: serde_json::Value) -> eyre::Result<ToolResult> {
        let start = Instant::now();
        let params: MyUsageInput = serde_json::from_value(input).unwrap_or(MyUsageInput {
            period: default_period(),
            group_by: default_group_by(),
        });

        let (from, to) = period_to_range(&params.period);
        let summary = self.ctx.store.get_agent_usage_summary(
            &self.ctx.agent_id,
            from,
            to,
            &params.group_by,
        )?;

        let output = serde_json::json!({
            "agent_id": self.ctx.agent_id,
            "period": params.period,
            "group_by": params.group_by,
            "results": summary,
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

// ─── stats.compare_models ────────────────────────────────────────────────────

pub struct StatsCompareModelsTool {
    ctx: UsageToolContext,
}

impl StatsCompareModelsTool {
    pub fn new(ctx: UsageToolContext) -> Self {
        Self { ctx }
    }
}

#[derive(Deserialize)]
struct CompareModelsInput {
    #[serde(default = "default_period")]
    period: String,
}

#[async_trait]
impl Tool for StatsCompareModelsTool {
    fn name(&self) -> &str {
        "stats.compare_models"
    }

    fn description(&self) -> &str {
        "Compare your performance across different models"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "period": {
                    "type": "string",
                    "description": "Time period: '1d', '7d', '30d', '90d'. Default: '7d'",
                    "enum": ["1d", "7d", "30d", "90d"]
                }
            }
        ,
            "required": []})
    }

    async fn execute(&self, input: serde_json::Value) -> eyre::Result<ToolResult> {
        let start = Instant::now();
        let params: CompareModelsInput =
            serde_json::from_value(input).unwrap_or(CompareModelsInput {
                period: default_period(),
            });

        let (from, to) = period_to_range(&params.period);
        let comparison = self
            .ctx
            .store
            .compare_models_for_agent(&self.ctx.agent_id, from, to)?;

        let output = serde_json::json!({
            "agent_id": self.ctx.agent_id,
            "period": params.period,
            "models": comparison,
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

// ─── stats.task_history ──────────────────────────────────────────────────────

pub struct StatsTaskHistoryTool {
    ctx: UsageToolContext,
}

impl StatsTaskHistoryTool {
    pub fn new(ctx: UsageToolContext) -> Self {
        Self { ctx }
    }
}

#[derive(Deserialize)]
struct TaskHistoryInput {
    #[serde(default = "default_limit")]
    limit: usize,
    task_type: Option<String>,
    model: Option<String>,
    /// Time period filter: "1d", "7d", "30d", "90d".
    period: Option<String>,
    /// Sort by: "cost", "duration", "tokens", or default "completed_at".
    sort_by: Option<String>,
}

fn default_limit() -> usize {
    20
}

#[async_trait]
impl Tool for StatsTaskHistoryTool {
    fn name(&self) -> &str {
        "stats.task_history"
    }

    fn description(&self) -> &str {
        "Review your recent task execution history"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "limit": {
                    "type": "integer",
                    "description": "Max number of tasks to return (default: 20, max: 100)"
                },
                "task_type": {
                    "type": "string",
                    "description": "Filter by task type (e.g., 'coding', 'writing')"
                },
                "model": {
                    "type": "string",
                    "description": "Filter by model name"
                },
                "period": {
                    "type": "string",
                    "description": "Time period filter: '1d', '7d', '30d', '90d'",
                    "enum": ["1d", "7d", "30d", "90d"]
                },
                "sort_by": {
                    "type": "string",
                    "description": "Sort results by",
                    "enum": ["cost", "duration", "tokens", "completed_at"]
                }
            }
        ,
            "required": []})
    }

    async fn execute(&self, input: serde_json::Value) -> eyre::Result<ToolResult> {
        let start = Instant::now();
        let params: TaskHistoryInput = serde_json::from_value(input).unwrap_or(TaskHistoryInput {
            limit: default_limit(),
            task_type: None,
            model: None,
            period: None,
            sort_by: None,
        });

        let limit = params.limit.min(100);
        let since = params.period.as_ref().map(|p| {
            let (from, _) = period_to_range(p);
            from.to_rfc3339()
        });
        let history = self.ctx.store.get_task_history_filtered(
            &self.ctx.agent_id,
            limit,
            params.task_type.as_deref(),
            params.model.as_deref(),
            since.as_deref(),
            params.sort_by.as_deref(),
        )?;

        let output = serde_json::json!({
            "agent_id": self.ctx.agent_id,
            "tasks": history,
            "count": history.len(),
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

// ─── stats.set_model_preference ──────────────────────────────────────────────

pub struct StatsSetModelPreferenceTool {
    ctx: UsageToolContext,
}

impl StatsSetModelPreferenceTool {
    pub fn new(ctx: UsageToolContext) -> Self {
        Self { ctx }
    }
}

#[derive(Deserialize)]
struct SetModelPreferenceInput {
    task_type: String,
    model: String,
    reason: Option<String>,
}

#[async_trait]
impl Tool for StatsSetModelPreferenceTool {
    fn name(&self) -> &str {
        "stats.set_model_preference"
    }

    fn description(&self) -> &str {
        "Set your preferred model for a specific task type"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "task_type": {
                    "type": "string",
                    "description": "The task type to set a preference for"
                },
                "model": {
                    "type": "string",
                    "description": "The model to prefer for this task type"
                },
                "reason": {
                    "type": "string",
                    "description": "Why you chose this model (for audit/review)"
                }
            },
            "required": ["task_type", "model"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> eyre::Result<ToolResult> {
        let start = Instant::now();
        let params: SetModelPreferenceInput = serde_json::from_value(input)?;

        // Validate model is in allowed list
        if !self.ctx.allowed_models.is_empty() && !self.ctx.allowed_models.contains(&params.model) {
            return Ok(ToolResult {
                output: serde_json::json!({
                    "error": format!(
                        "Model '{}' is not in your allowed models list. Available: {:?}",
                        params.model, self.ctx.allowed_models
                    ),
                }),
                success: false,
                error: None,
                rate_limited: None,
                duration_ms: start.elapsed().as_millis() as u64,
                loads: Vec::new(),
            });
        }

        let pref = AgentModelPreference {
            agent_id: self.ctx.agent_id.clone(),
            task_type: params.task_type.clone(),
            preferred_model: params.model.clone(),
            reason: params.reason.clone(),
            updated_at: chrono::Utc::now().to_rfc3339(),
        };

        self.ctx.store.upsert_agent_model_preference(&pref)?;

        let output = serde_json::json!({
            "status": "preference_saved",
            "agent_id": self.ctx.agent_id,
            "task_type": params.task_type,
            "preferred_model": params.model,
            "reason": params.reason,
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

// ─── stats.efficiency_report ─────────────────────────────────────────────────

pub struct StatsEfficiencyReportTool {
    ctx: UsageToolContext,
}

impl StatsEfficiencyReportTool {
    pub fn new(ctx: UsageToolContext) -> Self {
        Self { ctx }
    }
}

#[derive(Deserialize)]
struct EfficiencyReportInput {
    #[serde(default = "default_period")]
    period: String,
}

#[async_trait]
impl Tool for StatsEfficiencyReportTool {
    fn name(&self) -> &str {
        "stats.efficiency_report"
    }

    fn description(&self) -> &str {
        "Generate an efficiency report showing trends over time"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "period": {
                    "type": "string",
                    "description": "Time period: '1d', '7d', '30d', '90d'. Default: '7d'",
                    "enum": ["1d", "7d", "30d", "90d"]
                }
            }
        ,
            "required": []})
    }

    async fn execute(&self, input: serde_json::Value) -> eyre::Result<ToolResult> {
        let start = Instant::now();
        let params: EfficiencyReportInput =
            serde_json::from_value(input).unwrap_or(EfficiencyReportInput {
                period: default_period(),
            });

        let (from, to) = period_to_range(&params.period);

        let trends = self
            .ctx
            .store
            .get_efficiency_trends(&self.ctx.agent_id, from, to)?;

        // Also get current model preference
        let preferences: Vec<_> = ["coding", "writing", "analysis", "review"]
            .iter()
            .filter_map(|tt| {
                self.ctx
                    .store
                    .get_agent_model_preference(&self.ctx.agent_id, tt)
                    .ok()
                    .flatten()
            })
            .collect();

        let output = serde_json::json!({
            "agent_id": self.ctx.agent_id,
            "period": params.period,
            "daily_trends": trends,
            "current_preferences": preferences,
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

// ─── stats.cache_status ──────────────────────────────────────────────────────

pub struct StatsCacheStatusTool {
    ctx: UsageToolContext,
}

impl StatsCacheStatusTool {
    pub fn new(ctx: UsageToolContext) -> Self {
        Self { ctx }
    }
}

#[derive(Deserialize)]
struct CacheStatusInput {
    #[serde(default = "default_cache_period")]
    period: String,
}

fn default_cache_period() -> String {
    "1d".to_string()
}

#[async_trait]
impl Tool for StatsCacheStatusTool {
    fn name(&self) -> &str {
        "stats.cache_status"
    }

    fn description(&self) -> &str {
        "View prompt cache performance in real-time"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "period": {
                    "type": "string",
                    "description": "Time period: '1d' (default), '7d', '30d'",
                    "enum": ["1d", "7d", "30d"]
                }
            }
        ,
            "required": []})
    }

    async fn execute(&self, input: serde_json::Value) -> eyre::Result<ToolResult> {
        let start = Instant::now();
        let params: CacheStatusInput = serde_json::from_value(input).unwrap_or(CacheStatusInput {
            period: default_cache_period(),
        });

        let (from, _) = period_to_range(&params.period);
        let entries = self.ctx.store.get_cache_status(&self.ctx.agent_id, from)?;
        let (total_rounds, rounds_with_hits) = self
            .ctx
            .store
            .get_round_cache_stats(&self.ctx.agent_id, from)?;

        let total_created: u64 = entries.iter().map(|e| e.total_cache_creation_tokens).sum();
        let total_read: u64 = entries.iter().map(|e| e.total_cache_read_tokens).sum();
        let total_input: u64 = entries.iter().map(|e| e.total_input_tokens).sum();

        let cache_hit_rate_pct = if total_rounds > 0 {
            rounds_with_hits as f64 / total_rounds as f64 * 100.0
        } else {
            0.0
        };

        // Cache savings: tokens read from cache instead of recomputed
        let cache_savings_pct = if total_input > 0 {
            total_read as f64 / total_input as f64 * 100.0
        } else {
            0.0
        };

        let output = serde_json::json!({
            "agent_id": self.ctx.agent_id,
            "period": params.period,
            "total_cache_creation_tokens": total_created,
            "total_cache_read_tokens": total_read,
            "total_input_tokens": total_input,
            "cache_hit_rate_pct": (cache_hit_rate_pct * 10.0).round() / 10.0,
            "cache_savings_pct": (cache_savings_pct * 10.0).round() / 10.0,
            "rounds_with_cache_hits": rounds_with_hits,
            "total_rounds": total_rounds,
            "by_model": entries,
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

// ─── stats.tool_costs ────────────────────────────────────────────────────────

pub struct StatsToolCostsTool {
    ctx: UsageToolContext,
}

impl StatsToolCostsTool {
    pub fn new(ctx: UsageToolContext) -> Self {
        Self { ctx }
    }
}

#[derive(Deserialize)]
struct ToolCostsInput {
    #[serde(default = "default_period")]
    period: String,
    #[serde(default = "default_limit")]
    limit: usize,
}

#[async_trait]
impl Tool for StatsToolCostsTool {
    fn name(&self) -> &str {
        "stats.tool_costs"
    }

    fn description(&self) -> &str {
        "View cost breakdown by tool"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "period": {
                    "type": "string",
                    "description": "Time period: '1d', '7d' (default), '30d', '90d'",
                    "enum": ["1d", "7d", "30d", "90d"]
                },
                "limit": {
                    "type": "integer",
                    "description": "Max number of tools to return (default: 20)"
                }
            }
        ,
            "required": []})
    }

    async fn execute(&self, input: serde_json::Value) -> eyre::Result<ToolResult> {
        let start = Instant::now();
        let params: ToolCostsInput = serde_json::from_value(input).unwrap_or(ToolCostsInput {
            period: default_period(),
            limit: default_limit(),
        });

        let (from, _) = period_to_range(&params.period);
        let limit = params.limit.min(100);
        let tools = self
            .ctx
            .store
            .get_tool_cost_breakdown(&self.ctx.agent_id, from, limit)?;

        let total_invocations: u64 = tools.iter().map(|t| t.invocations).sum();
        let total_duration: u64 = tools.iter().map(|t| t.total_duration_ms).sum();

        let output = serde_json::json!({
            "agent_id": self.ctx.agent_id,
            "period": params.period,
            "tools": tools,
            "total_tool_invocations": total_invocations,
            "total_tool_duration_ms": total_duration,
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

// ─── stats.routing_history ───────────────────────────────────────────────────

pub struct StatsRoutingHistoryTool {
    ctx: UsageToolContext,
}

impl StatsRoutingHistoryTool {
    pub fn new(ctx: UsageToolContext) -> Self {
        Self { ctx }
    }
}

#[derive(Deserialize)]
struct RoutingHistoryInput {
    #[serde(default = "default_routing_period")]
    period: String,
    #[serde(default = "default_routing_limit")]
    limit: usize,
}

fn default_routing_period() -> String {
    "30d".to_string()
}
fn default_routing_limit() -> usize {
    50
}

#[async_trait]
impl Tool for StatsRoutingHistoryTool {
    fn name(&self) -> &str {
        "stats.routing_history"
    }

    fn description(&self) -> &str {
        "Configure task-type model routing preferences"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "period": {
                    "type": "string",
                    "description": "Time period: '1d', '7d', '30d' (default), '90d'",
                    "enum": ["1d", "7d", "30d", "90d"]
                },
                "limit": {
                    "type": "integer",
                    "description": "Max number of tasks to return (default: 50, max: 100)"
                }
            }
        ,
            "required": []})
    }

    async fn execute(&self, input: serde_json::Value) -> eyre::Result<ToolResult> {
        let start = Instant::now();
        let params: RoutingHistoryInput =
            serde_json::from_value(input).unwrap_or(RoutingHistoryInput {
                period: default_routing_period(),
                limit: default_routing_limit(),
            });

        let (from, _) = period_to_range(&params.period);
        let limit = params.limit.min(100);
        let tasks = self
            .ctx
            .store
            .get_routing_history(&self.ctx.agent_id, from, limit)?;

        let mut total_switches: u64 = 0;
        let mut switch_patterns: std::collections::HashMap<String, u64> =
            std::collections::HashMap::new();
        let mut by_task_type: std::collections::HashMap<String, u64> =
            std::collections::HashMap::new();
        let mut recent_switches: Vec<serde_json::Value> = Vec::new();

        for task in &tasks {
            if let Some(ref switches_json) = task.model_switches {
                if let Ok(switches) = serde_json::from_str::<Vec<serde_json::Value>>(switches_json)
                {
                    let count = switches.len() as u64;
                    total_switches += count;

                    let task_type = task.task_type.as_deref().unwrap_or("unknown").to_string();
                    *by_task_type.entry(task_type).or_default() += count;

                    for sw in &switches {
                        let from_m = sw.get("from_model").and_then(|v| v.as_str()).unwrap_or("?");
                        let to_m = sw.get("to_model").and_then(|v| v.as_str()).unwrap_or("?");
                        let key = format!("{}→{}", from_m, to_m);
                        *switch_patterns.entry(key).or_default() += 1;

                        if recent_switches.len() < 20 {
                            recent_switches.push(serde_json::json!({
                                "task_id": task.task_id,
                                "task_type": task.task_type,
                                "round": sw.get("round"),
                                "from_model": from_m,
                                "to_model": to_m,
                                "reason": sw.get("reason"),
                                "completed_at": task.completed_at,
                            }));
                        }
                    }
                }
            }
        }

        let output = serde_json::json!({
            "agent_id": self.ctx.agent_id,
            "period": params.period,
            "total_tasks_with_switches": tasks.len(),
            "total_switches": total_switches,
            "switch_patterns": switch_patterns,
            "by_task_type": by_task_type,
            "recent_switches": recent_switches,
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

// ─── stats.outlier_analysis ──────────────────────────────────────────────────

pub struct StatsOutlierAnalysisTool {
    ctx: UsageToolContext,
}

impl StatsOutlierAnalysisTool {
    pub fn new(ctx: UsageToolContext) -> Self {
        Self { ctx }
    }
}

#[derive(Deserialize)]
struct OutlierAnalysisInput {
    #[serde(default = "default_outlier_period")]
    period: String,
    #[serde(default = "default_threshold")]
    threshold: f64,
    #[serde(default = "default_outlier_limit")]
    limit: usize,
}

fn default_outlier_period() -> String {
    "30d".to_string()
}
fn default_threshold() -> f64 {
    2.0
}
fn default_outlier_limit() -> usize {
    20
}

#[async_trait]
impl Tool for StatsOutlierAnalysisTool {
    fn name(&self) -> &str {
        "stats.outlier_analysis"
    }

    fn description(&self) -> &str {
        "Identify tasks with unusually high round counts (outliers)"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "period": {
                    "type": "string",
                    "description": "Time period: '1d', '7d', '30d' (default), '90d'",
                    "enum": ["1d", "7d", "30d", "90d"]
                },
                "threshold": {
                    "type": "number",
                    "description": "Standard deviation multiplier for outlier detection (default"
                },
                "limit": {
                    "type": "integer",
                    "description": "Max outliers to return (default: 20)"
                }
            }
        ,
            "required": []})
    }

    async fn execute(&self, input: serde_json::Value) -> eyre::Result<ToolResult> {
        let start = Instant::now();
        let params: OutlierAnalysisInput =
            serde_json::from_value(input).unwrap_or(OutlierAnalysisInput {
                period: default_outlier_period(),
                threshold: default_threshold(),
                limit: default_outlier_limit(),
            });

        let (from, _) = period_to_range(&params.period);
        let limit = params.limit.min(100);
        let threshold = params.threshold.max(0.5); // min 0.5 stddev

        let (stats, outliers) =
            self.ctx
                .store
                .get_round_outliers(&self.ctx.agent_id, from, threshold, limit)?;

        let threshold_rounds = stats.mean + threshold * stats.stddev;

        // Aggregate patterns from outliers
        let mut by_task_type: std::collections::HashMap<String, (u64, f64)> =
            std::collections::HashMap::new();
        let mut by_model: std::collections::HashMap<String, (u64, f64)> =
            std::collections::HashMap::new();

        for o in &outliers {
            let task_type = o.task_type.as_deref().unwrap_or("unknown").to_string();
            let entry = by_task_type.entry(task_type).or_default();
            entry.0 += 1;
            entry.1 += o.rounds as f64;

            let entry = by_model.entry(o.model.clone()).or_default();
            entry.0 += 1;
            entry.1 += o.rounds as f64;
        }

        let patterns_by_task_type: serde_json::Value = by_task_type
            .iter()
            .map(|(k, (count, total_rounds))| {
                (
                    k.clone(),
                    serde_json::json!({
                        "count": count,
                        "avg_rounds": if *count > 0 { total_rounds / *count as f64 } else { 0.0 },
                    }),
                )
            })
            .collect::<serde_json::Map<String, serde_json::Value>>()
            .into();

        let patterns_by_model: serde_json::Value = by_model
            .iter()
            .map(|(k, (count, total_rounds))| {
                (
                    k.clone(),
                    serde_json::json!({
                        "count": count,
                        "avg_rounds": if *count > 0 { total_rounds / *count as f64 } else { 0.0 },
                    }),
                )
            })
            .collect::<serde_json::Map<String, serde_json::Value>>()
            .into();

        let output = serde_json::json!({
            "agent_id": self.ctx.agent_id,
            "period": params.period,
            "stats": stats,
            "threshold_multiplier": threshold,
            "threshold_rounds": (threshold_rounds * 10.0).round() / 10.0,
            "outlier_count": outliers.len(),
            "outliers": outliers,
            "patterns": {
                "by_task_type": patterns_by_task_type,
                "by_model": patterns_by_model,
            },
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

// ─── stats.ticket_cost (DW-400) ──────────────────────────────────────────────

pub struct StatsTicketCostTool {
    ctx: UsageToolContext,
}

impl StatsTicketCostTool {
    pub fn new(ctx: UsageToolContext) -> Self {
        Self { ctx }
    }
}

#[derive(Deserialize)]
struct TicketCostInput {
    identifier: String,
}

#[async_trait]
impl Tool for StatsTicketCostTool {
    fn name(&self) -> &str {
        "stats.ticket_cost"
    }

    fn description(&self) -> &str {
        "Get the total cost of implementing a ticket"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "identifier": {
                    "type": "string",
                    "description": "Ticket identifier (e.g. 'DW-400')"
                }
            },
            "required": ["identifier"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> eyre::Result<ToolResult> {
        let start = Instant::now();
        let params: TicketCostInput = serde_json::from_value(input)?;

        let summary = self.ctx.store.cost_by_ticket(&params.identifier)?;

        let output = serde_json::json!({
            "ticket_id": summary.ticket_id,
            "total_cost_usd": summary.total_cost_usd,
            "total_input_tokens": summary.total_input_tokens,
            "total_output_tokens": summary.total_output_tokens,
            "total_rounds": summary.total_rounds,
            "total_duration_ms": summary.total_duration_ms,
            "step_count": summary.step_count,
            "models_used": summary.models_used,
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

// ─── Helpers ─────────────────────────────────────────────────────────────────

fn period_to_range(period: &str) -> (chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>) {
    let now = chrono::Utc::now();
    let days = match period {
        "1d" => 1,
        "7d" => 7,
        "30d" => 30,
        "90d" => 90,
        _ => 7,
    };
    let from = now - chrono::Duration::days(days);
    (from, now)
}
