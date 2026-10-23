//! Agent management tools — let agents list, inspect, create, update, and delete peer agents.
//!
//! Tools:
//! - `agent.list`   — list the configured agents and whether each is running
//! - `agent.show`   — show a single agent by ID
//! - `agent.create` — create a new agent (with optional role template)
//! - `agent.update` — update an existing agent (partial merge)
//! - `agent.delete` — delete an agent from config

use std::path::PathBuf;
use std::sync::{Arc, Weak};
use std::time::Instant;

use async_trait::async_trait;
use serde::Deserialize;
use tracing::info;

use workforce_agent::tools::{Tool, ToolResult};
use workforce_common::locking::LockManager;
use workforce_common::settings::{AgentConfig, WorkforceConfig};

use crate::hosts::NotServing;

// ─── Agent Tool Context ─────────────────────────────────────────────────────

/// Shared context for agent management tools.
#[derive(Clone)]
pub struct AgentToolContext {
    pub config_path: Option<PathBuf>,
    pub config: WorkforceConfig,
    pub data_dir: String,
    pub lock_manager: Arc<LockManager>,
    pub agent_id: String,
    /// Revision store for `config_overlay` history (`None` without Postgres).
    pub def_store: Option<Arc<workforce_orchestrator::workflow_store::WorkflowStore>>,
    /// The live registry, set once the agent joins it. `agent.list` and
    /// `agent.show` read it, so an agent created after this one was built
    /// is found.
    pub registry: crate::hosts::RegistryCell,
}

/// A configured agent and whether it serves.
struct ListedAgent {
    config: AgentConfig,
    status: AgentStatus,
}

enum AgentStatus {
    Running,
    NotServing(NotServing),
    /// Read from the build-time config, before the registry is attached.
    Configured,
}

impl ListedAgent {
    /// `fields` with the agent's `status`, and why it failed to load.
    fn json(&self, mut fields: serde_json::Value) -> serde_json::Value {
        let (status, load_error) = match &self.status {
            AgentStatus::Running => ("running", None),
            AgentStatus::NotServing(NotServing::Starting) => ("starting", None),
            AgentStatus::NotServing(NotServing::Failed(reason)) => ("failed", Some(reason)),
            AgentStatus::Configured => ("configured", None),
        };
        fields["status"] = status.into();
        if let Some(reason) = load_error {
            fields["load_error"] = reason.as_str().into();
        }
        fields
    }
}

impl AgentToolContext {
    /// Every configured agent, sorted by id: the running ones, each read
    /// from its own service, which holds the entry it was (re)spawned with,
    /// and the ones starting or failed to load. Before the registry is
    /// attached, the config this tool was built with.
    fn agents(&self) -> Vec<ListedAgent> {
        let Some(registry) = self.registry.get().and_then(Weak::upgrade) else {
            return self
                .config
                .agents
                .iter()
                .map(|config| ListedAgent {
                    config: config.clone(),
                    status: AgentStatus::Configured,
                })
                .collect();
        };
        let running = registry.all_services().into_iter().filter_map(|svc| {
            let id = svc.agent_id.as_str();
            svc.config
                .agents
                .iter()
                .chain(&self.config.agents)
                .find(|a| a.id == id)
                .map(|config| ListedAgent {
                    config: config.clone(),
                    status: AgentStatus::Running,
                })
        });
        let unloaded = registry.unloaded().into_iter().map(|u| ListedAgent {
            config: u.config,
            status: AgentStatus::NotServing(u.state),
        });
        let mut agents: Vec<ListedAgent> = running.chain(unloaded).collect();
        agents.sort_by(|a, b| a.config.id.cmp(&b.config.id));
        agents.dedup_by(|later, earlier| later.config.id == earlier.config.id);
        agents
    }
}

/// The identity the current turn runs as (empty when unbound) — recorded as
/// the revision author for config writes made through these tools.
fn current_turn_user() -> Option<String> {
    workforce_agent::runner::CURRENT_USER_ID
        .try_with(|u| u.clone())
        .ok()
        .filter(|u| !u.is_empty())
}

