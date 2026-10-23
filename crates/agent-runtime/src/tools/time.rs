//! Wall-clock tools (`time.*`) — an essential built-in family, registered in
//! the core toolset.
//!
//! `time.sleep` exists so an agent can run a *bounded* polling loop — wait for
//! a deployment to converge, a job to report, a PR check to finish — without
//! burning LLM rounds on busy-waiting or shelling out to `sleep`. Each call is
//! capped at [`MAX_SLEEP_SECS`] so a single call can never outlast the
//! runner's check-in interval; longer waits are the loop's job.

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use eyre::{eyre, Result};
use serde_json::{json, Value};

use crate::tools::{Tool, ToolResult};

/// Every `time.*` tool name, in registration order.
pub const TOOL_NAMES: &[&str] = &["time.sleep"];

/// Shortest and longest sleep a single call may perform.
pub const MIN_SLEEP_SECS: u64 = 1;
pub const MAX_SLEEP_SECS: u64 = 120;

/// Above [`MAX_SLEEP_SECS`] so a clamped sleep always returns on its own, and
/// below `runner::CHECK_IN_INTERVAL` as [`Tool::soft_deadline`] requires.
const SOFT_DEADLINE: Duration = Duration::from_secs(135);

/// The wait a call performs for the `seconds` it named: clamped into
/// `[MIN_SLEEP_SECS, MAX_SLEEP_SECS]`, with fractional values rounded to the
/// nearest whole second.
fn clamp_seconds(requested: f64) -> u64 {
    (requested.round() as u64).clamp(MIN_SLEEP_SECS, MAX_SLEEP_SECS)
}

/// The `seconds` argument as a number, or the error the model is shown.
fn requested_seconds(input: &Value) -> Result<f64> {
    match input.get("seconds") {
        Some(Value::Number(n)) => n
            .as_f64()
            .filter(|s| s.is_finite())
            .ok_or_else(|| eyre!("`seconds` must be a finite number")),
        Some(_) => Err(eyre!("`seconds` must be a number")),
        None => Err(eyre!("`seconds` is required")),
    }
}

pub struct SleepTool;

#[async_trait]
impl Tool for SleepTool {
    fn name(&self) -> &str {
        "time.sleep"
    }

    fn description(&self) -> &str {
        "Pause for `seconds` before continuing. Use it for bounded polling loops: wait, \
         then re-check whether something has converged (a deployment rolled out, a job \
         finished, a check reported), and stop once it has or after a fixed number of \
         tries. Capped at 120 seconds per call — sleep again for a longer wait. Values \
         outside 1–120 are clamped to that range."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "seconds": {
                    "type": "number",
                    "minimum": MIN_SLEEP_SECS,
                    "maximum": MAX_SLEEP_SECS,
                    "description": "How long to pause, in seconds (1–120; clamped)"
                }
            },
            "required": ["seconds"]
        })
    }

    fn soft_deadline(&self) -> Option<Duration> {
        Some(SOFT_DEADLINE)
    }

    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let started = Instant::now();
        let seconds = match requested_seconds(&input) {
            Ok(requested) => clamp_seconds(requested),
            Err(e) => return Ok(ToolResult::from_report(&e, 0)),
        };
        tokio::time::sleep(Duration::from_secs(seconds)).await;
        Ok(ToolResult::success(
            json!({ "slept_seconds": seconds }),
            started.elapsed().as_millis() as u64,
        ))
    }
}

/// The `time.*` tools, in [`TOOL_NAMES`] order.
pub fn time_tools() -> Vec<Arc<dyn Tool>> {
    vec![Arc::new(SleepTool)]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::CHECK_IN_INTERVAL;

    #[test]
    fn clamp_seconds_bounds_both_ends_and_rounds() {
        assert_eq!(clamp_seconds(0.0), MIN_SLEEP_SECS);
        assert_eq!(clamp_seconds(-30.0), MIN_SLEEP_SECS);
        assert_eq!(clamp_seconds(0.4), MIN_SLEEP_SECS);
        assert_eq!(clamp_seconds(2.6), 3);
        assert_eq!(clamp_seconds(120.0), MAX_SLEEP_SECS);
        assert_eq!(clamp_seconds(121.0), MAX_SLEEP_SECS);
        assert_eq!(clamp_seconds(1e9), MAX_SLEEP_SECS);
    }

    #[test]
    fn requested_seconds_rejects_missing_and_non_numeric_input() {
        assert!(requested_seconds(&json!({})).is_err());
        assert!(requested_seconds(&json!({ "seconds": "5" })).is_err());
        assert!(requested_seconds(&json!({ "seconds": null })).is_err());
        assert!(requested_seconds(&json!({ "seconds": true })).is_err());
        assert_eq!(requested_seconds(&json!({ "seconds": 5 })).unwrap(), 5.0);
        assert_eq!(requested_seconds(&json!({ "seconds": 2.5 })).unwrap(), 2.5);
    }

    #[tokio::test]
    async fn execute_reports_bad_input_as_a_failed_call() {
        for input in [json!({}), json!({ "seconds": "soon" })] {
            let result = SleepTool.execute(input).await.unwrap();
            assert!(!result.success);
            assert!(result.error.as_deref().unwrap().contains("`seconds`"));
        }
    }

    #[tokio::test]
    async fn execute_sleeps_for_the_clamped_value() {
        let started = Instant::now();
        let result = SleepTool.execute(json!({ "seconds": 0.2 })).await.unwrap();
        assert!(result.success);
        assert_eq!(result.output["slept_seconds"], MIN_SLEEP_SECS);
        assert!(started.elapsed() >= Duration::from_secs(1));
    }

    #[tokio::test(start_paused = true)]
    async fn execute_clamps_an_oversized_request_to_the_cap() {
        let result = SleepTool.execute(json!({ "seconds": 3600 })).await.unwrap();
        assert!(result.success);
        assert_eq!(result.output["slept_seconds"], MAX_SLEEP_SECS);
    }

    #[test]
    fn soft_deadline_outlasts_the_cap_but_not_the_check_in() {
        let deadline = SleepTool.soft_deadline().unwrap();
        assert!(deadline > Duration::from_secs(MAX_SLEEP_SECS));
        assert!(deadline < CHECK_IN_INTERVAL);
    }

    #[test]
    fn registration_order_matches_tool_names() {
        let names: Vec<String> = time_tools().iter().map(|t| t.name().to_string()).collect();
        assert_eq!(names, TOOL_NAMES);
        assert!(SleepTool.description().contains("120 seconds per call"));
        assert_eq!(SleepTool.input_schema()["required"], json!(["seconds"]));
    }
}
