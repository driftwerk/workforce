use async_trait::async_trait;
use chrono::Utc;
use eyre::Result;
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;
use workforce_agent::tools::{Tool, ToolResult};
use workforce_common::activity_log::{parse_absolute_time, parse_relative_time, ActivityQuery};
use workforce_common::telemetry::TelemetryStore;

pub struct ActivityTool {
    store: Arc<dyn TelemetryStore>,
    /// The owning agent — the DEFAULT scope for queries. Without this the
    /// tool returned every agent's activity unless the model volunteered an
    /// id, which read as cross-agent contamination on multi-agent installs.
    agent_id: String,
}

#[derive(Debug, Deserialize)]
struct ActivityQueryInput {
    agent_id: Option<String>,
    last: Option<String>,
    from: Option<String>,
    to: Option<String>,
    verbose: Option<bool>,
}

impl ActivityTool {
    pub fn new(store: Arc<dyn TelemetryStore>, agent_id: impl Into<String>) -> Self {
        Self {
            store,
            agent_id: agent_id.into(),
        }
    }
}

/// Resolve the agent filter: own activity by default, an explicit id for
/// another agent, or `all`/`*` for the whole fleet.
fn resolve_agent_filter(own: &str, requested: Option<&str>) -> Option<String> {
    match requested {
        None => Some(own.to_string()),
        Some("all") | Some("*") => None,
        Some(other) => Some(other.to_string()),
    }
}

#[async_trait]
impl Tool for ActivityTool {
    fn name(&self) -> &str {
        "activity_history"
    }

    fn description(&self) -> &str {
        "Query activity history and metrics. Defaults to YOUR OWN activity; \
         pass agent_id to inspect another agent, or 'all' for every agent."
    }

    fn input_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "agent_id": {
                    "type": "string",
                    "description": "Agent to inspect. Omit for your own activity; 'all' for every agent."
                },
                "last": {
                    "type": "string",
                    "description": "Show activity from last duration (e.g. '1h', '30m', '7d')"
                },
                "from": {
                    "type": "string",
                    "description": "Start time (ISO 8601 format)"
                },
                "to": {
                    "type": "string",
                    "description": "End time (ISO 8601 format)"
                },
                "verbose": {
                    "type": "boolean",
                    "description": "Include per-round LLM details (default: false)"
                }
            },
            "required": []
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();
        let query_input: ActivityQueryInput = serde_json::from_value(input)?;

        let (from_time, to_time) = if let Some(last_str) = &query_input.last {
            (Some(parse_relative_time(last_str)?), Some(Utc::now()))
        } else if let (Some(from_str), Some(to_str)) = (&query_input.from, &query_input.to) {
            (
                Some(parse_absolute_time(from_str)?),
                Some(parse_absolute_time(to_str)?),
            )
        } else {
            // Default: last 1 hour
            (
                Some(Utc::now() - chrono::Duration::hours(1)),
                Some(Utc::now()),
            )
        };

        let agent_filter = resolve_agent_filter(&self.agent_id, query_input.agent_id.as_deref());
        let query = ActivityQuery {
            agent_id: agent_filter.clone(),
            start_time: from_time,
            end_time: to_time,
            verbose: query_input.verbose.unwrap_or(false),
        };

        let activities = self.store.query_activities(&query)?;
        let count = activities.len();

        let activities_json: Vec<serde_json::Value> = activities
            .into_iter()
            .map(|a| {
                json!({
                    "timestamp": a.timestamp.to_rfc3339(),
                    "agent_id": a.agent_id,
                    "activity_type": a.activity_type,
                    "channel": a.channel,
                    "description": a.description,
                    "duration_ms": a.duration_ms,
                    "success": a.success,
                    "metadata": a.metadata
                })
            })
            .collect();

        Ok(ToolResult::success(
            json!({
                "agent_scope": agent_filter.unwrap_or_else(|| "all".to_string()),
                "activities": activities_json,
                "count": count,
                "note": "tool/round entries are timestamped at their task's start (per-row execution times are not recorded); round numbers give the in-task order",
            }),
            start.elapsed().as_millis() as u64,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_filter_defaults_to_own_activity() {
        assert_eq!(
            resolve_agent_filter("quill", None),
            Some("quill".to_string())
        );
        assert_eq!(
            resolve_agent_filter("quill", Some("atlas")),
            Some("atlas".to_string())
        );
        assert_eq!(resolve_agent_filter("quill", Some("all")), None);
        assert_eq!(resolve_agent_filter("quill", Some("*")), None);
    }
}