/// Create all agent management tools.
pub fn agent_tools(ctx: AgentToolContext) -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(AgentListTool::new(ctx.clone())),
        Arc::new(AgentShowTool::new(ctx.clone())),
        Arc::new(AgentCreateTool::new(ctx.clone())),
        Arc::new(AgentUpdateTool::new(ctx.clone())),
        Arc::new(AgentDeleteTool::new(ctx)),
    ]
}

// ─── agent.list ─────────────────────────────────────────────────────────────

pub struct AgentListTool {
    ctx: AgentToolContext,
}

impl AgentListTool {
    pub fn new(ctx: AgentToolContext) -> Self {
        Self { ctx }
    }
}

#[async_trait]
impl Tool for AgentListTool {
    fn name(&self) -> &str {
        "agent.list"
    }

    fn description(&self) -> &str {
        "List the configured agents, including ones created since this agent started, each \
         with its status: running, starting, or failed (with the load error)"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {}
        ,
            "required": []})
    }

    async fn execute(&self, _input: serde_json::Value) -> eyre::Result<ToolResult> {
        let start = Instant::now();

        let agents_json: Vec<serde_json::Value> = self
            .ctx
            .agents()
            .iter()
            .map(|listed| {
                let a = &listed.config;
                listed.json(serde_json::json!({
                    "id": a.id,
                    "name": a.name,
                    "model": a.model,
                    "default": a.default,
                    "capabilities": a.capabilities,
                }))
            })
            .collect();

        Ok(ToolResult::success(
            serde_json::json!({ "agents": agents_json }),
            start.elapsed().as_millis() as u64,
        ))
    }
}

// ─── agent.show ─────────────────────────────────────────────────────────────

pub struct AgentShowTool {
    ctx: AgentToolContext,
}

impl AgentShowTool {
    pub fn new(ctx: AgentToolContext) -> Self {
        Self { ctx }
    }
}

#[derive(Deserialize)]
struct AgentShowInput {
    agent_id: String,
}

#[async_trait]
impl Tool for AgentShowTool {
    fn name(&self) -> &str {
        "agent.show"
    }

    fn description(&self) -> &str {
        "Show the full details and status of a single agent by its ID (an ID differing only \
         in case from exactly one agent's finds that agent)."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "agent_id": {
                    "type": "string",
                    "description": "The unique agent identifier"
                }
            },
            "required": ["agent_id"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> eyre::Result<ToolResult> {
        let start = Instant::now();
        let params: AgentShowInput = serde_json::from_value(input)?;
        let duration = || start.elapsed().as_millis() as u64;

        let agents = self.ctx.agents();
        let ids: Vec<String> = agents.iter().map(|a| a.config.id.clone()).collect();
        let found = crate::agent_registry::canonical_agent_id(&params.agent_id, &ids)
            .and_then(|id| agents.iter().find(|a| a.config.id == id));
        match found {
            Some(listed) => {
                let agent = &listed.config;
                Ok(ToolResult::success(
                    listed.json(serde_json::json!({
                        "id": agent.id,
                        "name": agent.name,
                        "model": agent.model,
                        "workspace": agent.workspace,
                        "system_prompt": agent.system_prompt,
                        "capabilities": agent.capabilities,
                        "default": agent.default,
                    })),
                    duration(),
                ))
            }
            None => Ok(ToolResult::failure(
                format!("Agent '{}' not found", params.agent_id),
                duration(),
            )),
        }
    }
}

// ─── agent.create ───────────────────────────────────────────────────────────

pub struct AgentCreateTool {
    ctx: AgentToolContext,
}

impl AgentCreateTool {
    pub fn new(ctx: AgentToolContext) -> Self {
        Self { ctx }
    }
}

#[derive(Deserialize)]
struct AgentCreateInput {
    id: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    system_prompt: Option<String>,
    #[serde(default)]
    capabilities: Option<Vec<String>>,
    #[serde(default)]
    default: Option<bool>,
}

#[async_trait]
impl Tool for AgentCreateTool {
    fn name(&self) -> &str {
        "agent.create"
    }

