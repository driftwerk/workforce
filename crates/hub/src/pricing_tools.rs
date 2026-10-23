//! Dynamic model pricing management tools (DW-334).
//!
//! Tools:
//! - `pricing.update` — manually set pricing for a model
//! - `pricing.list`   — list all current pricing with staleness information

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use chrono::Utc;
use serde::Deserialize;

use workforce_agent::tools::{Tool, ToolResult};
use workforce_common::cost::{CostTracker, ModelPricing};
use workforce_common::persistence::StoredModelPricing;
use workforce_common::telemetry::TelemetryStore;

/// Shared context for all pricing tools.
#[derive(Clone)]
pub struct PricingToolContext {
    pub store: Arc<dyn TelemetryStore>,
    pub cost_tracker: Arc<CostTracker>,
    pub provider: String,
}

/// Create all pricing management tools.
pub fn pricing_tools(ctx: PricingToolContext) -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(PricingUpdateTool::new(ctx.clone())),
        Arc::new(PricingListTool::new(ctx)),
    ]
}

// ─── pricing.update ──────────────────────────────────────────────────────────

pub struct PricingUpdateTool {
    ctx: PricingToolContext,
}

impl PricingUpdateTool {
    pub fn new(ctx: PricingToolContext) -> Self {
        Self { ctx }
    }
}

#[derive(Deserialize)]
struct PricingUpdateInput {
    model: String,
    input_per_million: f64,
    output_per_million: f64,
}

#[async_trait]
impl Tool for PricingUpdateTool {
    fn name(&self) -> &str {
        "pricing.update"
    }

    fn description(&self) -> &str {
        "Manually set or update pricing for a specific model"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "model": {
                    "type": "string",
                    "description": "The model identifier (e.g. 'claude-sonnet-4-20250514')"
                },
                "input_per_million": {
                    "type": "number",
                    "description": "Cost per 1M input tokens in USD"
                },
                "output_per_million": {
                    "type": "number",
                    "description": "Cost per 1M output tokens in USD"
                }
            },
            "required": ["model", "input_per_million", "output_per_million"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> eyre::Result<ToolResult> {
        let start = Instant::now();
        let params: PricingUpdateInput = serde_json::from_value(input)?;

        // Validate prices
        if params.input_per_million <= 0.0 || params.output_per_million <= 0.0 {
            return Ok(ToolResult {
                output: serde_json::json!({
                    "error": "Both input_per_million and output_per_million must be positive"
                }),
                success: false,
                error: None,
                rate_limited: None,
                duration_ms: start.elapsed().as_millis() as u64,
                loads: Vec::new(),
            });
        }

        let now = Utc::now().to_rfc3339();

        // Update in-memory pricing
        self.ctx.cost_tracker.set_pricing(ModelPricing {
            model: params.model.clone(),
            input_per_million: params.input_per_million,
            output_per_million: params.output_per_million,
            cache_read_per_million: None,
            cache_write_per_million: None,
            source: Some("manual".into()),
            updated_at: Some(now.clone()),
        });

        // Persist to SQLite
        self.ctx.store.upsert_model_pricing(&StoredModelPricing {
            model: params.model.clone(),
            provider: self.ctx.provider.clone(),
            input_per_million: params.input_per_million,
            output_per_million: params.output_per_million,
            source: "manual".to_string(),
            fetched_at: now.clone(),
            updated_at: now.clone(),
        })?;

        let output = serde_json::json!({
            "status": "updated",
            "model": params.model,
            "input_per_million": params.input_per_million,
            "output_per_million": params.output_per_million,
            "source": "manual",
            "updated_at": now,
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

// ─── pricing.list ────────────────────────────────────────────────────────────

pub struct PricingListTool {
    ctx: PricingToolContext,
}

impl PricingListTool {
    pub fn new(ctx: PricingToolContext) -> Self {
        Self { ctx }
    }
}

#[derive(Deserialize)]
struct PricingListInput {
    #[serde(default = "default_show_staleness")]
    show_staleness: bool,
}

fn default_show_staleness() -> bool {
    true
}

#[async_trait]
impl Tool for PricingListTool {
    fn name(&self) -> &str {
        "pricing.list"
    }

    fn description(&self) -> &str {
        "List current model pricing with staleness info"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "show_staleness": {
                    "type": "boolean",
                    "description": "Include staleness information (age_days, stale flag)"
                }
            }
        ,
            "required": []})
    }

    async fn execute(&self, input: serde_json::Value) -> eyre::Result<ToolResult> {
        let start = Instant::now();
        let params: PricingListInput = serde_json::from_value(input).unwrap_or(PricingListInput {
            show_staleness: default_show_staleness(),
        });

        let all_pricing = self.ctx.cost_tracker.get_all_pricing();
        let now = Utc::now();

        let entries: Vec<serde_json::Value> = all_pricing
            .iter()
            .map(|p| {
                let mut entry = serde_json::json!({
                    "model": p.model,
                    "input_per_million": p.input_per_million,
                    "output_per_million": p.output_per_million,
                    "source": p.source,
                    "updated_at": p.updated_at,
                });

                if params.show_staleness {
                    let (age_days, stale) = if let Some(ref updated_at) = p.updated_at {
                        if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(updated_at) {
                            let age = now.signed_duration_since(dt.with_timezone(&Utc));
                            let days = age.num_days();
                            (Some(days), days > 7)
                        } else {
                            (None, true)
                        }
                    } else {
                        (None, true)
                    };

                    entry["age_days"] = serde_json::json!(age_days);
                    entry["stale"] = serde_json::json!(stale);
                }

                entry
            })
            .collect();

        let stale_count = if params.show_staleness {
            entries
                .iter()
                .filter(|e| e.get("stale").and_then(|v| v.as_bool()).unwrap_or(false))
                .count()
        } else {
            0
        };

        let output = serde_json::json!({
            "total": entries.len(),
            "stale_count": stale_count,
            "entries": entries,
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Every Gemini model string the driftwerk ledger has recorded (read-only
    /// query of `telemetry.cost_records`, 2026-09-29) is priced by the table
    /// the hub loads at startup, not the $3/$15 unknown-model fallback.
    #[test]
    fn gemini_flash_models_are_priced() {
        let tracker = CostTracker::new();
        tracker.load_pricing_toml(workforce_agent::llm::DEFAULT_PRICING_TOML, None);
        let unpriced: Vec<&str> = [
            "gemini-2.5-pro",
            "gemini-3-flash-preview",
            "gemini-3.1-flash-lite-preview",
            "gemini-3.1-pro-preview",
            "gemini-3.6-flash",
            "gemini-3.8-flash",
            "google/gemini-2.0-flash",
            "google/gemini-2.5-pro",
            "google/gemini-3-flash-preview",
            "google/gemini-3-flash-preview-20251217",
            "google/gemini-3.1-pro-preview",
        ]
        .into_iter()
        .filter(|model| !tracker.is_priced(model))
        .collect();
        assert!(unpriced.is_empty(), "unpriced: {unpriced:?}");

        let cost = tracker.calculate_cost("gemini-3.8-flash", 1_000_000, 1_000_000, 0, 1_000_000);
        assert!((cost - 9.15).abs() < 1e-9, "{cost}");
    }
}
