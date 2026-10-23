//! Linear webhook event handler.
//!
//! Parses raw Linear webhook payloads into typed actions.
//! Linear webhooks send JSON with fields: action, type, data, url, updatedFrom, createdAt.
//!
//! Decoupled from the agent service: the handler only decides *what* should
//! happen, not *how* to execute it.

use serde::{Deserialize, Serialize};

/// Actions derived from Linear webhook events.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum LinearAction {
    IssueCreated {
        id: String,
        identifier: String,
        title: String,
        team_name: String,
        creator_name: String,
        url: String,
    },
    IssueUpdated {
        id: String,
        identifier: String,
        title: String,
        changes: Vec<String>,
        url: String,
    },
    IssueStateChanged {
        id: String,
        identifier: String,
        title: String,
        from_state: String,
        to_state: String,
        url: String,
    },
    IssueAssigned {
        id: String,
        identifier: String,
        title: String,
        assignee_name: String,
        url: String,
    },
    IssueRemoved {
        id: String,
        identifier: String,
        title: String,
    },
    CommentCreated {
        id: String,
        issue_id: String,
        issue_identifier: String,
        author_name: String,
        body: String,
        url: String,
    },
    CommentUpdated {
        id: String,
        issue_id: String,
        body: String,
    },
    ProjectUpdated {
        id: String,
        name: String,
        state: String,
        url: String,
    },
    /// Unrecognized or filtered event.
    Ignore,
}

pub struct LinearEventHandler;

impl LinearEventHandler {
    /// Parse a Linear webhook payload into a typed action.
    ///
    /// Linear webhooks have this structure:
    /// ```json
    /// {
    ///   "action": "create" | "update" | "remove",
    ///   "type": "Issue" | "Comment" | "Project" | ...,
    ///   "data": { ... the entity ... },
    ///   "url": "https://linear.app/...",
    ///   "updatedFrom": { ... previous values for changed fields ... },
    ///   "createdAt": "2026-..."
    /// }
    /// ```
    pub fn handle(payload: &serde_json::Value) -> LinearAction {
        let action = payload["action"].as_str().unwrap_or("");
        let entity_type = payload["type"].as_str().unwrap_or("");
        let data = &payload["data"];
        let url = payload["url"].as_str().unwrap_or("").to_string();

        match (entity_type, action) {
            ("Issue", "create") => Self::handle_issue_created(data, &url),
            ("Issue", "update") => Self::handle_issue_updated(data, payload, &url),
            ("Issue", "remove") => Self::handle_issue_removed(data),
            ("Comment", "create") => Self::handle_comment_created(data, &url),
            ("Comment", "update") => Self::handle_comment_updated(data),
            ("Project", "update") => Self::handle_project_updated(data, &url),
            _ => LinearAction::Ignore,
        }
    }

    fn handle_issue_created(data: &serde_json::Value, url: &str) -> LinearAction {
        LinearAction::IssueCreated {
            id: data["id"].as_str().unwrap_or("").to_string(),
            identifier: data["identifier"].as_str().unwrap_or("").to_string(),
            title: data["title"].as_str().unwrap_or("").to_string(),
            team_name: data["team"]["name"].as_str().unwrap_or("").to_string(),
            creator_name: data["creator"]["name"].as_str().unwrap_or("").to_string(),
            url: url.to_string(),
        }
    }

    fn handle_issue_updated(
        data: &serde_json::Value,
        payload: &serde_json::Value,
        url: &str,
    ) -> LinearAction {
        let updated_from = &payload["updatedFrom"];
        let id = data["id"].as_str().unwrap_or("").to_string();
        let identifier = data["identifier"].as_str().unwrap_or("").to_string();
        let title = data["title"].as_str().unwrap_or("").to_string();

        // Detect specific change types from updatedFrom
        if updated_from.is_object() {
            // State change: updatedFrom contains stateId
            if updated_from.get("stateId").is_some() {
                let from_state = updated_from["stateId"].as_str().unwrap_or("").to_string();
                let to_state = data["state"]["name"]
                    .as_str()
                    .unwrap_or_else(|| data["stateId"].as_str().unwrap_or(""))
                    .to_string();
                return LinearAction::IssueStateChanged {
                    id,
                    identifier,
                    title,
                    from_state,
                    to_state,
                    url: url.to_string(),
                };
            }

            // Assignment change: updatedFrom contains assigneeId
            if updated_from.get("assigneeId").is_some() {
                let assignee_name = data["assignee"]["name"]
                    .as_str()
                    .unwrap_or_else(|| data["assigneeId"].as_str().unwrap_or(""))
                    .to_string();
                return LinearAction::IssueAssigned {
                    id,
                    identifier,
                    title,
                    assignee_name,
                    url: url.to_string(),
                };
            }

            // Generic update: collect changed field names
            let changes: Vec<String> = updated_from
                .as_object()
                .map(|obj| obj.keys().cloned().collect())
                .unwrap_or_default();

            return LinearAction::IssueUpdated {
                id,
                identifier,
                title,
                changes,
                url: url.to_string(),
            };
        }

        // No updatedFrom — generic update with no change details
        LinearAction::IssueUpdated {
            id,
            identifier,
            title,
            changes: Vec::new(),
            url: url.to_string(),
        }
    }