    fn description(&self) -> &str {
        "Create a new agent"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "id": {
                    "type": "string",
                    "description": "Unique agent identifier"
                },
                "name": {
                    "type": "string",
                    "description": "Human-readable display name"
                },
                "role": {
                    "type": "string",
                    "description": "Role template ID to inherit system_prompt and"
                },
                "model": {
                    "type": "string",
                    "description": "LLM model override (e.g. 'claude-sonnet-4-5')"
                },
                "system_prompt": {
                    "type": "string",
                    "description": "System prompt override (takes precedence over role template)"
                },
                "capabilities": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Agent capability tags (takes precedence over role template)"
                },
                "default": {
                    "type": "boolean",
                    "description": "Whether this agent should be the default"
                }
            },
            "required": ["id"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> eyre::Result<ToolResult> {
        let start = Instant::now();
        let params: AgentCreateInput = serde_json::from_value(input)?;
        let duration = || start.elapsed().as_millis() as u64;

        let config_path = match &self.ctx.config_path {
            Some(p) => p.clone(),
            None => {
                return Ok(ToolResult::failure(
                    "No config path available — cannot persist agent changes",
                    duration(),
                ));
            }
        };

        // Fresh-load config from disk (base ⊕ overlay)
        let config = match WorkforceConfig::load(&config_path) {
            Ok(c) => c,
            Err(e) => {
                return Ok(ToolResult::failure(
                    format!("Failed to load config: {}", e),
                    duration(),
                ));
            }
        };

        // The id names the agent's workspace directory.
        if !workforce_common::config_files::is_plain_name(&params.id) {
            return Ok(ToolResult::failure(
                format!("'{}' is not an agent id", params.id),
                duration(),
            ));
        }

        // Validate ID is unique
        if config.agents.iter().any(|a| a.id == params.id) {
            return Ok(ToolResult::failure(
                format!("Agent '{}' already exists", params.id),
                duration(),
            ));
        }

        // Optionally load role template
        let (role_prompt, role_caps) = if let Some(ref role_id) = params.role {
            match workforce_common::roles::load_role(&self.ctx.data_dir, role_id) {
                Ok(role) => (Some(role.system_prompt), role.capabilities),
                Err(e) => {
                    return Ok(ToolResult::failure(
                        format!("Failed to load role '{}': {}", role_id, e),
                        duration(),
                    ));
                }
            }
        } else {
            (None, Vec::new())
        };

        // Build workspace path
        let workspace = format!("{}/agents/{}", self.ctx.data_dir, params.id);

        // Create workspace directory
        if let Err(e) = std::fs::create_dir_all(&workspace) {
            return Ok(ToolResult::failure(
                format!("Failed to create workspace directory: {}", e),
                duration(),
            ));
        }

        // Raw overlay entry — only the provided fields, unexpanded values.
        // The base workforce.toml is infra-owned; runtime agent creation
        // lives in the overlay.
        let mut agent = toml::Table::new();
        agent.insert("id".to_string(), toml::Value::String(params.id.clone()));
        if let Some(ref name) = params.name {
            agent.insert("name".to_string(), toml::Value::String(name.clone()));
        }
        if let Some(model) = params.model {
            agent.insert("model".to_string(), toml::Value::String(model));
        }
        agent.insert("workspace".to_string(), toml::Value::String(workspace));
        if let Some(prompt) = params.system_prompt.or(role_prompt) {
            agent.insert("system_prompt".to_string(), toml::Value::String(prompt));
        }
        let capabilities = params.capabilities.unwrap_or(role_caps);
        if !capabilities.is_empty() {
            agent.insert(
                "capabilities".to_string(),
                toml::Value::Array(capabilities.into_iter().map(toml::Value::String).collect()),
            );
        }
        if params.default.unwrap_or(false) {
            agent.insert("default".to_string(), toml::Value::Boolean(true));
        }

        let user = current_turn_user();
        if let Err(e) = crate::config_writer::overlay_upsert_agent(
            &config_path,
            std::path::Path::new(&self.ctx.data_dir),
            agent,
            user.as_deref(),
            "agent_tool",
            self.ctx.def_store.as_deref(),
            &self.ctx.lock_manager,
        )
        .await
        {
            return Ok(ToolResult::failure(
                format!("Failed to save config: {}", e),
                duration(),
            ));
        }

        info!(agent_id = %params.id, "Agent created via agent.create (overlay)");
        Ok(ToolResult::success(
            serde_json::json!({
                "created": true,
                "id": params.id,
                "name": params.name,
                "live_within_seconds": true,
            }),
            duration(),
        ))
    }
}

// ─── agent.update ───────────────────────────────────────────────────────────

pub struct AgentUpdateTool {
    ctx: AgentToolContext,
}

impl AgentUpdateTool {
    pub fn new(ctx: AgentToolContext) -> Self {
        Self { ctx }
    }
}

#[derive(Deserialize)]
struct AgentUpdateInput {
    agent_id: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    system_prompt: Option<String>,
    #[serde(default)]
    default: Option<bool>,
}

#[async_trait]
impl Tool for AgentUpdateTool {
    fn name(&self) -> &str {
        "agent.update"
    }

