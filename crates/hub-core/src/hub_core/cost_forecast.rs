use chrono::Utc;
use eyre::Result;
use std::sync::Arc;
use tracing::debug;
use workforce_common::telemetry::TelemetryStore;

/// Cost forecast for a task before execution.
#[derive(Debug, Clone)]
pub struct CostForecast {
    /// Estimated cost in USD.
    pub estimated_cost_usd: f64,
    /// Estimated number of rounds.
    pub estimated_rounds: u32,
    /// Estimated input tokens.
    pub estimated_input_tokens: u64,
    /// Estimated output tokens.
    pub estimated_output_tokens: u64,
    /// Confidence level (0.0-1.0).
    pub confidence: f64,
    /// Basis for the estimate (e.g., "historical average for task_type=coding").
    pub basis: String,
    /// Whether this task should trigger a cost warning.
    pub warn: bool,
}

impl CostForecast {
    /// Default fallback forecast when no historical data is available.
    pub fn default_estimate() -> Self {
        Self {
            estimated_cost_usd: 0.12, // System average
            estimated_rounds: 5,
            estimated_input_tokens: 8000,
            estimated_output_tokens: 2000,
            confidence: 0.3, // Low confidence
            basis: "system default (no historical data)".to_string(),
            warn: false,
        }
    }

    /// Project this per-run estimate over a recurring cadence.
    pub fn recurring(&self, runs_per_day: f64) -> RecurringForecast {
        let per_day = self.estimated_cost_usd * runs_per_day;
        RecurringForecast {
            runs_per_day,
            per_run_usd: self.estimated_cost_usd,
            per_day_usd: per_day,
            per_month_usd: per_day * 30.0,
        }
    }
}

/// A per-run estimate projected over a recurring schedule.
#[derive(Debug, Clone, PartialEq)]
pub struct RecurringForecast {
    pub runs_per_day: f64,
    pub per_run_usd: f64,
    pub per_day_usd: f64,
    pub per_month_usd: f64,
}

/// Round to cents for user-facing estimate payloads.
pub fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

/// Cost forecasting engine.
pub struct CostForecaster {
    store: Arc<dyn TelemetryStore>,
    /// Cost threshold in USD for warnings (`[budget] warn_turn_usd`).
    warn_threshold: f64,
}

impl CostForecaster {
    pub fn new(store: Arc<dyn TelemetryStore>, warn_threshold: f64) -> Self {
        Self {
            store,
            warn_threshold,
        }
    }

    /// Predict the cost of a task before execution.
    ///
    /// Uses historical data for the agent, task type, and model to forecast:
    /// - Estimated cost
    /// - Estimated rounds
    /// - Estimated token usage
    /// - Confidence level
    /// - Whether to warn the user
    pub async fn forecast(
        &self,
        agent_id: &str,
        task_type: Option<&str>,
        model: &str,
    ) -> Result<CostForecast> {
        // Strategy:
        // 1. Try to get historical avg for (agent, task_type, model)
        // 2. Fall back to (agent, task_type) across all models
        // 3. Fall back to (agent, model) across all task types
        // 4. Fall back to global agent average
        // 5. Use system default

        let from = Utc::now() - chrono::Duration::days(30); // Last 30 days
        let from_str = from.to_rfc3339();

        // Try 1: Task type + model specific
        if let Some(tt) = task_type {
            if let Ok(history) = self.store.get_task_history_filtered(
                agent_id,
                100,
                Some(tt),
                Some(model),
                Some(&from_str),
                None,
            ) {
                if !history.is_empty() {
                    debug!(
                        "Cost forecast: Using {} tasks for task_type={}, model={}",
                        history.len(),
                        tt,
                        model
                    );
                    return Ok(self.build_forecast_from_history(&history, tt, model));
                }
            }
        }

        // Try 2: Task type only (any model)
        if let Some(tt) = task_type {
            if let Ok(history) = self.store.get_task_history_filtered(
                agent_id,
                100,
                Some(tt),
                None,
                Some(&from_str),
                None,
            ) {
                if !history.is_empty() {
                    debug!(
                        "Cost forecast: Using {} tasks for task_type={} (any model)",
                        history.len(),
                        tt
                    );
                    return Ok(self.build_forecast_from_history(&history, tt, "any model"));
                }
            }
        }

        // Try 3: Model only (any task type)
        if let Ok(history) = self.store.get_task_history_filtered(
            agent_id,
            100,
            None,
            Some(model),
            Some(&from_str),
            None,
        ) {
            if !history.is_empty() {
                debug!(
                    "Cost forecast: Using {} tasks for model={} (any task type)",
                    history.len(),
                    model
                );
                return Ok(self.build_forecast_from_history(&history, "any type", model));
            }
        }

        // Try 4: Global agent average (last 100 tasks)
        if let Ok(history) =
            self.store
                .get_task_history_filtered(agent_id, 100, None, None, Some(&from_str), None)
        {
            if !history.is_empty() {
                debug!(
                    "Cost forecast: Using {} tasks (global agent average)",
                    history.len()
                );
                return Ok(self.build_forecast_from_history(&history, "any type", "any model"));
            }
        }

        // Fallback: System default
        debug!("Cost forecast: No historical data, using system default");
        Ok(CostForecast::default_estimate())
    }

