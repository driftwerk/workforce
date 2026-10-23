//! Notion skill — agent-callable tools for the Notion API.
//!
//! Auth is via internal integration tokens (`Authorization: Bearer <token>`).
//! Supports multi-workspace configuration via an optional `workspace`
//! parameter on every tool.

pub mod client;
pub mod config;
pub mod tools;

use std::collections::HashMap;
use std::sync::Arc;

use workforce_agent::tools::Tool;

use crate::adapters::utils::{build_instances, Instances};
use crate::definition::{
    CredentialRequirement, SkillDefinition, SkillPolicy, SkillRequirements, SkillTool,
};

pub use config::{NotionSkillConfig, NotionWorkspaceConfig};

/// Read-only Notion tools — never mutate the workspace. The single source of
/// truth for the worker-acquirable Notion read surface (see the hub's
/// `worker_read` catalogue). Explicit names, never a `notion.*` glob.
pub const READ_TOOLS: &[&str] = &[
    "notion.search",
    "notion.page_get",
    "notion.database_get",
    "notion.database_query",
    "notion.block_get",
    "notion.block_children",
    "notion.user_list",
    "notion.user_get",
    "notion.comment_list",
];

/// Write/mutating Notion tools — the "(mutating)" tools — never offered to a
/// read-only worker.
pub const WRITE_TOOLS: &[&str] = &[
    "notion.page_create",
    "notion.page_update",
    "notion.page_archive",
    "notion.database_create",
    "notion.block_append",
    "notion.block_update",
    "notion.block_delete",
    "notion.comment_create",
];

pub struct NotionSkill {
    clients: Arc<HashMap<String, Arc<client::NotionClient>>>,
    default_workspace: String,
}

impl NotionSkill {
    /// Build from config with static credentials. Errors if no workspaces are configured.
    pub fn new(config: NotionSkillConfig) -> eyre::Result<Self> {
        Self::build(config, |_, ws| {
            client::NotionClient::new(&ws.token, ws.max_results)
        })
    }

    /// Build a `NotionSkill` whose clients resolve their token from `store`
    /// per request. Pass a `LayeredUserVault` for per-user resolution.
    pub fn with_store(
        config: NotionSkillConfig,
        store: Arc<dyn workforce_common::settings::SecretStore>,
    ) -> eyre::Result<Self> {
        Self::build(config, |name, ws| {
            client::NotionClient::with_store(Arc::clone(&store), name, ws.max_results)
        })
    }

    fn build(
        config: NotionSkillConfig,
        make: impl Fn(&str, &NotionWorkspaceConfig) -> eyre::Result<client::NotionClient>,
    ) -> eyre::Result<Self> {
        let Instances { clients, default } = build_instances(
            "notion",
            &config.resolve(),
            config.default_workspace.as_deref(),
            "No Notion workspaces configured. Set `token` or `[workspaces.<name>]`.",
            make,
        )?;
        Ok(Self {
            clients,
            default_workspace: default,
        })
    }

    pub fn tools(&self) -> Vec<Arc<dyn Tool>> {
        let c = &self.clients;
        let d = &self.default_workspace;
        vec![
            Arc::new(tools::SearchTool::new(Arc::clone(c), d.clone())),
            Arc::new(tools::PageGetTool::new(Arc::clone(c), d.clone())),
            Arc::new(tools::PageCreateTool::new(Arc::clone(c), d.clone())),
            Arc::new(tools::PageUpdateTool::new(Arc::clone(c), d.clone())),
            Arc::new(tools::PageArchiveTool::new(Arc::clone(c), d.clone())),
            Arc::new(tools::DatabaseGetTool::new(Arc::clone(c), d.clone())),
            Arc::new(tools::DatabaseQueryTool::new(Arc::clone(c), d.clone())),
            Arc::new(tools::DatabaseCreateTool::new(Arc::clone(c), d.clone())),
            Arc::new(tools::BlockGetTool::new(Arc::clone(c), d.clone())),
            Arc::new(tools::BlockChildrenTool::new(Arc::clone(c), d.clone())),
            Arc::new(tools::BlockAppendTool::new(Arc::clone(c), d.clone())),
            Arc::new(tools::BlockUpdateTool::new(Arc::clone(c), d.clone())),
            Arc::new(tools::BlockDeleteTool::new(Arc::clone(c), d.clone())),
            Arc::new(tools::UserListTool::new(Arc::clone(c), d.clone())),
            Arc::new(tools::UserGetTool::new(Arc::clone(c), d.clone())),
            Arc::new(tools::CommentListTool::new(Arc::clone(c), d.clone())),
            Arc::new(tools::CommentCreateTool::new(Arc::clone(c), d.clone())),
        ]
    }