    fn description(&self) -> &str {
        "Update an existing agent"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "agent_id": {
                    "type": "string",
                    "description": "The ID of the agent to update"
                },
                "name": {
                    "type": "string",
                    "description": "New display name"
                },
                "model": {
                    "type": "string",
                    "description": "New LLM model override"
                },
                "system_prompt": {
                    "type": "string",
                    "description": "New system prompt"
                },
                "default": {
                    "type": "boolean",
                    "description": "Whether this agent should be the default"
                }
            },
            "required": ["agent_id"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> eyre::Result<ToolResult> {
        let start = Instant::now();
        let params: AgentUpdateInput = serde_json::from_value(input)?;
        let duration = || start.elapsed().as_millis() as u64;

        let config_path = match &self.ctx.config_path {
            Some(p) => p.clone(),
            None => {
                return Ok(ToolResult::failure(
                    "No config path available — cannot persist agent changes",
                    duration(),
                ));
            }
        };

        // Update only the provided fields on the agent's RAW entry (taken
        // from overlay/base tables, never the env-expanded struct) and write
        // it to the overlay as a per-entity replacement.
        let AgentUpdateInput {
            agent_id,
            name,
            model,
            system_prompt,
            default,
        } = params;
        let user = current_turn_user();
        if let Err(e) = crate::config_writer::overlay_update_agent_fields(
            &config_path,
            std::path::Path::new(&self.ctx.data_dir),
            &agent_id,
            move |entry| {
                if let Some(name) = name {
                    entry.insert("name".to_string(), toml::Value::String(name));
                }
                if let Some(model) = model {
                    entry.insert("model".to_string(), toml::Value::String(model));
                }
                if let Some(system_prompt) = system_prompt {
                    entry.insert(
                        "system_prompt".to_string(),
                        toml::Value::String(system_prompt),
                    );
                }
                if let Some(default) = default {
                    entry.insert("default".to_string(), toml::Value::Boolean(default));
                }
            },
            user.as_deref(),
            "agent_tool",
            self.ctx.def_store.as_deref(),
            &self.ctx.lock_manager,
        )
        .await
        {
            return Ok(ToolResult::failure(
                format!("Failed to save config: {}", e),
                duration(),
            ));
        }

        info!(agent_id = %agent_id, "Agent updated via agent.update (overlay)");
        Ok(ToolResult::success(
            serde_json::json!({
                "updated": true,
                "id": agent_id,
                "live_within_seconds": true,
            }),
            duration(),
        ))
    }
}

// ─── agent.delete ───────────────────────────────────────────────────────────

pub struct AgentDeleteTool {
    ctx: AgentToolContext,
}

impl AgentDeleteTool {
    pub fn new(ctx: AgentToolContext) -> Self {
        Self { ctx }
    }
}

#[derive(Deserialize)]
struct AgentDeleteInput {
    agent_id: String,
}

#[async_trait]
impl Tool for AgentDeleteTool {
    fn name(&self) -> &str {
        "agent.delete"
    }

