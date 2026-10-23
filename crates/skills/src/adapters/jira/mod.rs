//! Jira Cloud skill — agent-callable tools for the Jira REST API v3.
//!
//! Authentication uses HTTPS Basic with `email:api_token`. Multi-tenant
//! support mirrors the GitHub and Linear skills: configure named workspaces
//! in the vault and they get auto-routed by project-key prefix.

pub mod client;
pub mod config;
mod markdown;
pub mod tools;

use std::collections::HashMap;
use std::sync::Arc;

use workforce_agent::tools::Tool;

use crate::adapters::utils::{build_instances, Instances};

use crate::definition::{
    CredentialRequirement, SkillDefinition, SkillPolicy, SkillRequirements, SkillTool,
};

pub use config::{JiraSkillConfig, JiraWorkspaceConfig};

/// Read-only Jira tools — the worker-safe read surface fed to
/// [`crate::adapters::read_only_adapter_tools`]. Classification only: Jira's
/// `default_policies` intentionally keep every tool at `allow`, so this list
/// does not alter approval behaviour.
pub const READ_TOOLS: &[&str] = &[
    "jira.search",
    "jira.get_issue",
    "jira.get_attachment",
    "jira.list_comments",
    "jira.list_transitions",
    "jira.list_projects",
    "jira.get_project",
    "jira.search_users",
    "jira.list_boards",
    "jira.list_sprints",
];

/// Mutating Jira tools (including the generic `jira.api` escape hatch).
pub const WRITE_TOOLS: &[&str] = &[
    "jira.create_issue",
    "jira.update_issue",
    "jira.delete_issue",
    "jira.assign_issue",
    "jira.add_comment",
    "jira.transition_issue",
    "jira.add_attachment",
    "jira.add_worklog",
    "jira.api",
];

/// The Jira skill — provides tools for interacting with Jira Cloud REST v3.
pub struct JiraSkill {
    clients: Arc<HashMap<String, Arc<client::JiraClient>>>,
    default_workspace: String,
    prefix_map: Arc<HashMap<String, String>>,
}

impl JiraSkill {
    /// Build a `JiraSkill` from its config with static credentials.
    pub fn new(config: JiraSkillConfig) -> eyre::Result<Self> {
        Self::build(config, |name, ws| {
            let site = ws
                .site
                .as_deref()
                .ok_or_else(|| eyre::eyre!("Jira workspace '{name}' has no site configured"))?;
            client::JiraClient::new(&ws.email, &ws.api_token, site, ws.default_project.clone())
        })
    }

    /// Build a `JiraSkill` with a `SecretStore` for per-user credential resolution.
    ///
    /// Credentials are read from the store on each API request.
    pub fn with_store(
        config: JiraSkillConfig,
        store: Arc<dyn workforce_common::settings::SecretStore>,
    ) -> eyre::Result<Self> {
        Self::build(config, |name, ws| {
            client::JiraClient::with_store(
                Arc::clone(&store),
                name,
                ws.site.as_deref(),
                ws.default_project.clone(),
            )
        })
    }

