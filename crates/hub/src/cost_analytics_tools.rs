//! Cost analytics tools (DW-335, DW-340).
//!
//! Tools:
//! - `costs.analyze`  — analyse API-equivalent usage costs for a period
//! - `costs.forecast` — forecast future usage and costs

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use serde::Deserialize;

use workforce_agent::tools::{Tool, ToolResult};
use workforce_common::cost_analytics::CostAnalytics;
use workforce_common::telemetry::TelemetryStore;

/// Shared context for cost analytics tools.
#[derive(Clone)]
pub struct CostAnalyticsToolContext {
    pub store: Arc<dyn TelemetryStore>,
    pub agent_id: String,
}

/// Create all cost analytics tools.
pub fn cost_analytics_tools(ctx: CostAnalyticsToolContext) -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(CostsAnalyzeTool::new(ctx.clone())),
        Arc::new(CostsForecastTool::new(ctx)),
    ]
}

// ─── costs.analyze ──────────────────────────────────────────────────────────

pub struct CostsAnalyzeTool {
    ctx: CostAnalyticsToolContext,
}

impl CostsAnalyzeTool {
    pub fn new(ctx: CostAnalyticsToolContext) -> Self {
        Self { ctx }
    }
}

#[derive(Deserialize)]
struct AnalyzeInput {
    #[serde(default = "default_analyze_period")]
    period: String,
}

fn default_analyze_period() -> String {
    "current_month".to_string()
}

#[async_trait]
impl Tool for CostsAnalyzeTool {
    fn name(&self) -> &str {
        "costs.analyze"
    }

    fn description(&self) -> &str {
        "Analyse API-equivalent usage costs for a time period"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "period": {
                    "type": "string",
                    "description": "Time period",
                    "enum": ["current_month", "previous_month", "last_3_months", "30d", "90d"]
                }
            }
        ,
            "required": []})
    }

    async fn execute(&self, input: serde_json::Value) -> eyre::Result<ToolResult> {
        let start = Instant::now();
        let params: AnalyzeInput = serde_json::from_value(input).unwrap_or(AnalyzeInput {
            period: default_analyze_period(),
        });

        let analytics = CostAnalytics::new(Arc::clone(&self.ctx.store));

        let output = match params.period.as_str() {
            "previous_month" => {
                let period = CostAnalytics::calendar_month(-1);
                let analysis = analytics.analyze_usage(&period)?;
                serde_json::to_value(&analysis)?
            }
            "last_3_months" => {
                let mut results = Vec::new();
                for offset in (-2..=0).rev() {
                    let period = CostAnalytics::calendar_month(offset);
                    let analysis = analytics.analyze_usage(&period)?;
                    results.push(analysis);
                }
                serde_json::to_value(&results)?
            }
            "30d" => {
                let period = CostAnalytics::rolling_days(30);
                let analysis = analytics.analyze_usage(&period)?;
                serde_json::to_value(&analysis)?
            }
            "90d" => {
                let period = CostAnalytics::rolling_days(90);
                let analysis = analytics.analyze_usage(&period)?;
                serde_json::to_value(&analysis)?
            }
            _ => {
                // "current_month"
                let period = CostAnalytics::calendar_month(0);
                let analysis = analytics.analyze_usage(&period)?;
                serde_json::to_value(&analysis)?
            }
        };

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

// ─── costs.forecast ─────────────────────────────────────────────────────────

pub struct CostsForecastTool {
    ctx: CostAnalyticsToolContext,
}

impl CostsForecastTool {
    pub fn new(ctx: CostAnalyticsToolContext) -> Self {
        Self { ctx }
    }
}

#[derive(Deserialize)]
struct ForecastInput {
    #[serde(default)]
    days_ahead: Option<u32>,
}

#[async_trait]
impl Tool for CostsForecastTool {
    fn name(&self) -> &str {
        "costs.forecast"
    }

    fn description(&self) -> &str {
        "Forecast future usage and API-equivalent costs"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "days_ahead": {
                    "type": "integer",
                    "description": "Number of days to forecast. Default: 30."
                }
            }
        ,
            "required": []})
    }

    async fn execute(&self, input: serde_json::Value) -> eyre::Result<ToolResult> {
        let start = Instant::now();
        let params: ForecastInput =
            serde_json::from_value(input).unwrap_or(ForecastInput { days_ahead: None });

        let days_ahead = params.days_ahead.unwrap_or(30);

        let analytics = CostAnalytics::new(Arc::clone(&self.ctx.store));
        let forecast = analytics.forecast_usage(days_ahead)?;
        let output = serde_json::to_value(&forecast)?;

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