    fn handle_issue_removed(data: &serde_json::Value) -> LinearAction {
        LinearAction::IssueRemoved {
            id: data["id"].as_str().unwrap_or("").to_string(),
            identifier: data["identifier"].as_str().unwrap_or("").to_string(),
            title: data["title"].as_str().unwrap_or("").to_string(),
        }
    }

    fn handle_comment_created(data: &serde_json::Value, url: &str) -> LinearAction {
        LinearAction::CommentCreated {
            id: data["id"].as_str().unwrap_or("").to_string(),
            issue_id: data["issueId"].as_str().unwrap_or("").to_string(),
            issue_identifier: data["issue"]["identifier"]
                .as_str()
                .unwrap_or("")
                .to_string(),
            author_name: data["user"]["name"].as_str().unwrap_or("").to_string(),
            body: data["body"].as_str().unwrap_or("").to_string(),
            url: url.to_string(),
        }
    }

    fn handle_comment_updated(data: &serde_json::Value) -> LinearAction {
        LinearAction::CommentUpdated {
            id: data["id"].as_str().unwrap_or("").to_string(),
            issue_id: data["issueId"].as_str().unwrap_or("").to_string(),
            body: data["body"].as_str().unwrap_or("").to_string(),
        }
    }

    fn handle_project_updated(data: &serde_json::Value, url: &str) -> LinearAction {
        LinearAction::ProjectUpdated {
            id: data["id"].as_str().unwrap_or("").to_string(),
            name: data["name"].as_str().unwrap_or("").to_string(),
            state: data["state"].as_str().unwrap_or("").to_string(),
            url: url.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Issue events ────────────────────────────────────────────────

    #[test]
    fn issue_created() {
        let payload = serde_json::json!({
            "action": "create",
            "type": "Issue",
            "data": {
                "id": "issue-1",
                "identifier": "ENG-42",
                "title": "Implement login flow",
                "team": { "name": "Engineering" },
                "creator": { "name": "Alice Smith" }
            },
            "url": "https://linear.app/acme/issue/ENG-42"
        });

        match LinearEventHandler::handle(&payload) {
            LinearAction::IssueCreated {
                id,
                identifier,
                title,
                team_name,
                creator_name,
                url,
            } => {
                assert_eq!(id, "issue-1");
                assert_eq!(identifier, "ENG-42");
                assert_eq!(title, "Implement login flow");
                assert_eq!(team_name, "Engineering");
                assert_eq!(creator_name, "Alice Smith");
                assert_eq!(url, "https://linear.app/acme/issue/ENG-42");
            }
            other => panic!("Expected IssueCreated, got {:?}", other),
        }
    }

    #[test]
    fn issue_state_changed() {
        let payload = serde_json::json!({
            "action": "update",
            "type": "Issue",
            "data": {
                "id": "issue-2",
                "identifier": "ENG-43",
                "title": "Fix auth bug",
                "state": { "name": "In Progress" },
                "stateId": "state-in-progress"
            },
            "updatedFrom": {
                "stateId": "state-backlog"
            },
            "url": "https://linear.app/acme/issue/ENG-43"
        });

        match LinearEventHandler::handle(&payload) {
            LinearAction::IssueStateChanged {
                id,
                identifier,
                title,
                from_state,
                to_state,
                url,
            } => {
                assert_eq!(id, "issue-2");
                assert_eq!(identifier, "ENG-43");
                assert_eq!(title, "Fix auth bug");
                assert_eq!(from_state, "state-backlog");
                assert_eq!(to_state, "In Progress");
                assert_eq!(url, "https://linear.app/acme/issue/ENG-43");
            }
            other => panic!("Expected IssueStateChanged, got {:?}", other),
        }
    }

    #[test]
    fn issue_assigned() {
        let payload = serde_json::json!({
            "action": "update",
            "type": "Issue",
            "data": {
                "id": "issue-3",
                "identifier": "ENG-44",
                "title": "Add dashboard",
                "assignee": { "name": "Bob Jones" },
                "assigneeId": "user-bob"
            },
            "updatedFrom": {
                "assigneeId": null
            },
            "url": "https://linear.app/acme/issue/ENG-44"
        });

        match LinearEventHandler::handle(&payload) {
            LinearAction::IssueAssigned {
                id,
                identifier,
                title,
                assignee_name,
                url,
            } => {
                assert_eq!(id, "issue-3");
                assert_eq!(identifier, "ENG-44");
                assert_eq!(title, "Add dashboard");
                assert_eq!(assignee_name, "Bob Jones");
                assert_eq!(url, "https://linear.app/acme/issue/ENG-44");
            }
            other => panic!("Expected IssueAssigned, got {:?}", other),
        }
    }

    #[test]
    fn issue_generic_update() {
        let payload = serde_json::json!({
            "action": "update",
            "type": "Issue",
            "data": {
                "id": "issue-4",
                "identifier": "ENG-45",
                "title": "Updated title",
                "priority": 2
            },
            "updatedFrom": {
                "title": "Old title",
                "priority": 1
            },
            "url": "https://linear.app/acme/issue/ENG-45"
        });

        match LinearEventHandler::handle(&payload) {
            LinearAction::IssueUpdated {
                id,
                identifier,
                title,
                changes,
                url,
            } => {
                assert_eq!(id, "issue-4");
                assert_eq!(identifier, "ENG-45");
                assert_eq!(title, "Updated title");
                assert!(changes.contains(&"title".to_string()));
                assert!(changes.contains(&"priority".to_string()));
                assert_eq!(changes.len(), 2);
                assert_eq!(url, "https://linear.app/acme/issue/ENG-45");
            }
            other => panic!("Expected IssueUpdated, got {:?}", other),
        }
    }

    #[test]
    fn issue_update_without_updated_from() {
        let payload = serde_json::json!({
            "action": "update",
            "type": "Issue",
            "data": {
                "id": "issue-5",
                "identifier": "ENG-46",
                "title": "Some issue"
            },
            "url": "https://linear.app/acme/issue/ENG-46"
        });

        match LinearEventHandler::handle(&payload) {
            LinearAction::IssueUpdated { changes, .. } => {
                assert!(changes.is_empty());
            }
            other => panic!("Expected IssueUpdated, got {:?}", other),
        }
    }

    #[test]
    fn issue_removed() {
        let payload = serde_json::json!({
            "action": "remove",
            "type": "Issue",
            "data": {
                "id": "issue-6",
                "identifier": "ENG-47",
                "title": "Deleted issue"
            }
        });

        match LinearEventHandler::handle(&payload) {
            LinearAction::IssueRemoved {
                id,
                identifier,
                title,
            } => {
                assert_eq!(id, "issue-6");
                assert_eq!(identifier, "ENG-47");
                assert_eq!(title, "Deleted issue");
            }
            other => panic!("Expected IssueRemoved, got {:?}", other),
        }
    }

    // ── Comment events ──────────────────────────────────────────────

    #[test]
    fn comment_created() {
        let payload = serde_json::json!({
            "action": "create",
            "type": "Comment",
            "data": {
                "id": "comment-1",
                "issueId": "issue-1",
                "issue": { "identifier": "ENG-42" },
                "user": { "name": "Alice Smith" },
                "body": "This is a comment on the issue."
            },
            "url": "https://linear.app/acme/issue/ENG-42#comment-1"
        });

        match LinearEventHandler::handle(&payload) {
            LinearAction::CommentCreated {
                id,
                issue_id,
                issue_identifier,
                author_name,
                body,
                url,
            } => {
                assert_eq!(id, "comment-1");
                assert_eq!(issue_id, "issue-1");
                assert_eq!(issue_identifier, "ENG-42");
                assert_eq!(author_name, "Alice Smith");
                assert_eq!(body, "This is a comment on the issue.");
                assert_eq!(url, "https://linear.app/acme/issue/ENG-42#comment-1");
            }
            other => panic!("Expected CommentCreated, got {:?}", other),
        }
    }

    #[test]
    fn comment_updated() {
        let payload = serde_json::json!({
            "action": "update",
            "type": "Comment",
            "data": {
                "id": "comment-2",
                "issueId": "issue-1",
                "body": "Updated comment body."
            }
        });

        match LinearEventHandler::handle(&payload) {
            LinearAction::CommentUpdated { id, issue_id, body } => {
                assert_eq!(id, "comment-2");
                assert_eq!(issue_id, "issue-1");
                assert_eq!(body, "Updated comment body.");
            }
            other => panic!("Expected CommentUpdated, got {:?}", other),
        }
    }

    // ── Project events ──────────────────────────────────────────────

    #[test]
    fn project_updated() {
        let payload = serde_json::json!({
            "action": "update",
            "type": "Project",
            "data": {
                "id": "proj-1",
                "name": "Q1 Roadmap",
                "state": "started"
            },
            "url": "https://linear.app/acme/project/q1-roadmap"
        });

        match LinearEventHandler::handle(&payload) {
            LinearAction::ProjectUpdated {
                id,
                name,
                state,
                url,
            } => {
                assert_eq!(id, "proj-1");
                assert_eq!(name, "Q1 Roadmap");
                assert_eq!(state, "started");
                assert_eq!(url, "https://linear.app/acme/project/q1-roadmap");
            }
            other => panic!("Expected ProjectUpdated, got {:?}", other),
        }
    }

    // ── Ignored events ──────────────────────────────────────────────

    #[test]
    fn unknown_type_ignored() {
        let payload = serde_json::json!({
            "action": "create",
            "type": "Cycle",
            "data": { "id": "cycle-1" }
        });

        assert!(matches!(
            LinearEventHandler::handle(&payload),
            LinearAction::Ignore
        ));
    }

    #[test]
    fn unknown_action_ignored() {
        let payload = serde_json::json!({
            "action": "archive",
            "type": "Issue",
            "data": { "id": "issue-1" }
        });

        assert!(matches!(
            LinearEventHandler::handle(&payload),
            LinearAction::Ignore
        ));
    }

    #[test]
    fn empty_payload_ignored() {
        let payload = serde_json::json!({});

        assert!(matches!(
            LinearEventHandler::handle(&payload),
            LinearAction::Ignore
        ));
    }

    // ── State change priority over assignment ───────────────────────

    #[test]
    fn state_change_takes_priority_when_both_present() {
        // If both stateId and assigneeId are in updatedFrom, stateId wins
        let payload = serde_json::json!({
            "action": "update",
            "type": "Issue",
            "data": {
                "id": "issue-7",
                "identifier": "ENG-48",
                "title": "Multi-change issue",
                "state": { "name": "Done" },
                "assignee": { "name": "Carol" }
            },
            "updatedFrom": {
                "stateId": "state-old",
                "assigneeId": "user-old"
            },
            "url": "https://linear.app/acme/issue/ENG-48"
        });

        // Should be IssueStateChanged because stateId is checked first
        assert!(matches!(
            LinearEventHandler::handle(&payload),
            LinearAction::IssueStateChanged { .. }
        ));
    }

    // ── Missing fields handled gracefully ───────────────────────────

    #[test]
    fn issue_created_missing_nested_fields() {
        let payload = serde_json::json!({
            "action": "create",
            "type": "Issue",
            "data": {
                "id": "issue-8",
                "identifier": "ENG-49"
            },
            "url": "https://linear.app/acme/issue/ENG-49"
        });

        match LinearEventHandler::handle(&payload) {
            LinearAction::IssueCreated {
                title,
                team_name,
                creator_name,
                ..
            } => {
                assert_eq!(title, "");
                assert_eq!(team_name, "");
                assert_eq!(creator_name, "");
            }
            other => panic!("Expected IssueCreated, got {:?}", other),
        }
    }

    // ── Serialization round-trip ────────────────────────────────────

    #[test]
    fn action_serialization_round_trip() {
        let action = LinearAction::IssueCreated {
            id: "id-1".to_string(),
            identifier: "ENG-1".to_string(),
            title: "Test".to_string(),
            team_name: "Team".to_string(),
            creator_name: "Creator".to_string(),
            url: "https://linear.app/test".to_string(),
        };

        let json = serde_json::to_string(&action).unwrap();
        let parsed: LinearAction = serde_json::from_str(&json).unwrap();
        match parsed {
            LinearAction::IssueCreated {
                identifier, title, ..
            } => {
                assert_eq!(identifier, "ENG-1");
                assert_eq!(title, "Test");
            }
            other => panic!("Expected IssueCreated, got {:?}", other),
        }
    }
}