    /// Tool names provided by this skill, in registration order.
    pub fn tool_names() -> Vec<&'static str> {
        vec![
            "notion.search",
            "notion.page_get",
            "notion.page_create",
            "notion.page_update",
            "notion.page_archive",
            "notion.database_get",
            "notion.database_query",
            "notion.database_create",
            "notion.block_get",
            "notion.block_children",
            "notion.block_append",
            "notion.block_update",
            "notion.block_delete",
            "notion.user_list",
            "notion.user_get",
            "notion.comment_list",
            "notion.comment_create",
        ]
    }

    pub fn definition() -> SkillDefinition {
        let workspace_param = || {
            serde_json::json!({
                "type": "string",
                "description": "Notion workspace name (multi-workspace setups)"
            })
        };

        let tools = vec![
            SkillTool {
                name: "notion.search".to_string(),
                description: "Search pages and databases by title.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "query": { "type": "string" },
                        "filter": { "type": "object" },
                        "sort": { "type": "object" },
                        "page_size": { "type": "integer" },
                        "start_cursor": { "type": "string" },
                        "workspace": workspace_param(),
                    }
                }),
            },
            SkillTool {
                name: "notion.page_get".to_string(),
                description: "Get a page by ID.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": { "id": { "type": "string" }, "workspace": workspace_param() },
                    "required": ["id"]
                }),
            },
            SkillTool {
                name: "notion.page_create".to_string(),
                description: "Create a page (mutating).".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "parent": { "type": "object" },
                        "properties": { "type": "object" },
                        "children": { "type": "array", "items": { "type": "object" } },
                        "workspace": workspace_param(),
                    },
                    "required": ["parent", "properties"]
                }),
            },
            SkillTool {
                name: "notion.page_update".to_string(),
                description: "Update page properties (mutating).".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "id": { "type": "string" },
                        "properties": { "type": "object" },
                        "workspace": workspace_param(),
                    },
                    "required": ["id"]
                }),
            },
            SkillTool {
                name: "notion.page_archive".to_string(),
                description: "Archive or restore a page (mutating).".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "id": { "type": "string" },
                        "archived": { "type": "boolean" },
                        "workspace": workspace_param(),
                    },
                    "required": ["id"]
                }),
            },
            SkillTool {
                name: "notion.database_get".to_string(),
                description: "Get a database schema by ID.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": { "id": { "type": "string" }, "workspace": workspace_param() },
                    "required": ["id"]
                }),
            },
            SkillTool {
                name: "notion.database_query".to_string(),
                description: "Query a database with filters and sorts.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "id": { "type": "string" },
                        "filter": { "type": "object" },
                        "sorts": { "type": "array" },
                        "page_size": { "type": "integer" },
                        "start_cursor": { "type": "string" },
                        "workspace": workspace_param(),
                    },
                    "required": ["id"]
                }),
            },
            SkillTool {
                name: "notion.database_create".to_string(),
                description: "Create an inline database (mutating).".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "parent": { "type": "object" },
                        "title": { "type": "array" },
                        "properties": { "type": "object" },
                        "workspace": workspace_param(),
                    },
                    "required": ["parent", "title", "properties"]
                }),
            },
            SkillTool {
                name: "notion.block_get".to_string(),
                description: "Get a block by ID.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": { "id": { "type": "string" }, "workspace": workspace_param() },
                    "required": ["id"]
                }),
            },
            SkillTool {
                name: "notion.block_children".to_string(),
                description: "Get child blocks of a block or page.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "id": { "type": "string" },
                        "page_size": { "type": "integer" },
                        "start_cursor": { "type": "string" },
                        "workspace": workspace_param(),
                    },
                    "required": ["id"]
                }),
            },
            SkillTool {
                name: "notion.block_append".to_string(),
                description: "Append child blocks (mutating).".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "id": { "type": "string" },
                        "children": { "type": "array", "items": { "type": "object" } },
                        "workspace": workspace_param(),
                    },
                    "required": ["id", "children"]
                }),
            },
            SkillTool {
                name: "notion.block_update".to_string(),
                description: "Update a block (mutating).".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "id": { "type": "string" },
                        "block": { "type": "object" },
                        "workspace": workspace_param(),
                    },
                    "required": ["id", "block"]
                }),
            },
            SkillTool {
                name: "notion.block_delete".to_string(),
                description: "Delete a block (mutating).".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": { "id": { "type": "string" }, "workspace": workspace_param() },
                    "required": ["id"]
                }),
            },
            SkillTool {
                name: "notion.user_list".to_string(),
                description: "List workspace users.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "page_size": { "type": "integer" },
                        "start_cursor": { "type": "string" },
                        "workspace": workspace_param(),
                    }
                }),
            },
            SkillTool {
                name: "notion.user_get".to_string(),
                description: "Get a user by ID.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": { "id": { "type": "string" }, "workspace": workspace_param() },
                    "required": ["id"]
                }),
            },
            SkillTool {
                name: "notion.comment_list".to_string(),
                description: "List comments on a page or block.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "block_id": { "type": "string" },
                        "page_size": { "type": "integer" },
                        "start_cursor": { "type": "string" },
                        "workspace": workspace_param(),
                    },
                    "required": ["block_id"]
                }),
            },
            SkillTool {
                name: "notion.comment_create".to_string(),
                description: "Create a comment (mutating).".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "parent": { "type": "object" },
                        "discussion_id": { "type": "string" },
                        "rich_text": { "type": "array", "items": { "type": "object" } },
                        "workspace": workspace_param(),
                    },
                    "required": ["rich_text"]
                }),
            },
        ];

        let default_policies: Vec<SkillPolicy> = tools
            .iter()
            .map(|t| SkillPolicy {
                tool: t.name.clone(),
                action: "allow".to_string(),
            })
            .collect();

        SkillDefinition {
            name: "notion-integration".to_string(),
            version: "1.0.0".to_string(),
            description: "Interact with Notion — pages, databases, blocks, users, and comments across multiple workspaces.".to_string(),
            author: "workforce".to_string(),
            license: "MIT".to_string(),
            tools,
            event_sources: Vec::new(),
            default_policies,
            memory_schemas: Vec::new(),
            requirements: SkillRequirements {
                credentials: vec![CredentialRequirement {
                    name: "notion_token".to_string(),
                    r#type: "api_key".to_string(),
                    description: "Notion internal integration token (Bearer auth)".to_string(),
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

    #[test]
    fn read_and_write_tool_lists_cover_all_tools() {
        crate::adapters::test_support::assert_read_write_partition(
            NotionSkill::tool_names(),
            READ_TOOLS,
            WRITE_TOOLS,
        );
    }

    #[test]
    fn definition_lists_all_tools() {
        let def = NotionSkill::definition();
        assert_eq!(def.name, "notion-integration");
        assert!(def.tools.len() >= 17);
        let names: Vec<&str> = def.tools.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"notion.search"));
        assert!(names.contains(&"notion.page_create"));
        assert!(names.contains(&"notion.database_query"));
        assert!(names.contains(&"notion.block_children"));
        assert!(names.contains(&"notion.comment_create"));
        assert_eq!(def.default_policies.len(), def.tools.len());
    }

    #[test]
    fn new_errors_when_no_workspaces() {
        let err = NotionSkill::new(NotionSkillConfig::default())
            .err()
            .expect("expected error");
        assert!(err.to_string().contains("No Notion workspaces"));
    }

    #[test]
    fn a_stale_selector_is_ignored_and_a_sole_workspace_is_the_default() {
        let flat = NotionSkillConfig {
            token: Some("ntn_test".to_string()),
            default_workspace: Some("nope".to_string()),
            ..NotionSkillConfig::default()
        };
        assert_eq!(NotionSkill::new(flat).unwrap().default_workspace, "default");

        let named = NotionSkillConfig {
            workspaces: HashMap::from([(
                "prod".to_string(),
                NotionWorkspaceConfig {
                    token: "ntn_prod".to_string(),
                    max_results: 100,
                },
            )]),
            ..NotionSkillConfig::default()
        };
        assert_eq!(NotionSkill::new(named).unwrap().default_workspace, "prod");
    }

    #[test]
    fn new_succeeds_single_workspace() {
        let cfg = NotionSkillConfig {
            token: Some("ntn_test".to_string()),
            ..NotionSkillConfig::default()
        };
        let skill = NotionSkill::new(cfg).unwrap();
        assert!(skill.tools().len() >= 17);
    }

    #[test]
    fn tool_names_matches_registered_tools() {
        let cfg = NotionSkillConfig {
            token: Some("ntn_test".to_string()),
            ..NotionSkillConfig::default()
        };
        let skill = NotionSkill::new(cfg).unwrap();
        let registered: Vec<String> = skill.tools().iter().map(|t| t.name().to_string()).collect();
        let declared: Vec<String> = NotionSkill::tool_names()
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(
            registered, declared,
            "NotionSkill::tool_names() drifted from tools()"
        );
    }
}
