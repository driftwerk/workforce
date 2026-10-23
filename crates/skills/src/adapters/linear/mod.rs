//! Linear skill — agent-callable tools for interacting with the Linear API.
//!
//! Provides tools for managing issues, comments, workflow states, teams, and documents.
//! Authentication is via API key stored in the secrets vault or environment variables.
//! Supports both single-workspace and multi-workspace configurations.

pub mod client;
pub mod config;
pub mod event_handler;
pub mod tools;
pub mod types;

use std::collections::HashMap;
use std::sync::Arc;

use workforce_agent::tools::Tool;

use crate::adapters::utils::{build_instances, Instances};
use crate::definition::{
    CredentialRequirement, SkillDefinition, SkillPolicy, SkillRequirements, SkillTool,
};

pub use config::LinearSkillConfig;

/// Read-only Linear tools — the worker-safe read surface fed to
/// [`crate::adapters::read_only_adapter_tools`]. Classification only: Linear's
/// `default_policies` intentionally keep every tool at `allow`, so this list
/// does not alter approval behaviour.
pub const READ_TOOLS: &[&str] = &[
    "linear.issue_list",
    "linear.issue_get",
    "linear.issue_search",
    "linear.issue_children",
    "linear.comment_list",
    "linear.states_list",
    "linear.teams_list",
    "linear.project_list",
    "linear.project_get",
    "linear.labels_list",
    "linear.members_list",
    "linear.document_get",
    "linear.document_list",
    "linear.document_search",
];

pub const WRITE_TOOLS: &[&str] = &[
    "linear.issue_create",
    "linear.issue_update",
    "linear.issue_bulk_update",
    "linear.issue_link",
    "linear.comment_add",
    "linear.document_create",
    "linear.document_update",
    "linear.document_delete",
];

/// The Linear skill — provides tools for interacting with the Linear GraphQL API.
pub struct LinearSkill {
    clients: Arc<HashMap<String, Arc<client::LinearClient>>>,
    default_workspace: String,
    default_team_id: Option<String>,
    /// Team key prefix → workspace name (e.g. "STA" → "contoso").
    prefix_map: Arc<HashMap<String, String>>,
}

impl LinearSkill {
    /// Create from config with static credentials captured at build time.
    /// Use [`LinearSkill::with_store`] for per-user credential resolution.
    pub fn new(config: LinearSkillConfig) -> eyre::Result<Self> {
        Self::build(config, |_, ws| {
            client::LinearClient::new(&ws.api_key, &ws.base_url)
        })
    }

    /// Build a `LinearSkill` whose clients resolve their API keys from
    /// `store` on every request. The store is expected to be a
    /// `LayeredUserVault` so per-user overrides
    /// (`users/{uid}/skills/linear/workspaces/{ws}/api_key`) take effect
    /// transparently. The hub uses this; single-tenant CLI uses
    /// [`LinearSkill::new`].
    pub fn with_store(
        config: LinearSkillConfig,
        store: Arc<dyn workforce_common::settings::SecretStore>,
    ) -> eyre::Result<Self> {
        Self::build(config, |name, ws| {
            client::LinearClient::with_store(Arc::clone(&store), name, &ws.base_url)
        })
    }

    fn build(
        config: LinearSkillConfig,
        make: impl Fn(&str, &config::LinearWorkspaceConfig) -> eyre::Result<client::LinearClient>,
    ) -> eyre::Result<Self> {
        let resolved = config.resolve();
        let Instances { clients, default } = build_instances(
            "linear",
            &resolved,
            config.default_workspace.as_deref(),
            "No Linear workspaces configured. Set api_key or workspaces.",
            make,
        )?;
        let default_team_id = resolved
            .iter()
            .find(|(name, _)| *name == default)
            .and_then(|(_, ws)| ws.default_team_id.clone());
        Ok(Self {
            clients,
            default_workspace: default,
            default_team_id,
            prefix_map: Arc::new(config.prefix_map),
        })
    }

