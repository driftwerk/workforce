//! Cost analytics framework (DW-335, DW-340).
//!
//! Pure computation module for analysing API-equivalent usage costs.
//! Provides period-based cost breakdowns by model/agent and linear
//! regression forecasting.

use chrono::{DateTime, Datelike, NaiveDate, Utc};
use eyre::eyre;
use rusqlite::params;
use serde::Serialize;
use std::sync::Arc;

use crate::persistence::{DailyUsageRow, SqliteStore, TaskUsageRow};
use crate::telemetry::TelemetryStore;

// ─── Types ───────────────────────────────────────────────────────────────────

/// A time period for cost analysis.
#[derive(Debug, Clone, Serialize)]
pub struct AnalysisPeriod {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ModelUsageBreakdown {
    pub model: String,
    pub messages: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub api_cost: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct AgentUsageBreakdown {
    pub agent_id: String,
    pub messages: u64,
    pub api_cost: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProviderUsageBreakdown {
    pub provider: String,
    pub messages: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub api_cost: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct UsageAnalysis {
    pub period: AnalysisPeriod,
    pub total_messages: u64,
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub api_equivalent_cost: f64,
    pub daily_average_messages: f64,
    pub peak_day_messages: u64,
    pub by_model: Vec<ModelUsageBreakdown>,
    pub by_agent: Vec<AgentUsageBreakdown>,
    pub by_provider: Vec<ProviderUsageBreakdown>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DailyProjection {
    pub date: String,
    pub projected_messages: u64,
    pub projected_api_cost: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct UsageForecast {
    pub projected_messages: u64,
    pub projected_total_api_cost: f64,
    pub utilization_trend: String,
    pub daily_projections: Vec<DailyProjection>,
    pub confidence: f64,
}

// ─── Core engine ─────────────────────────────────────────────────────────────

pub struct CostAnalytics {
    store: Arc<dyn TelemetryStore>,
}

impl CostAnalytics {
    pub fn new(store: Arc<dyn TelemetryStore>) -> Self {
        Self { store }
    }

    /// Get the analysis period for a calendar month offset (0 = current, -1 = previous, etc).
    pub fn calendar_month(offset: i32) -> AnalysisPeriod {
        let now = Utc::now();
        let base_months = now.year() * 12 + now.month() as i32 - 1 + offset;
        let start_year = base_months.div_euclid(12);
        let start_month = (base_months.rem_euclid(12) + 1) as u32;

        let end_months = base_months + 1;
        let end_year = end_months.div_euclid(12);
        let end_month = (end_months.rem_euclid(12) + 1) as u32;

        let start = NaiveDate::from_ymd_opt(start_year, start_month, 1)
            .expect("valid date")
            .and_hms_opt(0, 0, 0)
            .expect("valid time")
            .and_utc();
        let end = NaiveDate::from_ymd_opt(end_year, end_month, 1)
            .expect("valid date")
            .and_hms_opt(0, 0, 0)
            .expect("valid time")
            .and_utc();

        AnalysisPeriod { start, end }
    }

    /// Get an analysis period for the last N days.
    pub fn rolling_days(n: u32) -> AnalysisPeriod {
        let now = Utc::now();
        let start = now - chrono::Duration::days(n as i64);
        AnalysisPeriod { start, end: now }
    }

    /// Analyse usage for a period by fetching `task_telemetry` rows
    /// from the backend and aggregating them in pure Rust.
    pub fn analyze_usage(&self, period: &AnalysisPeriod) -> eyre::Result<UsageAnalysis> {
        let rows = self.store.task_usage_rows(period.start, period.end)?;

        let mut total_messages: u64 = 0;
        let mut total_input_tokens: u64 = 0;
        let mut total_output_tokens: u64 = 0;
        let mut api_equivalent_cost: f64 = 0.0;

        let mut model_map: std::collections::HashMap<String, (u64, u64, u64, f64)> =
            std::collections::HashMap::new();
        let mut agent_map: std::collections::HashMap<String, (u64, f64)> =
            std::collections::HashMap::new();
        let mut provider_map: std::collections::HashMap<String, (u64, u64, u64, f64)> =
            std::collections::HashMap::new();
        let mut day_map: std::collections::HashMap<String, u64> = std::collections::HashMap::new();

        for row in &rows {
            let msgs = row.rounds as u64;
            total_messages += msgs;
            total_input_tokens += row.input_tokens;
            total_output_tokens += row.output_tokens;
            api_equivalent_cost += row.cost_usd;

            {
                let entry = model_map.entry(row.model.clone()).or_default();
                entry.0 += msgs;
                entry.1 += row.input_tokens;
                entry.2 += row.output_tokens;
                entry.3 += row.cost_usd;
            }
            {
                let entry = agent_map.entry(row.agent_id.clone()).or_default();
                entry.0 += msgs;
                entry.1 += row.cost_usd;
            }
            {
                let entry = provider_map.entry(row.provider.clone()).or_default();
                entry.0 += msgs;
                entry.1 += row.input_tokens;
                entry.2 += row.output_tokens;
                entry.3 += row.cost_usd;
            }
            {
                *day_map.entry(row.day.clone()).or_default() += msgs;
            }
        }

        let by_model: Vec<ModelUsageBreakdown> = model_map
            .into_iter()
            .map(
                |(model, (messages, input_tokens, output_tokens, api_cost))| ModelUsageBreakdown {
                    model,
                    messages,
                    input_tokens,
                    output_tokens,
                    api_cost,
                },
            )
            .collect();

        let by_agent: Vec<AgentUsageBreakdown> = agent_map
            .into_iter()
            .map(|(agent_id, (messages, api_cost))| AgentUsageBreakdown {
                agent_id,
                messages,
                api_cost,
            })
            .collect();

        let by_provider: Vec<ProviderUsageBreakdown> = provider_map
            .into_iter()
            .map(
                |(provider, (messages, input_tokens, output_tokens, api_cost))| {
                    ProviderUsageBreakdown {
                        provider,
                        messages,
                        input_tokens,
                        output_tokens,
                        api_cost,
                    }
                },
            )
            .collect();

        let peak_day_messages = day_map.values().copied().max().unwrap_or(0);

        let days_in_period = {
            let d = (period.end - period.start).num_days().max(1);
            // Cap to elapsed days if period extends into the future.
            let now = Utc::now();
            if now < period.end {
                (now - period.start).num_days().max(1) as f64
            } else {
                d as f64
            }
        };
        let daily_average_messages = total_messages as f64 / days_in_period;

        Ok(UsageAnalysis {
            period: period.clone(),
            total_messages,
            total_input_tokens,
            total_output_tokens,
            api_equivalent_cost,
            daily_average_messages,
            peak_day_messages,
            by_model,
            by_agent,
            by_provider,
        })
    }

    /// Forecast future usage based on recent trends. Pulls daily
    /// aggregates from the backend for the last 14 days and runs a
    /// linear regression in pure Rust.
    pub fn forecast_usage(&self, days_ahead: u32) -> eyre::Result<UsageForecast> {
        let now = Utc::now();
        let lookback = now - chrono::Duration::days(14);
        let daily_data = self.store.daily_usage_rows(lookback)?;

        if daily_data.is_empty() {
            return Ok(UsageForecast {
                projected_messages: 0,
                projected_total_api_cost: 0.0,
                utilization_trend: "no_data".to_string(),
                daily_projections: vec![],
                confidence: 0.0,
            });
        }

        let n = daily_data.len() as f64;
        let msgs: Vec<f64> = daily_data
            .iter()
            .map(|row| row.total_rounds as f64)
            .collect();
        let costs: Vec<f64> = daily_data.iter().map(|row| row.total_cost).collect();

        let (slope_msgs, intercept_msgs) = linear_regression(&msgs);
        let (slope_cost, intercept_cost) = linear_regression(&costs);

        let utilization_trend = if slope_msgs > 0.5 {
            "increasing".to_string()
        } else if slope_msgs < -0.5 {
            "decreasing".to_string()
        } else {
            "stable".to_string()
        };

        let mut projected_messages: u64 = 0;
        let mut projected_total_api_cost: f64 = 0.0;
        let mut daily_projections = Vec::new();

        for d in 0..days_ahead {
            let x = n + d as f64;
            let proj_msgs = (intercept_msgs + slope_msgs * x).max(0.0);
            let proj_cost = (intercept_cost + slope_cost * x).max(0.0);

            projected_messages += proj_msgs.round() as u64;
            projected_total_api_cost += proj_cost;

            let date = (now + chrono::Duration::days(d as i64 + 1))
                .format("%Y-%m-%d")
                .to_string();
            daily_projections.push(DailyProjection {
                date,
                projected_messages: proj_msgs.round() as u64,
                projected_api_cost: (proj_cost * 100.0).round() / 100.0,
            });
        }

        let confidence = match daily_data.len() {
            0..=3 => 0.3,
            4..=7 => 0.5,
            8..=12 => 0.7,
            _ => 0.85,
        };

        Ok(UsageForecast {
            projected_messages,
            projected_total_api_cost: (projected_total_api_cost * 100.0).round() / 100.0,
            utilization_trend,
            daily_projections,
            confidence,
        })
    }
}

// ─── Inherent SqliteStore methods backing the trait ─────────────────────────
//
// The actual SQL lives here next to the consumers (`CostAnalytics`) so the
// query shape and the aggregation stay close to each other. They are also
// surfaced through `TelemetryStore` so a future Postgres backend can be
// dropped in without touching `CostAnalytics`.

impl SqliteStore {
    /// Fetch flattened `task_telemetry` rows for a closed-open period
    /// `[from, to)`. Used by [`CostAnalytics::analyze_usage`].
    pub fn task_usage_rows(
        &self,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> eyre::Result<Vec<TaskUsageRow>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| eyre!("Lock poisoned: {}", e))?;
        let mut stmt = conn.prepare(
            "SELECT agent_id, model, input_tokens, output_tokens, cost_usd, rounds,
                    DATE(completed_at) as day, COALESCE(provider, 'unknown') as provider
             FROM task_telemetry
             WHERE completed_at >= ?1 AND completed_at < ?2
             ORDER BY completed_at ASC",
        )?;
        let rows = stmt
            .query_map(params![from.to_rfc3339(), to.to_rfc3339()], |row| {
                Ok(TaskUsageRow {
                    agent_id: row.get(0)?,
                    model: row.get(1)?,
                    input_tokens: row.get::<_, i64>(2)? as u64,
                    output_tokens: row.get::<_, i64>(3)? as u64,
                    cost_usd: row.get(4)?,
                    rounds: row.get::<_, i64>(5)? as u32,
                    day: row.get(6)?,
                    provider: row.get(7)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Daily aggregates of `task_telemetry` activity since `since`,
    /// grouped by day. Used by [`CostAnalytics::forecast_usage`].
    pub fn daily_usage_rows(&self, since: DateTime<Utc>) -> eyre::Result<Vec<DailyUsageRow>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| eyre!("Lock poisoned: {}", e))?;
        let mut stmt = conn.prepare(
            "SELECT DATE(completed_at) as day, COUNT(*) as task_count,
                    SUM(cost_usd) as total_cost, SUM(rounds) as total_rounds
             FROM task_telemetry
             WHERE completed_at >= ?1
             GROUP BY DATE(completed_at)
             ORDER BY day ASC",
        )?;
        let rows = stmt
            .query_map(params![since.to_rfc3339()], |row| {
                Ok(DailyUsageRow {
                    day: row.get(0)?,
                    task_count: row.get::<_, i64>(1)? as u64,
                    total_cost: row.get(2)?,
                    total_rounds: row.get::<_, i64>(3)? as u64,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

/// Simple linear regression: returns (slope, intercept) for y = intercept + slope * x.
fn linear_regression(ys: &[f64]) -> (f64, f64) {
    let n = ys.len() as f64;
    if n < 2.0 {
        let mean = ys.first().copied().unwrap_or(0.0);
        return (0.0, mean);
    }
    let sum_x: f64 = (0..ys.len()).map(|i| i as f64).sum();
    let sum_y: f64 = ys.iter().sum();
    let sum_xy: f64 = ys.iter().enumerate().map(|(i, y)| i as f64 * y).sum();
    let sum_xx: f64 = (0..ys.len()).map(|i| (i as f64) * (i as f64)).sum();

    let denom = n * sum_xx - sum_x * sum_x;
    if denom.abs() < 1e-12 {
        return (0.0, sum_y / n);
    }
    let slope = (n * sum_xy - sum_x * sum_y) / denom;
    let intercept = (sum_y - slope * sum_x) / n;
    (slope, intercept)
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persistence::{SqliteStore, TaskTelemetryRecord};

    fn make_analytics(store: Arc<dyn TelemetryStore>) -> CostAnalytics {
        CostAnalytics::new(store)
    }

    fn insert_test_task(
        store: &SqliteStore,
        agent_id: &str,
        model: &str,
        cost: f64,
        rounds: u32,
        completed_at: &str,
    ) {
        let task = TaskTelemetryRecord {
            agent_id: agent_id.to_string(),
            model: model.to_string(),
            provider: "anthropic".to_string(),
            input_tokens: 5000,
            output_tokens: 1000,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
            cost_usd: cost,
            rounds,
            duration_ms: 5000,
            tool_invocation_count: 2,
            success: true,
            error_type: None,
            retry_count: 0,
            task_type: Some("coding".to_string()),
            task_id: Some(format!("task-{}", uuid::Uuid::new_v4())),
            started_at: completed_at.to_string(),
            completed_at: completed_at.to_string(),
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

    #[test]
    fn calendar_month_current() {
        let period = CostAnalytics::calendar_month(0);
        let now = Utc::now();
        assert_eq!(period.start.day(), 1);
        assert_eq!(period.start.month(), now.month());
        assert!(period.end > period.start);
    }

    #[test]
    fn calendar_month_previous() {
        let current = CostAnalytics::calendar_month(0);
        let prev = CostAnalytics::calendar_month(-1);
        assert_eq!(prev.end, current.start);
    }

    #[test]
    fn rolling_days_period() {
        let period = CostAnalytics::rolling_days(30);
        let duration = period.end - period.start;
        assert!((duration.num_days() - 30).abs() <= 1);
    }

    #[test]
    fn analyze_usage_empty() {
        let store = Arc::new(SqliteStore::in_memory().unwrap());
        let analytics = make_analytics(store);

        let period = CostAnalytics::calendar_month(0);
        let analysis = analytics.analyze_usage(&period).unwrap();

        assert_eq!(analysis.total_messages, 0);
        assert_eq!(analysis.total_input_tokens, 0);
        assert_eq!(analysis.total_output_tokens, 0);
        assert_eq!(analysis.api_equivalent_cost, 0.0);
        assert_eq!(analysis.peak_day_messages, 0);
        assert!(analysis.by_model.is_empty());
        assert!(analysis.by_agent.is_empty());
    }

    #[test]
    fn analyze_usage_with_data() {
        let store = Arc::new(SqliteStore::in_memory().unwrap());

        let now = Utc::now();
        let day_str = now.format("%Y-%m-%dT12:00:00+00:00").to_string();

        for _ in 0..10 {
            insert_test_task(&store, "agent-1", "claude-opus-4", 5.0, 3, &day_str);
        }

        let analytics = make_analytics(store);
        let period = CostAnalytics::calendar_month(0);
        let analysis = analytics.analyze_usage(&period).unwrap();

        assert_eq!(analysis.total_messages, 30); // 10 tasks * 3 rounds
        assert_eq!(analysis.api_equivalent_cost, 50.0); // 10 * $5
        assert_eq!(analysis.by_model.len(), 1);
        assert_eq!(analysis.by_agent.len(), 1);
    }

    #[test]
    fn forecast_usage_empty() {
        let store = Arc::new(SqliteStore::in_memory().unwrap());
        let analytics = make_analytics(store);
        let forecast = analytics.forecast_usage(7).unwrap();

        assert_eq!(forecast.projected_messages, 0);
        assert_eq!(forecast.confidence, 0.0);
        assert_eq!(forecast.utilization_trend, "no_data");
    }

    #[test]
    fn linear_regression_constant() {
        let (slope, intercept) = linear_regression(&[10.0, 10.0, 10.0, 10.0]);
        assert!(slope.abs() < 1e-6);
        assert!((intercept - 10.0).abs() < 1e-6);
    }

    #[test]
    fn linear_regression_ascending() {
        let (slope, intercept) = linear_regression(&[0.0, 1.0, 2.0, 3.0]);
        assert!((slope - 1.0).abs() < 1e-6);
        assert!(intercept.abs() < 1e-6);
    }
}
