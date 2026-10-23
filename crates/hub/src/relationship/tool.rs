//! `relationship_status` — lets the agent surface the caller's own
//! working-relationship summary on demand (e.g. "how are we doing?"). Strictly
//! scoped to the requesting identity; refuses to run unscoped.

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use eyre::Result;
use serde::Deserialize;
use workforce_agent::tools::{Tool, ToolResult};
use workforce_common::telemetry::TelemetryStore;
use workforce_core::types::AgentId;

use crate::relationship::summary::summarise;

const DEFAULT_WINDOW_DAYS: i64 = 90;
const MAX_WINDOW_DAYS: i64 = 365;

pub struct RelationshipStatusTool {
    telemetry: Arc<dyn TelemetryStore>,
    agent_id: AgentId,
}

impl RelationshipStatusTool {
    pub fn new(telemetry: Arc<dyn TelemetryStore>, agent_id: AgentId) -> Self {
        Self {
            telemetry,
            agent_id,
        }
    }
}

#[derive(Deserialize)]
struct Input {
    #[serde(default)]
    days: Option<i64>,
}

#[async_trait]
impl Tool for RelationshipStatusTool {
    fn name(&self) -> &str {
        "relationship_status"
    }

    fn description(&self) -> &str {
        "Show this user their own working-relationship summary with you — overall warmth, recent \
         appreciative vs frustrated signals, time spent, and notable moments — built from your past \
         direct conversations. Use it when the user asks how things are going between you, or to \
         ground an acknowledgement of the dynamic. Returns only the requesting user's own data. \
         Optional `days` sets the window (default 90)."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "days": {
                    "type": "integer",
                    "description": "Look-back window in days (default 90, max 365)."
                }
            }
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();
        let elapsed = || start.elapsed().as_millis() as u64;

        let Some(identity_id) = crate::user_config::current_identity_id() else {
            return Ok(ToolResult::failure(
                "relationship_status requires a user identity in scope; refusing to run unscoped",
                elapsed(),
            ));
        };

        let input: Input = serde_json::from_value(input).unwrap_or(Input { days: None });
        let days = input
            .days
            .unwrap_or(DEFAULT_WINDOW_DAYS)
            .clamp(1, MAX_WINDOW_DAYS);

        let now = chrono::Utc::now();
        let events = match self.telemetry.relationship_events_in_range(
            Some(self.agent_id.as_str()),
            Some(&identity_id),
            now - chrono::Duration::days(days),
            now,
        ) {
            Ok(events) => events,
            Err(e) => {
                return Ok(ToolResult::failure(
                    format!("Failed to load relationship: {e}"),
                    elapsed(),
                ))
            }
        };

        let summary = summarise(&events, now, days);
        match serde_json::to_value(&summary) {
            Ok(value) => Ok(ToolResult::success(value, elapsed())),
            Err(e) => Ok(ToolResult::failure(
                format!("Failed to serialise summary: {e}"),
                elapsed(),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use workforce_agent::runner::CURRENT_USER_ID;
    use workforce_common::persistence::SqliteStore;

    #[tokio::test]
    async fn refuses_without_identity() {
        let store: Arc<dyn TelemetryStore> = Arc::new(SqliteStore::in_memory().unwrap());
        let tool = RelationshipStatusTool::new(store, AgentId::new("atlas"));
        let result = tool.execute(serde_json::json!({})).await.unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("requires a user identity"));
    }

    #[tokio::test]
    async fn returns_only_callers_own_summary() {
        let store = Arc::new(SqliteStore::in_memory().unwrap());
        let now = chrono::Utc::now();
        crate::relationship::capture_turn(
            store.as_ref(),
            "atlas",
            "alice",
            "s1",
            "m1",
            now,
            "perfect, thanks!",
        )
        .unwrap();
        crate::relationship::capture_turn(
            store.as_ref(),
            "atlas",
            "bob",
            "s2",
            "m2",
            now,
            "no, that's wrong",
        )
        .unwrap();

        let tool = RelationshipStatusTool::new(
            store.clone() as Arc<dyn TelemetryStore>,
            AgentId::new("atlas"),
        );
        CURRENT_USER_ID
            .scope("alice".to_string(), async {
                let result = tool.execute(serde_json::json!({})).await.unwrap();
                assert!(result.success);
                assert_eq!(result.output["interactions"], 1);
                assert_eq!(result.output["praise"], 1);
                assert_eq!(result.output["corrections"], 0, "must not see bob's data");
            })
            .await;
    }
}
