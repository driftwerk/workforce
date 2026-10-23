//! `system.logs` tool — lets authorized agents read workforce hub logs.
//!
//! Reads from the in-memory ring buffer populated by the tracing `BufferLayer`.
//! Access is controlled by the policy engine — restrict via `tool: "system.logs"`.

use std::time::Instant;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use eyre::Result;
use serde::Deserialize;
use workforce_agent::tools::{Tool, ToolResult};
use workforce_common::logging::LOG_BUFFER;

/// Tool that exposes hub logs to agents.
pub struct SystemLogsTool;

#[derive(Deserialize)]
struct LogsInput {
    action: String,
    /// Number of log lines to return (for `read`). Default 50, max 500.
    #[serde(default)]
    lines: Option<usize>,
    /// Filter by log level: trace, debug, info, warn, error (for `search`).
    #[serde(default)]
    level: Option<String>,
    /// Filter by target module path substring (for `search`).
    #[serde(default)]
    target: Option<String>,
    /// Filter by keyword substring in message (for `search`).
    #[serde(default)]
    keyword: Option<String>,
    /// Only return entries after this ISO 8601 timestamp (for `search`).
    #[serde(default)]
    since: Option<String>,
    /// Max results for `search`. Default 100, max 500.
    #[serde(default)]
    limit: Option<usize>,
}

#[async_trait]
impl Tool for SystemLogsTool {
    fn name(&self) -> &str {
        "system.logs"
    }

    fn description(&self) -> &str {
        "Read workforce hub logs from the in-memory buffer"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["read", "search"],
                    "description": "Action to perform"
                },
                "lines": {
                    "type": "integer",
                    "description": "Number of recent log lines to return (for 'read'"
                },
                "level": {
                    "type": "string",
                    "enum": ["trace", "debug", "info", "warn", "error"],
                    "description": "Filter by log level (for 'search')"
                },
                "target": {
                    "type": "string",
                    "description": "Filter by module path substring"
                },
                "keyword": {
                    "type": "string",
                    "description": "Filter by keyword in log message (for 'search')"
                },
                "since": {
                    "type": "string",
                    "description": "ISO 8601 timestamp"
                },
                "limit": {
                    "type": "integer",
                    "description": "Max results to return (for 'search', default 100, max 500)"
                }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();
        let input: LogsInput =
            serde_json::from_value(input).map_err(|e| eyre::eyre!("Invalid logs input: {}", e))?;

        let buffer = match LOG_BUFFER.get() {
            Some(b) => b,
            None => {
                return Ok(ToolResult::failure(
                    "Log buffer not initialised",
                    start.elapsed().as_millis() as u64,
                ))
            }
        };

        match input.action.as_str() {
            "read" => {
                let n = input.lines.unwrap_or(50).min(500);
                let entries = buffer.read_last(n);
                let lines: Vec<serde_json::Value> = entries
                    .iter()
                    .map(|e| {
                        serde_json::json!({
                            "timestamp": e.timestamp.to_rfc3339(),
                            "level": e.level,
                            "target": e.target,
                            "message": e.message,
                        })
                    })
                    .collect();

                Ok(ToolResult::success(
                    serde_json::json!({ "entries": lines, "count": lines.len() }),
                    start.elapsed().as_millis() as u64,
                ))
            }
            "search" => {
                let limit = input.limit.unwrap_or(100).min(500);

                let since: Option<DateTime<Utc>> = if let Some(ref s) = input.since {
                    match s.parse::<DateTime<Utc>>() {
                        Ok(ts) => Some(ts),
                        Err(e) => {
                            return Ok(ToolResult::failure(
                                format!("Invalid 'since' timestamp: {}", e),
                                start.elapsed().as_millis() as u64,
                            ))
                        }
                    }
                } else {
                    None
                };

                let entries = buffer.search(
                    input.level.as_deref(),
                    input.target.as_deref(),
                    input.keyword.as_deref(),
                    since,
                    limit,
                );

                let lines: Vec<serde_json::Value> = entries
                    .iter()
                    .map(|e| {
                        serde_json::json!({
                            "timestamp": e.timestamp.to_rfc3339(),
                            "level": e.level,
                            "target": e.target,
                            "message": e.message,
                        })
                    })
                    .collect();

                Ok(ToolResult::success(
                    serde_json::json!({ "entries": lines, "count": lines.len() }),
                    start.elapsed().as_millis() as u64,
                ))
            }
            other => Ok(ToolResult::failure(
                format!("Unknown action '{}'. Use: read, search", other),
                start.elapsed().as_millis() as u64,
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_name_and_schema() {
        let tool = SystemLogsTool;
        assert_eq!(tool.name(), "system.logs");
        let schema = tool.input_schema();
        assert!(schema["properties"]["action"].is_object());
        assert!(schema["properties"]["lines"].is_object());
        assert!(schema["properties"]["level"].is_object());
    }
}
