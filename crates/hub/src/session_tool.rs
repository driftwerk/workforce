//! Session label tool — let agents set human-readable labels on sessions.
//!
//! Tool: `session.set_label`

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use eyre::Result;
use serde::Deserialize;
use workforce_agent::tools::{Tool, ToolResult};
use workforce_common::session::SessionStore;

/// Tool that lets agents set a human-readable label on a session.
pub struct SessionLabelTool {
    sessions: Arc<dyn SessionStore>,
}

impl SessionLabelTool {
    pub fn new(sessions: Arc<dyn SessionStore>) -> Self {
        Self { sessions }
    }

    /// Resolve the session ID: prefer explicit input, fall back to task-local.
    fn resolve_session_id(&self, explicit: Option<&str>) -> Option<String> {
        if let Some(sid) = explicit {
            if !sid.is_empty() {
                return Some(sid.to_string());
            }
        }
        workforce_agent::runner::CURRENT_SESSION_ID
            .try_with(|s| s.clone())
            .ok()
    }
}

#[derive(Deserialize)]
struct SetLabelInput {
    label: String,
    #[serde(default)]
    session_id: Option<String>,
}

#[async_trait]
impl Tool for SessionLabelTool {
    fn name(&self) -> &str {
        "session.set_label"
    }

    fn description(&self) -> &str {
        "Set a human-readable label on a session for easy identification"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "label": {
                    "type": "string",
                    "description": "Human-readable session label"
                },
                "session_id": {
                    "type": "string",
                    "description": "Session ID to label (default: current session)"
                }
            },
            "required": ["label"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();
        let params: SetLabelInput =
            serde_json::from_value(input).map_err(|e| eyre::eyre!("Invalid input: {}", e))?;

        let session_id = match self.resolve_session_id(params.session_id.as_deref()) {
            Some(id) => id,
            None => {
                return Ok(ToolResult::failure(
                    "No session_id provided and no current session".to_string(),
                    start.elapsed().as_millis() as u64,
                ));
            }
        };

        match self.sessions.get(&session_id)? {
            Some(mut session) => {
                // Ownership guard: don't let one user relabel another's session
                // (an explicit `session_id` can address any session). Enforce
                // only when an identity is in scope and the session has a
                // different non-empty owner; system/CLI (no identity) and
                // ownerless sessions pass. Returns the same "not found" as a
                // missing session so ids don't leak existence.
                if let Some(identity) = crate::user_config::current_identity_id() {
                    if !session.owner.is_empty() && session.owner != identity {
                        return Ok(ToolResult::failure(
                            format!("Session not found: {}", session_id),
                            start.elapsed().as_millis() as u64,
                        ));
                    }
                }
                session.label = Some(params.label.clone());
                self.sessions.save(&session)?;
                Ok(ToolResult::success(
                    serde_json::json!({
                        "session_id": session_id,
                        "label": params.label,
                        "status": "applied"
                    }),
                    start.elapsed().as_millis() as u64,
                ))
            }
            None => Ok(ToolResult::failure(
                format!("Session not found: {}", session_id),
                start.elapsed().as_millis() as u64,
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use workforce_common::session::InMemorySessionStore;

    #[tokio::test]
    async fn set_label_with_explicit_session_id() {
        let store = Arc::new(InMemorySessionStore::new());
        // Create a session first
        let session = store
            .get_or_create("test:channel", "user1", "agent1")
            .unwrap();
        let session_id = session.id.clone();

        let tool = SessionLabelTool::new(Arc::clone(&store) as Arc<dyn SessionStore>);
        let result = tool
            .execute(serde_json::json!({
                "label": "Linear triage",
                "session_id": session_id
            }))
            .await
            .unwrap();

        assert!(result.success);
        assert_eq!(result.output["label"], "Linear triage");
        assert_eq!(result.output["status"], "applied");

        // Verify persistence
        let updated = store.get(&session_id).unwrap().unwrap();
        assert_eq!(updated.label.as_deref(), Some("Linear triage"));
    }

    #[tokio::test]
    async fn set_label_session_not_found() {
        let store = Arc::new(InMemorySessionStore::new());
        let tool = SessionLabelTool::new(store);

        let result = tool
            .execute(serde_json::json!({
                "label": "test label",
                "session_id": "nonexistent-id"
            }))
            .await
            .unwrap();

        assert!(!result.success);
    }

    #[tokio::test]
    async fn set_label_no_session_id_and_no_task_local() {
        let store = Arc::new(InMemorySessionStore::new());
        let tool = SessionLabelTool::new(store);

        let result = tool
            .execute(serde_json::json!({
                "label": "test label"
            }))
            .await
            .unwrap();

        assert!(!result.success);
    }

    #[tokio::test]
    async fn set_label_with_task_local_session_id() {
        use workforce_agent::runner::CURRENT_SESSION_ID;

        let store = Arc::new(InMemorySessionStore::new());
        let session = store
            .get_or_create("test:channel", "user1", "agent1")
            .unwrap();
        let session_id = session.id.clone();

        let tool = SessionLabelTool::new(Arc::clone(&store) as Arc<dyn SessionStore>);

        let result = CURRENT_SESSION_ID
            .scope(session_id.clone(), async {
                tool.execute(serde_json::json!({
                    "label": "stats testing"
                }))
                .await
            })
            .await
            .unwrap();

        assert!(result.success);
        assert_eq!(result.output["label"], "stats testing");

        let updated = store.get(&session_id).unwrap().unwrap();
        assert_eq!(updated.label.as_deref(), Some("stats testing"));
    }

    /// A user must not relabel another user's session via an explicit
    /// `session_id`. The denial looks like a missing session (no existence
    /// leak), and the owner can still relabel their own.
    #[tokio::test]
    async fn set_label_rejects_another_users_session() {
        use workforce_agent::runner::CURRENT_USER_ID;

        let store = Arc::new(InMemorySessionStore::new());
        let session = store
            .get_or_create("test:channel", "owner-user", "agent1")
            .unwrap();
        let session_id = session.id.clone();
        let tool = SessionLabelTool::new(Arc::clone(&store) as Arc<dyn SessionStore>);

        let denied = CURRENT_USER_ID
            .scope("attacker".to_string(), async {
                tool.execute(serde_json::json!({
                    "label": "hijacked",
                    "session_id": session_id
                }))
                .await
            })
            .await
            .unwrap();
        assert!(!denied.success, "another user must not relabel the session");
        let after = store.get(&session_id).unwrap().unwrap();
        assert_ne!(after.label.as_deref(), Some("hijacked"));

        let owned = CURRENT_USER_ID
            .scope("owner-user".to_string(), async {
                tool.execute(serde_json::json!({
                    "label": "mine",
                    "session_id": session_id
                }))
                .await
            })
            .await
            .unwrap();
        assert!(owned.success, "the owner can relabel their own session");
    }
}