    fn description(&self) -> &str {
        "Delete an agent from the workforce config by its ID."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "agent_id": {
                    "type": "string",
                    "description": "The ID of the agent to delete"
                }
            },
            "required": ["agent_id"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> eyre::Result<ToolResult> {
        let start = Instant::now();
        let params: AgentDeleteInput = serde_json::from_value(input)?;
        let duration = || start.elapsed().as_millis() as u64;

        let config_path = match &self.ctx.config_path {
            Some(p) => p.clone(),
            None => {
                return Ok(ToolResult::failure(
                    "No config path available — cannot persist agent changes",
                    duration(),
                ));
            }
        };

        // Fresh-load config from disk (base ⊕ overlay) to verify existence.
        let config = match WorkforceConfig::load(&config_path) {
            Ok(c) => c,
            Err(e) => {
                return Ok(ToolResult::failure(
                    format!("Failed to load config: {}", e),
                    duration(),
                ));
            }
        };
        if !config.agents.iter().any(|a| a.id == params.agent_id) {
            return Ok(ToolResult::failure(
                format!("Agent '{}' not found", params.agent_id),
                duration(),
            ));
        }

        // Drop any overlay entry and tombstone the id in agents_remove so a
        // base-defined agent stays gone across infra deploys.
        let user = current_turn_user();
        if let Err(e) = crate::config_writer::overlay_remove_agent(
            &config_path,
            std::path::Path::new(&self.ctx.data_dir),
            &params.agent_id,
            user.as_deref(),
            "agent_tool",
            self.ctx.def_store.as_deref(),
            &self.ctx.lock_manager,
        )
        .await
        {
            return Ok(ToolResult::failure(
                format!("Failed to save config: {}", e),
                duration(),
            ));
        }

        info!(agent_id = %params.agent_id, "Agent deleted via agent.delete (overlay)");
        Ok(ToolResult::success(
            serde_json::json!({
                "deleted": true,
                "id": params.agent_id,
                "live_within_seconds": true,
            }),
            duration(),
        ))
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_registry::AgentRegistry;
    use crate::serve::Compose as _;

    fn test_ctx(agents: Vec<workforce_common::settings::AgentConfig>) -> AgentToolContext {
        let mut config = WorkforceConfig::default();
        config.agents = agents;
        AgentToolContext {
            config_path: None,
            config,
            data_dir: "/tmp/test-workforce".to_string(),
            lock_manager: Arc::new(LockManager::new()),
            agent_id: "test-caller".to_string(),
            def_store: None,
            registry: Arc::default(),
        }
    }

    fn sample_agent(
        id: &str,
        name: Option<&str>,
        default: bool,
    ) -> workforce_common::settings::AgentConfig {
        workforce_common::settings::AgentConfig {
            id: id.to_string(),
            name: name.map(|n| n.to_string()),
            model: Some("claude-sonnet-4-5".to_string()),
            workspace: None,
            system_prompt: None,
            capabilities: vec!["code".into(), "planning".into()],
            default,
            llm: None,
            vault: None,
            compaction: None,
            memory: None,
            knowledge: None,
            access: Default::default(),
            scope: None,
            user_map: Default::default(),
            owner_id: None,
            owner_type: Default::default(),
            advisor: None,
            advisor_mode: false,
        }
    }

    #[tokio::test]
    async fn list_returns_agents_from_config() {
        let ctx = test_ctx(vec![
            sample_agent("alice", Some("Alice"), true),
            sample_agent("bob", Some("Bob"), false),
        ]);
        let tool = AgentListTool::new(ctx);
        let result = tool.execute(serde_json::json!({})).await.unwrap();
        assert!(result.success);
        let agents = result.output["agents"].as_array().unwrap();
        assert_eq!(agents.len(), 2);
        assert_eq!(agents[0]["id"], "alice");
        assert_eq!(agents[0]["name"], "Alice");
        assert_eq!(agents[0]["default"], true);
        assert_eq!(
            agents[0]["capabilities"],
            serde_json::json!(["code", "planning"])
        );
        assert_eq!(agents[1]["id"], "bob");
    }

    #[tokio::test]
    async fn show_finds_agent_by_id() {
        let ctx = test_ctx(vec![
            sample_agent("alice", Some("Alice"), true),
            sample_agent("bob", Some("Bob"), false),
        ]);
        let tool = AgentShowTool::new(ctx);
        let result = tool
            .execute(serde_json::json!({ "agent_id": "bob" }))
            .await
            .unwrap();
        assert!(result.success);
        assert_eq!(result.output["id"], "bob");
        assert_eq!(result.output["name"], "Bob");
        assert_eq!(result.output["model"], "claude-sonnet-4-5");
        assert_eq!(result.output["default"], false);
    }

    #[tokio::test]
    async fn show_and_list_find_agents_created_later_and_those_not_running() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = WorkforceConfig::default();
        config.hub.data_dir = dir.path().to_string_lossy().to_string();
        // The default is a relative `data/memory.db` shared by every test that
        // leaves it; under cargo-nextest each test is its own process, and two
        // opening that one file at once fail with "database is locked".
        config.memory.db_path = dir.path().join("memory.db").to_string_lossy().into_owned();
        config.llm.api_key = Some("test-key".into());
        let mut services = std::collections::HashMap::new();
        for (id, name) in [("alice", "Alice"), ("Minny", "Minny the new one")] {
            let entry = sample_agent(id, Some(name), id == "alice");
            let mut config = config.clone();
            config.agents = vec![entry.clone()];
            let service = crate::serve::AgentService::build_for_agent(
                config.clone(),
                None,
                Some(&entry),
                None,
                Arc::new(LockManager::new()),
                None,
                None,
                None,
                None,
                crate::subscription::Subscriptions::new(&config),
            )
            .await
            .unwrap();
            services.insert(id.to_string(), service);
        }
        let registry = Arc::new(AgentRegistry::from_parts(services, "alice".to_string()));
        registry.load_failed(
            &sample_agent("broken", Some("Broken"), false),
            "unknown provider".to_string(),
        );
        registry.begin_loading(&sample_agent("Newbie", None, false));
        let ctx = test_ctx(vec![sample_agent("alice", Some("Alice"), true)]);
        let weak = Arc::downgrade(&registry);
        ctx.registry.set(weak).unwrap();

        let shown = AgentShowTool::new(ctx.clone())
            .execute(serde_json::json!({ "agent_id": "minny" }))
            .await
            .unwrap();
        assert!(shown.success, "{:?}", shown.error);
        assert_eq!(shown.output["id"], "Minny");
        assert_eq!(shown.output["name"], "Minny the new one");
        assert_eq!(shown.output["status"], "running");

        let broken = AgentShowTool::new(ctx.clone())
            .execute(serde_json::json!({ "agent_id": "broken" }))
            .await
            .unwrap();
        assert!(broken.success, "{:?}", broken.error);
        assert_eq!(broken.output["status"], "failed");
        assert_eq!(broken.output["load_error"], "unknown provider");

        let listed = AgentListTool::new(ctx)
            .execute(serde_json::json!({}))
            .await
            .unwrap();
        let statuses: Vec<(&str, &str)> = listed.output["agents"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| (a["id"].as_str().unwrap(), a["status"].as_str().unwrap()))
            .collect();
        assert_eq!(
            statuses,
            vec![
                ("Minny", "running"),
                ("Newbie", "starting"),
                ("alice", "running"),
                ("broken", "failed"),
            ]
        );
    }

    #[tokio::test]
    async fn show_returns_error_for_unknown_agent() {
        let ctx = test_ctx(vec![sample_agent("alice", Some("Alice"), true)]);
        let tool = AgentShowTool::new(ctx);
        let result = tool
            .execute(serde_json::json!({ "agent_id": "unknown" }))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(
            result.error.as_deref().unwrap_or("").contains("not found"),
            "Expected 'not found' error, got: {:?}",
            result.error
        );
    }

    #[tokio::test]
    async fn list_empty_config_returns_empty_array() {
        let ctx = test_ctx(vec![]);
        let tool = AgentListTool::new(ctx);
        let result = tool.execute(serde_json::json!({})).await.unwrap();
        assert!(result.success);
        let agents = result.output["agents"].as_array().unwrap();
        assert!(agents.is_empty());
    }

    // Admin gating for `agent.{create,update,delete}` is now enforced
    // pre-dispatch by the `HubCapabilityGate` (`agent.* → AgentsManage`) off the
    // `tool_capability` registry, not by an inline check here. The
    // member-denied / admin-allowed adversarial coverage lives in
    // `crate::authz` and `crate::cap_gate`; `agent.list`/`agent.show` stay
    // member-accessible (unregistered) and are covered above.
}