    fn build(
        config: JiraSkillConfig,
        make: impl Fn(&str, &JiraWorkspaceConfig) -> eyre::Result<client::JiraClient>,
    ) -> eyre::Result<Self> {
        let Instances { clients, default } = build_instances(
            "jira",
            &config.resolve(),
            config.default_workspace.as_deref(),
            "No Jira workspaces configured. Set email/api_token/site or workspaces.*.",
            make,
        )?;
        Ok(Self {
            clients,
            default_workspace: default,
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
        let c = || Arc::clone(&self.clients);
        let w = || self.default_workspace.clone();
        let p = || Arc::clone(&self.prefix_map);
        vec![
            Arc::new(tools::JiraSearchTool::new(c(), w(), p())),
            Arc::new(tools::JiraGetIssueTool::new(c(), w(), p())),
            Arc::new(tools::JiraCreateIssueTool::new(c(), w(), p())),
            Arc::new(tools::JiraUpdateIssueTool::new(c(), w(), p())),
            Arc::new(tools::JiraDeleteIssueTool::new(c(), w(), p())),
            Arc::new(tools::JiraAssignIssueTool::new(c(), w(), p())),
            Arc::new(tools::JiraAddCommentTool::new(c(), w(), p())),
            Arc::new(tools::JiraListCommentsTool::new(c(), w(), p())),
            Arc::new(tools::JiraListTransitionsTool::new(c(), w(), p())),
            Arc::new(tools::JiraTransitionIssueTool::new(c(), w(), p())),
            Arc::new(tools::JiraAddAttachmentTool::new(c(), w(), p())),
            Arc::new(tools::JiraGetAttachmentTool::new(c(), w(), p())),
            Arc::new(tools::JiraListProjectsTool::new(c(), w(), p())),
            Arc::new(tools::JiraGetProjectTool::new(c(), w(), p())),
            Arc::new(tools::JiraSearchUsersTool::new(c(), w(), p())),
            Arc::new(tools::JiraListBoardsTool::new(c(), w(), p())),
            Arc::new(tools::JiraListSprintsTool::new(c(), w(), p())),
            Arc::new(tools::JiraAddWorklogTool::new(c(), w(), p())),
            Arc::new(tools::JiraApiTool::new(c(), w(), p())),
        ]
    }

    /// Tool names provided by this skill, in registration order.
    pub fn tool_names() -> Vec<&'static str> {
        vec![
            "jira.search",
            "jira.get_issue",
            "jira.create_issue",
            "jira.update_issue",
            "jira.delete_issue",
            "jira.assign_issue",
            "jira.add_comment",
            "jira.list_comments",
            "jira.list_transitions",
            "jira.transition_issue",
            "jira.add_attachment",
            "jira.get_attachment",
            "jira.list_projects",
            "jira.get_project",
            "jira.search_users",
            "jira.list_boards",
            "jira.list_sprints",
            "jira.add_worklog",
            "jira.api",
        ]
    }

    /// Skill definition for the Jira integration.
    pub fn definition() -> SkillDefinition {
        let mk_tool = |name: &str, description: &str, schema: serde_json::Value| SkillTool {
            name: name.to_string(),
            description: description.to_string(),
            input_schema: schema,
        };
        let allow = |name: &str| SkillPolicy {
            tool: name.to_string(),
            action: "allow".to_string(),
        };

        SkillDefinition {
            name: "jira-integration".to_string(),
            version: "1.0.0".to_string(),
            description: "Interact with Jira Cloud issues, comments, transitions, boards, sprints, and worklogs".to_string(),
            author: "workforce".to_string(),
            license: "MIT".to_string(),

            tools: vec![
                mk_tool(
                    "jira.search",
                    "Search Jira issues using JQL",
                    serde_json::json!({
                        "type": "object",
                        "properties": {
                            "workspace": { "type": "string" },
                            "jql": { "type": "string" },
                            "fields": { "type": "array", "items": { "type": "string" } },
                            "next_page_token": { "type": "string" },
                            "max_results": { "type": "integer" }
                        },
                        "required": ["jql"]
                    }),
                ),
                mk_tool(
                    "jira.get_issue",
                    "Get full details of a Jira issue",
                    serde_json::json!({
                        "type": "object",
                        "properties": {
                            "workspace": { "type": "string" },
                            "issue_key": { "type": "string" }
                        },
                        "required": ["issue_key"]
                    }),
                ),
                mk_tool(
                    "jira.create_issue",
                    "Create a new Jira issue. Descriptions accept markdown (rendered to ADF) or raw ADF.",
                    serde_json::json!({
                        "type": "object",
                        "properties": {
                            "workspace": { "type": "string" },
                            "project_key": { "type": "string" },
                            "summary": { "type": "string" },
                            "issue_type": { "type": "string" },
                            "description": { "type": "string" },
                            "assignee_account_id": { "type": "string" },
                            "labels": { "type": "array", "items": { "type": "string" } },
                            "priority": { "type": "string" },
                            "fields": { "type": "object" }
                        },
                        "required": ["summary", "issue_type"]
                    }),
                ),
                mk_tool(
                    "jira.update_issue",
                    "Update fields on an existing Jira issue. Descriptions accept markdown (rendered to ADF) or raw ADF.",
                    serde_json::json!({
                        "type": "object",
                        "properties": {
                            "workspace": { "type": "string" },
                            "issue_key": { "type": "string" },
                            "summary": { "type": "string" },
                            "description": { "type": "string" },
                            "labels": { "type": "array", "items": { "type": "string" } },
                            "priority": { "type": "string" },
                            "fields": { "type": "object" }
                        },
                        "required": ["issue_key"]
                    }),
                ),
                mk_tool(
                    "jira.delete_issue",
                    "Delete a Jira issue permanently",
                    serde_json::json!({
                        "type": "object",
                        "properties": {
                            "workspace": { "type": "string" },
                            "issue_key": { "type": "string" }
                        },
                        "required": ["issue_key"]
                    }),
                ),
                mk_tool(
                    "jira.assign_issue",
                    "Assign a Jira issue to a user by accountId",
                    serde_json::json!({
                        "type": "object",
                        "properties": {
                            "workspace": { "type": "string" },
                            "issue_key": { "type": "string" },
                            "account_id": { "type": ["string", "null"] }
                        },
                        "required": ["issue_key"]
                    }),
                ),
                mk_tool(
                    "jira.add_comment",
                    "Add a comment to a Jira issue. Bodies accept markdown (rendered to ADF) or raw ADF.",
                    serde_json::json!({
                        "type": "object",
                        "properties": {
                            "workspace": { "type": "string" },
                            "issue_key": { "type": "string" },
                            "body": { "type": "string" }
                        },
                        "required": ["issue_key", "body"]
                    }),
                ),
                mk_tool(
                    "jira.list_comments",
                    "List comments on a Jira issue",
                    serde_json::json!({
                        "type": "object",
                        "properties": {
                            "workspace": { "type": "string" },
                            "issue_key": { "type": "string" }
                        },
                        "required": ["issue_key"]
                    }),
                ),
                mk_tool(
                    "jira.list_transitions",
                    "List the workflow transitions available on a Jira issue",
                    serde_json::json!({
                        "type": "object",
                        "properties": {
                            "workspace": { "type": "string" },
                            "issue_key": { "type": "string" }
                        },
                        "required": ["issue_key"]
                    }),
                ),
                mk_tool(
                    "jira.transition_issue",
                    "Move a Jira issue through a workflow transition",
                    serde_json::json!({
                        "type": "object",
                        "properties": {
                            "workspace": { "type": "string" },
                            "issue_key": { "type": "string" },
                            "transition": { "type": "string" }
                        },
                        "required": ["issue_key", "transition"]
                    }),
                ),
                mk_tool(
                    "jira.add_attachment",
                    "Upload a local file as an attachment on a Jira issue",
                    serde_json::json!({
                        "type": "object",
                        "properties": {
                            "workspace": { "type": "string" },
                            "issue_key": { "type": "string" },
                            "path": { "type": "string" }
                        },
                        "required": ["issue_key", "path"]
                    }),
                ),
                mk_tool(
                    "jira.get_attachment",
                    "Download a Jira issue attachment and render it into the agent's view (image, PDF, document, audio, or video)",
                    serde_json::json!({
                        "type": "object",
                        "properties": {
                            "workspace": { "type": "string" },
                            "issue_key": { "type": "string" },
                            "attachment_id": { "type": "string" }
                        },
                        "required": ["attachment_id"]
                    }),
                ),
                mk_tool(
                    "jira.list_projects",
                    "List Jira projects accessible to the authenticated user",
                    serde_json::json!({
                        "type": "object",
                        "properties": {
                            "workspace": { "type": "string" },
                            "query": { "type": "string" },
                            "max_results": { "type": "integer" }
                        }
                    }),
                ),
                mk_tool(
                    "jira.get_project",
                    "Get details about a Jira project",
                    serde_json::json!({
                        "type": "object",
                        "properties": {
                            "workspace": { "type": "string" },
                            "project_key": { "type": "string" }
                        },
                        "required": ["project_key"]
                    }),
                ),
                mk_tool(
                    "jira.search_users",
                    "Search Jira users by name or email",
                    serde_json::json!({
                        "type": "object",
                        "properties": {
                            "workspace": { "type": "string" },
                            "query": { "type": "string" },
                            "max_results": { "type": "integer" }
                        },
                        "required": ["query"]
                    }),
                ),
                mk_tool(
                    "jira.list_boards",
                    "List Jira Software (Agile) boards",
                    serde_json::json!({
                        "type": "object",
                        "properties": {
                            "workspace": { "type": "string" },
                            "project_key": { "type": "string" },
                            "type": { "type": "string" },
                            "max_results": { "type": "integer" }
                        }
                    }),
                ),
                mk_tool(
                    "jira.list_sprints",
                    "List sprints on a Jira board",
                    serde_json::json!({
                        "type": "object",
                        "properties": {
                            "workspace": { "type": "string" },
                            "board_id": { "type": "integer" },
                            "state": { "type": "string" }
                        },
                        "required": ["board_id"]
                    }),
                ),
                mk_tool(
                    "jira.add_worklog",
                    "Log time spent on a Jira issue. Optional comment accepts markdown (rendered to ADF) or raw ADF.",
                    serde_json::json!({
                        "type": "object",
                        "properties": {
                            "workspace": { "type": "string" },
                            "issue_key": { "type": "string" },
                            "time_spent": { "type": "string" },
                            "comment": { "type": "string" },
                            "started": { "type": "string" }
                        },
                        "required": ["issue_key", "time_spent"]
                    }),
                ),
                mk_tool(
                    "jira.api",
                    "Make an arbitrary Jira REST API request",
                    serde_json::json!({
                        "type": "object",
                        "properties": {
                            "workspace": { "type": "string" },
                            "method": { "type": "string", "enum": ["GET", "POST", "PUT", "DELETE"] },
                            "path": { "type": "string" },
                            "body": { "type": "object" },
                            "query": { "type": "object" }
                        },
                        "required": ["method", "path"]
                    }),
                ),
            ],

            event_sources: vec![],

            default_policies: vec![
                allow("jira.search"),
                allow("jira.get_issue"),
                allow("jira.create_issue"),
                allow("jira.update_issue"),
                allow("jira.delete_issue"),
                allow("jira.assign_issue"),
                allow("jira.add_comment"),
                allow("jira.list_comments"),
                allow("jira.list_transitions"),
                allow("jira.transition_issue"),
                allow("jira.add_attachment"),
                allow("jira.get_attachment"),
                allow("jira.list_projects"),
                allow("jira.get_project"),
                allow("jira.search_users"),
                allow("jira.list_boards"),
                allow("jira.list_sprints"),
                allow("jira.add_worklog"),
                allow("jira.api"),
            ],

            memory_schemas: Vec::new(),

            requirements: SkillRequirements {
                credentials: vec![
                    CredentialRequirement {
                        name: "jira_email".to_string(),
                        r#type: "string".to_string(),
                        description: "Atlassian account email used for Basic auth".to_string(),
                    },
                    CredentialRequirement {
                        name: "jira_api_token".to_string(),
                        r#type: "api_key".to_string(),
                        description: "Jira Cloud API token from id.atlassian.com".to_string(),
                    },
                    CredentialRequirement {
                        name: "jira_site".to_string(),
                        r#type: "string".to_string(),
                        description: "Atlassian site URL, e.g. https://acme.atlassian.net".to_string(),
                    },
                ],
                config: Vec::new(),
            },

            dependencies: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skill_builds_with_flat_config() {
        let cfg = JiraSkillConfig {
            email: Some("you@acme.com".to_string()),
            api_token: Some("tok".to_string()),
            site: Some("https://acme.atlassian.net".to_string()),
            ..JiraSkillConfig::default()
        };
        let skill = JiraSkill::new(cfg).unwrap();
        let tools = skill.tools();
        assert_eq!(tools.len(), 19);
        // Sanity check: all tool names start with "jira."
        for t in &tools {
            assert!(t.name().starts_with("jira."));
        }
    }

    #[test]
    fn skill_errors_with_no_workspaces() {
        let cfg = JiraSkillConfig::default();
        assert!(JiraSkill::new(cfg).is_err());
    }

    fn workspace(site: &str) -> JiraWorkspaceConfig {
        JiraWorkspaceConfig {
            email: "you@acme.com".to_string(),
            api_token: "tok".to_string(),
            site: Some(site.to_string()),
            default_project: None,
            project_keys: Vec::new(),
        }
    }

    fn named(names: &[&str], default_workspace: Option<&str>) -> JiraSkillConfig {
        JiraSkillConfig {
            workspaces: names
                .iter()
                .map(|n| {
                    (
                        n.to_string(),
                        workspace(&format!("https://{n}.atlassian.net")),
                    )
                })
                .collect(),
            default_workspace: default_workspace.map(str::to_string),
            ..JiraSkillConfig::default()
        }
    }

    #[test]
    fn a_sole_named_workspace_becomes_the_default_without_a_selector() {
        let skill = JiraSkill::new(named(&["harbor"], None)).unwrap();
        assert_eq!(skill.default_workspace, "harbor");
        let skill = JiraSkill::new(named(&["harbor"], Some("gone"))).unwrap();
        assert_eq!(skill.default_workspace, "harbor");
    }

    #[test]
    fn flat_credentials_keep_default_as_the_default_beside_named_workspaces() {
        let cfg = JiraSkillConfig {
            email: Some("you@acme.com".to_string()),
            api_token: Some("tok".to_string()),
            site: Some("https://acme.atlassian.net".to_string()),
            ..named(&["mateusz"], None)
        };
        let skill = JiraSkill::new(cfg).unwrap();
        assert_eq!(skill.default_workspace, "default");
    }

    #[test]
    fn a_configured_selector_wins_and_a_stale_one_over_several_is_kept_for_the_call_error() {
        let skill = JiraSkill::new(named(&["acme", "client"], Some("client"))).unwrap();
        assert_eq!(skill.default_workspace, "client");
        let skill = JiraSkill::new(named(&["acme", "client"], Some("gone"))).unwrap();
        assert_eq!(skill.default_workspace, "gone");
        let skill = JiraSkill::new(named(&["acme", "client"], None)).unwrap();
        assert_eq!(skill.default_workspace, "default");
    }

    #[test]
    fn definition_lists_all_19_tools() {
        let def = JiraSkill::definition();
        assert_eq!(def.tools.len(), 19);
        assert_eq!(def.default_policies.len(), 19);
        assert_eq!(def.requirements.credentials.len(), 3);
        assert!(def.validate().is_ok());
    }

    /// Static `tool_names()` must match the names registered by `tools()`.
    /// Drift here breaks the `/cc` MCP allow-list — a tool would be in
    /// `tools/list` from the child mcp-serve but rejected by Claude Code's
    /// `--allowedTools` enforcement.
    #[test]
    fn tool_names_matches_registered_tools() {
        let cfg = JiraSkillConfig {
            email: Some("you@acme.com".to_string()),
            api_token: Some("tok".to_string()),
            site: Some("https://acme.atlassian.net".to_string()),
            ..JiraSkillConfig::default()
        };
        let skill = JiraSkill::new(cfg).unwrap();
        let registered: Vec<String> = skill.tools().iter().map(|t| t.name().to_string()).collect();
        let declared: Vec<String> = JiraSkill::tool_names()
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(
            registered, declared,
            "JiraSkill::tool_names() drifted from tools()"
        );
    }

    #[test]
    fn read_and_write_tool_lists_cover_all_tools() {
        crate::adapters::test_support::assert_read_write_partition(
            JiraSkill::tool_names(),
            READ_TOOLS,
            WRITE_TOOLS,
        );
    }
}