    /// Build a forecast from historical task data.
    fn build_forecast_from_history(
        &self,
        history: &[workforce_common::persistence::TaskHistoryEntry],
        task_type_label: &str,
        model_label: &str,
    ) -> CostForecast {
        let count = history.len() as f64;
        let avg_cost = history.iter().map(|t| t.cost_usd).sum::<f64>() / count;
        let avg_rounds = history.iter().map(|t| t.rounds as f64).sum::<f64>() / count;
        let avg_input = history.iter().map(|t| t.input_tokens as f64).sum::<f64>() / count;
        let avg_output = history.iter().map(|t| t.output_tokens as f64).sum::<f64>() / count;

        // Confidence based on sample size
        let confidence = match history.len() {
            0..=5 => 0.5,
            6..=20 => 0.7,
            _ => 0.9,
        };

        let warn = avg_cost > self.warn_threshold;

        CostForecast {
            estimated_cost_usd: avg_cost,
            estimated_rounds: avg_rounds.round() as u32,
            estimated_input_tokens: avg_input.round() as u64,
            estimated_output_tokens: avg_output.round() as u64,
            confidence,
            basis: format!(
                "avg of {} tasks (task_type={}, model={})",
                history.len(),
                task_type_label,
                model_label
            ),
            warn,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use workforce_common::persistence::{SqliteStore, TaskTelemetryRecord};

    #[tokio::test]
    async fn test_forecast_with_historical_data() {
        let store: Arc<dyn TelemetryStore> = Arc::new(SqliteStore::in_memory().unwrap());

        // Insert some historical tasks
        for i in 0..10 {
            let task = TaskTelemetryRecord {
                agent_id: "test-agent".to_string(),
                model: "claude-sonnet-4-5-20250929".to_string(),
                provider: "anthropic".to_string(),
                input_tokens: 5000,
                output_tokens: 1000,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 0,
                cost_usd: 0.25,
                rounds: 3,
                duration_ms: 5000,
                tool_invocation_count: 2,
                success: true,
                error_type: None,
                retry_count: 0,
                task_type: Some("coding".to_string()),
                task_id: Some(format!("task-{}", i)),
                started_at: Utc::now().to_rfc3339(),
                completed_at: Utc::now().to_rfc3339(),
                forecast_cost_usd: None,
                forecast_rounds: None,
                forecast_confidence: None,
                team_id: None,
                session_id: None,
                model_switches: None,
                ticket_id: None,
                identity_id: None,
                description: None,
            };
            store.record_task_telemetry(&task, &[], &[]).unwrap();
        }

        let forecaster = CostForecaster::new(Arc::clone(&store), 0.50);
        let forecast = forecaster
            .forecast("test-agent", Some("coding"), "claude-sonnet-4-5-20250929")
            .await
            .unwrap();

        assert_eq!(forecast.estimated_cost_usd, 0.25);
        assert_eq!(forecast.estimated_rounds, 3);
        assert_eq!(forecast.estimated_input_tokens, 5000);
        assert_eq!(forecast.estimated_output_tokens, 1000);
        assert!(!forecast.warn); // 0.25 < 0.50 threshold
        assert!(forecast.confidence > 0.6);
    }

    #[tokio::test]
    async fn test_forecast_fallback_to_default() {
        let store: Arc<dyn TelemetryStore> = Arc::new(SqliteStore::in_memory().unwrap());
        let forecaster = CostForecaster::new(store, 0.50);

        let forecast = forecaster
            .forecast("new-agent", Some("coding"), "claude-opus-4-20250514")
            .await
            .unwrap();

        // Should use system default
        assert_eq!(forecast.estimated_cost_usd, 0.12);
        assert_eq!(forecast.confidence, 0.3);
        assert!(forecast.basis.contains("system default"));
    }

    #[test]
    fn recurring_projection_scales_per_run() {
        let f = CostForecast {
            estimated_cost_usd: 1.5,
            ..CostForecast::default_estimate()
        };
        let r = f.recurring(288.0);
        assert_eq!(r.per_run_usd, 1.5);
        assert_eq!(r.per_day_usd, 432.0);
        assert_eq!(r.per_month_usd, 12960.0);
    }

    #[tokio::test]
    async fn warn_threshold_is_config_driven() {
        let store: Arc<dyn TelemetryStore> = Arc::new(SqliteStore::in_memory().unwrap());
        let task = TaskTelemetryRecord {
            agent_id: "a".to_string(),
            model: "m".to_string(),
            provider: "anthropic".to_string(),
            input_tokens: 100,
            output_tokens: 10,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
            cost_usd: 0.25,
            rounds: 3,
            duration_ms: 100,
            tool_invocation_count: 0,
            success: true,
            error_type: None,
            retry_count: 0,
            task_type: Some("t".to_string()),
            task_id: None,
            started_at: Utc::now().to_rfc3339(),
            completed_at: Utc::now().to_rfc3339(),
            forecast_cost_usd: None,
            forecast_rounds: None,
            forecast_confidence: None,
            team_id: None,
            session_id: None,
            model_switches: None,
            ticket_id: None,
            identity_id: None,
            description: None,
        };
        store.record_task_telemetry(&task, &[], &[]).unwrap();

        let strict = CostForecaster::new(Arc::clone(&store), 0.10);
        assert!(strict.forecast("a", Some("t"), "m").await.unwrap().warn);
        let lax = CostForecaster::new(store, 5.0);
        assert!(!lax.forecast("a", Some("t"), "m").await.unwrap().warn);
    }
}