    /// The instances this skill exposes, for the environments of the agent
    /// that loaded it.
    pub fn variants(&self) -> Option<workforce_agent::llm::environment::SkillVariants> {
        workforce_agent::llm::environment::SkillVariants::of(
            self.clients.keys().cloned(),
            &self.default_workspace,
        )
    }

    /// Return all agent-callable tools.
    pub fn tools(&self) -> Vec<Arc<dyn Tool>> {
        let pm = &self.prefix_map;
        vec![
            Arc::new(tools::LinearIssueListTool::new(
                Arc::clone(&self.clients),
                self.default_workspace.clone(),
                self.default_team_id.clone(),
                Arc::clone(pm),
            )),
            Arc::new(tools::LinearIssueGetTool::new(
                Arc::clone(&self.clients),
                self.default_workspace.clone(),
                Arc::clone(pm),
            )),
            Arc::new(tools::LinearIssueSearchTool::new(
                Arc::clone(&self.clients),
                self.default_workspace.clone(),
                Arc::clone(pm),
            )),
            Arc::new(tools::LinearIssueCreateTool::new(
                Arc::clone(&self.clients),
                self.default_workspace.clone(),
                self.default_team_id.clone(),
                Arc::clone(pm),
            )),
            Arc::new(tools::LinearIssueUpdateTool::new(
                Arc::clone(&self.clients),
                self.default_workspace.clone(),
                Arc::clone(pm),
            )),
            Arc::new(tools::LinearIssueBulkUpdateTool::new(
                Arc::clone(&self.clients),
                self.default_workspace.clone(),
                Arc::clone(pm),
            )),
            Arc::new(tools::LinearIssueLinkTool::new(
                Arc::clone(&self.clients),
                self.default_workspace.clone(),
                Arc::clone(pm),
            )),
            Arc::new(tools::LinearIssueChildrenTool::new(
                Arc::clone(&self.clients),
                self.default_workspace.clone(),
                Arc::clone(pm),
            )),
            Arc::new(tools::LinearCommentListTool::new(
                Arc::clone(&self.clients),
                self.default_workspace.clone(),
                Arc::clone(pm),
            )),
            Arc::new(tools::LinearCommentAddTool::new(
                Arc::clone(&self.clients),
                self.default_workspace.clone(),
                Arc::clone(pm),
            )),
            Arc::new(tools::LinearStatesListTool::new(
                Arc::clone(&self.clients),
                self.default_workspace.clone(),
                self.default_team_id.clone(),
                Arc::clone(pm),
            )),
            Arc::new(tools::LinearTeamsListTool::new(
                Arc::clone(&self.clients),
                self.default_workspace.clone(),
                Arc::clone(pm),
            )),
            Arc::new(tools::LinearProjectListTool::new(
                Arc::clone(&self.clients),
                self.default_workspace.clone(),
                Arc::clone(pm),
            )),
            Arc::new(tools::LinearProjectGetTool::new(
                Arc::clone(&self.clients),
                self.default_workspace.clone(),
                Arc::clone(pm),
            )),
            Arc::new(tools::LinearLabelsListTool::new(
                Arc::clone(&self.clients),
                self.default_workspace.clone(),
                Arc::clone(pm),
            )),
            Arc::new(tools::LinearMembersListTool::new(
                Arc::clone(&self.clients),
                self.default_workspace.clone(),
                Arc::clone(pm),
            )),
            Arc::new(tools::LinearDocumentGetTool::new(
                Arc::clone(&self.clients),
                self.default_workspace.clone(),
                Arc::clone(pm),
            )),
            Arc::new(tools::LinearDocumentListTool::new(
                Arc::clone(&self.clients),
                self.default_workspace.clone(),
                Arc::clone(pm),
            )),
            Arc::new(tools::LinearDocumentCreateTool::new(
                Arc::clone(&self.clients),
                self.default_workspace.clone(),
                Arc::clone(pm),
            )),
            Arc::new(tools::LinearDocumentUpdateTool::new(
                Arc::clone(&self.clients),
                self.default_workspace.clone(),
                Arc::clone(pm),
            )),
            Arc::new(tools::LinearDocumentDeleteTool::new(
                Arc::clone(&self.clients),
                self.default_workspace.clone(),
                Arc::clone(pm),
            )),
            Arc::new(tools::LinearDocumentSearchTool::new(
                Arc::clone(&self.clients),
                self.default_workspace.clone(),
                Arc::clone(pm),
            )),
        ]
    }

    /// Tool names provided by this skill, in registration order.
    pub fn tool_names() -> Vec<&'static str> {
        vec![
            "linear.issue_list",
            "linear.issue_get",
            "linear.issue_search",
            "linear.issue_create",
            "linear.issue_update",
            "linear.issue_bulk_update",
            "linear.issue_link",
            "linear.issue_children",
            "linear.comment_list",
            "linear.comment_add",
            "linear.states_list",
            "linear.teams_list",
            "linear.project_list",
            "linear.project_get",
            "linear.labels_list",
            "linear.members_list",
            "linear.document_get",
            "linear.document_list",
            "linear.document_create",
            "linear.document_update",
            "linear.document_delete",
            "linear.document_search",
        ]
    }

    /// Skill definition for the Linear integration.
    pub fn definition() -> SkillDefinition {
        SkillDefinition {
            name: "linear-integration".to_string(),
            version: "1.0.0".to_string(),
            description: "Interact with Linear issues, comments, workflow states, teams, projects, labels, members, and documents"
                .to_string(),
            author: "workforce".to_string(),
            license: "MIT".to_string(),

            tools: vec![
                SkillTool {
                    name: "linear.issue_list".to_string(),
                    description:
                        "List issues with optional filters (state, priority, team, assignee, project, parent)"
                            .to_string(),
                    input_schema: serde_json::json!({
                        "type": "object",
                        "properties": {
                            "team_id": { "type": "string" },
                            "state": { "type": "string" },
                            "priority": { "type": "integer" },
                            "assignee_id": { "type": "string" },
                            "project_id": { "type": "string" },
                            "parent_id": { "type": "string" },
                            "limit": { "type": "integer" },
                            "workspace": { "type": "string" }
                        }
                    }),
                },
                SkillTool {
                    name: "linear.issue_get".to_string(),
                    description: "Get a specific issue by its identifier".to_string(),
                    input_schema: serde_json::json!({
                        "type": "object",
                        "properties": {
                            "identifier": { "type": "string" },
                            "workspace": { "type": "string" }
                        },
                        "required": ["identifier"]
                    }),
                },
                SkillTool {
                    name: "linear.issue_search".to_string(),
                    description: "Full-text search for issues".to_string(),
                    input_schema: serde_json::json!({
                        "type": "object",
                        "properties": {
                            "query": { "type": "string" },
                            "limit": { "type": "integer" },
                            "workspace": { "type": "string" }
                        },
                        "required": ["query"]
                    }),
                },
                SkillTool {
                    name: "linear.issue_create".to_string(),
                    description: "Create a new issue".to_string(),
                    input_schema: serde_json::json!({
                        "type": "object",
                        "properties": {
                            "title": { "type": "string" },
                            "description": { "type": "string" },
                            "team_id": { "type": "string" },
                            "state_id": { "type": "string" },
                            "priority": { "type": "integer" },
                            "assignee_id": { "type": "string" },
                            "workspace": { "type": "string" }
                        },
                        "required": ["title"]
                    }),
                },
                SkillTool {
                    name: "linear.issue_update".to_string(),
                    description: "Update an existing issue".to_string(),
                    input_schema: serde_json::json!({
                        "type": "object",
                        "properties": {
                            "identifier": { "type": "string" },
                            "title": { "type": "string" },
                            "description": { "type": "string" },
                            "state_id": { "type": "string" },
                            "priority": { "type": "integer" },
                            "assignee_id": { "type": "string" },
                            "workspace": { "type": "string" }
                        },
                        "required": ["identifier"]
                    }),
                },
                SkillTool {
                    name: "linear.issue_bulk_update".to_string(),
                    description: "Bulk update multiple issues at once (state, priority, assignee, labels)".to_string(),
                    input_schema: serde_json::json!({
                        "type": "object",
                        "properties": {
                            "identifiers": { "type": "array", "items": { "type": "string" } },
                            "state_id": { "type": "string" },
                            "priority": { "type": "integer" },
                            "assignee_id": { "type": "string" },
                            "label_ids": { "type": "array", "items": { "type": "string" } },
                            "workspace": { "type": "string" }
                        },
                        "required": ["identifiers"]
                    }),
                },
                SkillTool {
                    name: "linear.issue_link".to_string(),
                    description: "Create a relation between two issues (blocks, blocked_by, related, duplicate)".to_string(),
                    input_schema: serde_json::json!({
                        "type": "object",
                        "properties": {
                            "identifier": { "type": "string" },
                            "related_identifier": { "type": "string" },
                            "type": { "type": "string" },
                            "workspace": { "type": "string" }
                        },
                        "required": ["identifier", "related_identifier", "type"]
                    }),
                },
                SkillTool {
                    name: "linear.issue_children".to_string(),
                    description: "List child (sub) issues of a parent issue".to_string(),
                    input_schema: serde_json::json!({
                        "type": "object",
                        "properties": {
                            "identifier": { "type": "string" },
                            "limit": { "type": "integer" },
                            "workspace": { "type": "string" }
                        },
                        "required": ["identifier"]
                    }),
                },
                SkillTool {
                    name: "linear.comment_list".to_string(),
                    description: "List comments on an issue".to_string(),
                    input_schema: serde_json::json!({
                        "type": "object",
                        "properties": {
                            "identifier": { "type": "string" },
                            "limit": { "type": "integer" },
                            "workspace": { "type": "string" }
                        },
                        "required": ["identifier"]
                    }),
                },
                SkillTool {
                    name: "linear.comment_add".to_string(),
                    description: "Add a comment to an issue".to_string(),
                    input_schema: serde_json::json!({
                        "type": "object",
                        "properties": {
                            "identifier": { "type": "string" },
                            "body": { "type": "string" },
                            "workspace": { "type": "string" }
                        },
                        "required": ["identifier", "body"]
                    }),
                },
                SkillTool {
                    name: "linear.states_list".to_string(),
                    description: "List workflow states for a team".to_string(),
                    input_schema: serde_json::json!({
                        "type": "object",
                        "properties": {
                            "team_id": { "type": "string" },
                            "workspace": { "type": "string" }
                        }
                    }),
                },
                SkillTool {
                    name: "linear.teams_list".to_string(),
                    description: "List all teams in the workspace".to_string(),
                    input_schema: serde_json::json!({
                        "type": "object",
                        "properties": {
                            "workspace": { "type": "string" }
                        }
                    }),
                },
                SkillTool {
                    name: "linear.project_list".to_string(),
                    description: "List all projects in the workspace".to_string(),
                    input_schema: serde_json::json!({
                        "type": "object",
                        "properties": {
                            "limit": { "type": "integer" },
                            "include_archived": { "type": "boolean" },
                            "workspace": { "type": "string" }
                        }
                    }),
                },
                SkillTool {
                    name: "linear.project_get".to_string(),
                    description: "Get a specific project by its ID".to_string(),
                    input_schema: serde_json::json!({
                        "type": "object",
                        "properties": {
                            "id": { "type": "string" },
                            "workspace": { "type": "string" }
                        },
                        "required": ["id"]
                    }),
                },
                SkillTool {
                    name: "linear.labels_list".to_string(),
                    description: "List all issue labels in the workspace".to_string(),
                    input_schema: serde_json::json!({
                        "type": "object",
                        "properties": {
                            "limit": { "type": "integer" },
                            "workspace": { "type": "string" }
                        }
                    }),
                },
                SkillTool {
                    name: "linear.members_list".to_string(),
                    description: "List all members in the workspace".to_string(),
                    input_schema: serde_json::json!({
                        "type": "object",
                        "properties": {
                            "limit": { "type": "integer" },
                            "workspace": { "type": "string" }
                        }
                    }),
                },
                SkillTool {
                    name: "linear.document_get".to_string(),
                    description: "Get a specific Linear document by its UUID or URL".to_string(),
                    input_schema: serde_json::json!({
                        "type": "object",
                        "properties": {
                            "id": { "type": "string" },
                            "workspace": { "type": "string" }
                        },
                        "required": ["id"]
                    }),
                },
                SkillTool {
                    name: "linear.document_list".to_string(),
                    description: "List documents, optionally filtered by project".to_string(),
                    input_schema: serde_json::json!({
                        "type": "object",
                        "properties": {
                            "project_id": { "type": "string" },
                            "limit": { "type": "integer" },
                            "workspace": { "type": "string" }
                        }
                    }),
                },
                SkillTool {
                    name: "linear.document_create".to_string(),
                    description: "Create a new Linear document".to_string(),
                    input_schema: serde_json::json!({
                        "type": "object",
                        "properties": {
                            "title": { "type": "string" },
                            "content": { "type": "string" },
                            "project_id": { "type": "string" },
                            "icon": { "type": "string" },
                            "color": { "type": "string" },
                            "workspace": { "type": "string" }
                        },
                        "required": ["title"]
                    }),
                },
                SkillTool {
                    name: "linear.document_update".to_string(),
                    description: "Update an existing Linear document".to_string(),
                    input_schema: serde_json::json!({
                        "type": "object",
                        "properties": {
                            "id": { "type": "string" },
                            "title": { "type": "string" },
                            "content": { "type": "string" },
                            "icon": { "type": "string" },
                            "color": { "type": "string" },
                            "workspace": { "type": "string" }
                        },
                        "required": ["id"]
                    }),
                },
                SkillTool {
                    name: "linear.document_delete".to_string(),
                    description: "Delete a Linear document by its UUID".to_string(),
                    input_schema: serde_json::json!({
                        "type": "object",
                        "properties": {
                            "id": { "type": "string" },
                            "workspace": { "type": "string" }
                        },
                        "required": ["id"]
                    }),
                },
                SkillTool {
                    name: "linear.document_search".to_string(),
                    description: "Search Linear documents by title".to_string(),
                    input_schema: serde_json::json!({
                        "type": "object",
                        "properties": {
                            "query": { "type": "string" },
                            "project_id": { "type": "string" },
                            "limit": { "type": "integer" },
                            "workspace": { "type": "string" }
                        },
                        "required": ["query"]
                    }),
                },
            ],

            event_sources: Vec::new(), // Linear skill is tool-only, no event sources (webhooks could be added later)

            default_policies: vec![
                SkillPolicy {
                    tool: "linear.issue_list".to_string(),
                    action: "allow".to_string(),
                },
                SkillPolicy {
                    tool: "linear.issue_get".to_string(),
                    action: "allow".to_string(),
                },
                SkillPolicy {
                    tool: "linear.issue_search".to_string(),
                    action: "allow".to_string(),
                },
                SkillPolicy {
                    tool: "linear.issue_create".to_string(),
                    action: "allow".to_string(),
                },
                SkillPolicy {
                    tool: "linear.issue_update".to_string(),
                    action: "allow".to_string(),
                },
                SkillPolicy {
                    tool: "linear.issue_bulk_update".to_string(),
                    action: "allow".to_string(),
                },
                SkillPolicy {
                    tool: "linear.issue_link".to_string(),
                    action: "allow".to_string(),
                },
                SkillPolicy {
                    tool: "linear.issue_children".to_string(),
                    action: "allow".to_string(),
                },
                SkillPolicy {
                    tool: "linear.comment_list".to_string(),
                    action: "allow".to_string(),
                },
                SkillPolicy {
                    tool: "linear.comment_add".to_string(),
                    action: "allow".to_string(),
                },
                SkillPolicy {
                    tool: "linear.states_list".to_string(),
                    action: "allow".to_string(),
                },
                SkillPolicy {
                    tool: "linear.teams_list".to_string(),
                    action: "allow".to_string(),
                },
                SkillPolicy {
                    tool: "linear.project_list".to_string(),
                    action: "allow".to_string(),
                },
                SkillPolicy {
                    tool: "linear.project_get".to_string(),
                    action: "allow".to_string(),
                },
                SkillPolicy {
                    tool: "linear.labels_list".to_string(),
                    action: "allow".to_string(),
                },
                SkillPolicy {
                    tool: "linear.members_list".to_string(),
                    action: "allow".to_string(),
                },
                SkillPolicy {
                    tool: "linear.document_get".to_string(),
                    action: "allow".to_string(),
                },
                SkillPolicy {
                    tool: "linear.document_list".to_string(),
                    action: "allow".to_string(),
                },
                SkillPolicy {
                    tool: "linear.document_create".to_string(),
                    action: "allow".to_string(),
                },
                SkillPolicy {
                    tool: "linear.document_update".to_string(),
                    action: "allow".to_string(),
                },
                SkillPolicy {
                    tool: "linear.document_delete".to_string(),
                    action: "allow".to_string(),
                },
                SkillPolicy {
                    tool: "linear.document_search".to_string(),
                    action: "allow".to_string(),
                },
            ],

            memory_schemas: Vec::new(),

            requirements: SkillRequirements {
                credentials: vec![CredentialRequirement {
                    name: "linear_api_key".to_string(),
                    r#type: "api_key".to_string(),
                    description: "Linear API Key".to_string(),
                }],
                config: Vec::new(),
            },

            dependencies: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn named(names: &[&str], default_workspace: Option<&str>) -> LinearSkillConfig {
        LinearSkillConfig {
            api_key: None,
            base_url: "https://api.linear.app/graphql".to_string(),
            default_team_id: None,
            workspaces: names
                .iter()
                .map(|name| {
                    (
                        name.to_string(),
                        config::LinearWorkspaceConfig {
                            api_key: "k".to_string(),
                            base_url: "https://api.linear.app/graphql".to_string(),
                            default_team_id: Some(format!("{name}-team")),
                        },
                    )
                })
                .collect(),
            default_workspace: default_workspace.map(str::to_string),
            prefix_map: std::collections::HashMap::new(),
        }
    }

    #[test]
    fn the_default_team_follows_the_effective_default_workspace() {
        let sole = LinearSkill::new(named(&["acme"], None)).unwrap();
        assert_eq!(sole.default_workspace, "acme");
        assert_eq!(sole.default_team_id.as_deref(), Some("acme-team"));

        let selected = LinearSkill::new(named(&["acme", "client"], Some("client"))).unwrap();
        assert_eq!(selected.default_workspace, "client");
        assert_eq!(selected.default_team_id.as_deref(), Some("client-team"));

        let unselected = LinearSkill::new(named(&["acme", "client"], None)).unwrap();
        assert_eq!(unselected.default_workspace, "default");
        assert!(unselected.default_team_id.is_none());
    }

    #[test]
    fn tool_names_matches_registered_tools() {
        let cfg = LinearSkillConfig {
            api_key: Some("k".to_string()),
            base_url: "https://api.linear.app/graphql".to_string(),
            default_team_id: None,
            workspaces: std::collections::HashMap::new(),
            default_workspace: None,
            prefix_map: std::collections::HashMap::new(),
        };
        let skill = LinearSkill::new(cfg).unwrap();
        let registered: Vec<String> = skill.tools().iter().map(|t| t.name().to_string()).collect();
        let declared: Vec<String> = LinearSkill::tool_names()
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(
            registered, declared,
            "LinearSkill::tool_names() drifted from tools()"
        );
    }

    #[test]
    fn read_and_write_tool_lists_cover_all_tools() {
        crate::adapters::test_support::assert_read_write_partition(
            LinearSkill::tool_names(),
            READ_TOOLS,
            WRITE_TOOLS,
        );
    }
}
