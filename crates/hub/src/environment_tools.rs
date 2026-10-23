//! Agent-facing environment tools — the SAFE directions only.
//!
//! The agent can read the live state board ([`EnvStatusTool`]) and its own
//! environments ([`EnvListTool`]), create a draft ([`EnvCreateTool`]), bind
//! skills and MCP workspaces in a draft ([`EnvAddTool`]), and delete a draft
//! ([`EnvDeleteTool`]). A draft is never active for any session until a person
//! activates it, so nothing the agent writes changes what it can reach.

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use eyre::Result;
use serde::Deserialize;
use serde_json::{json, Value};

use workforce_agent::tools::{Tool, ToolResult};
use workforce_orchestrator::workflow_store::EditContext;
use workforce_policy::rules::{PolicyRule, PolicyScope, RuleAction};

use crate::environments::{
    self, check_mcp_binding, parse_env_target, validate_skill_binding, EnvTarget,
};
use crate::hub_core::environments::ResolvedEnvironments;
use crate::serve::AgentService;

/// Register the agent-facing environment tools on `service`. They are hidden
/// while the agent has environments off (see
/// [`environments::EnvironmentToolAccess`]).
pub fn register_environment_tools(service: &Arc<AgentService>) {
    let service = || Arc::clone(service);
    service().register_tool(Arc::new(EnvStatusTool(service())));
    service().register_tool(Arc::new(EnvListTool(service())));
    service().register_tool(Arc::new(EnvCreateTool(service())));
    service().register_tool(Arc::new(EnvDeleteTool(service())));
    service().register_tool(Arc::new(EnvAddTool(service())));
}

/// Allow-by-default policies for the agent-facing environment tools.
pub fn default_policies() -> Vec<PolicyRule> {
    environments::TOOL_NAMES
        .iter()
        .map(|tool| PolicyRule {
            name: format!("{tool}-allow-default"),
            scope: PolicyScope::Global,
            tool: (*tool).to_string(),
            action: RuleAction::Allow,
            conditions: Vec::new(),
            priority: 10,
            constitutional: true,
        })
        .collect()
}

fn agent_edit(actor: Option<&str>) -> EditContext<'_> {
    EditContext::new("agent_tool", actor)
}

/// This agent's environments, or the refusal while it has them off.
fn environments_of(service: &AgentService) -> Result<ResolvedEnvironments, String> {
    let environments = service.resolved.snapshot().environments.clone();
    if environments.enabled {
        Ok(environments)
    } else {
        Err("Environments are off for this agent.".to_string())
    }
}

fn finish(started: Instant, outcome: Result<Value, String>) -> ToolResult {
    let ms = started.elapsed().as_millis() as u64;
    match outcome {
        Ok(output) => ToolResult::success(output, ms),
        Err(reason) => ToolResult::failure(reason, ms),
    }
}

fn parsed<T: serde::de::DeserializeOwned>(input: Value) -> Result<T, String> {
    serde_json::from_value(input).map_err(|e| e.to_string())
}

// ── env.status ─────────────────────────────────────────────────

struct EnvStatusTool(Arc<AgentService>);

#[async_trait]
impl Tool for EnvStatusTool {
    fn name(&self) -> &str {
        "env.status"
    }
    fn description(&self) -> &str {
        "Show the live environment state board: which environment is active (and this agent's default), what each skill (the data adapters AND databases — member name `database`) and MCP workspace it binds resolves to RIGHT NOW, and — joined from the live MCP manager — whether each referenced MCP server is connected, installed-but-not-authorised, or absent, with the exact fix for anything unreachable. ALWAYS call this before answering a question about which environment is active, which MCP servers are connected/authorised, or why an MCP tool isn't working — it is ground truth. Do NOT guess, do NOT claim a server was uninstalled without checking here, and never offer to modify the database. If a server shows `installed, NOT authorised`, the fix is `mcp.authorize <server>`. You cannot switch the active environment yourself — a human runs `cmd: env use <name>`."
    }
    fn input_schema(&self) -> Value {
        json!({"type": "object", "properties": {}, "additionalProperties": false})
    }
    async fn execute(&self, _input: Value) -> Result<ToolResult> {
        let started = Instant::now();
        let outcome = environments_of(&self.0).map(|environments| {
            let active = workforce_agent::runner::current_active_environment();
            let statuses = self
                .0
                .mcp_manager()
                .map(|m| m.server_statuses())
                .unwrap_or_default();
            json!({
                "board": crate::commands::render_env_board(
                    environments.active(active.as_deref()),
                    environments.default.as_deref(),
                    &self.0.tools.skill_variants(),
                    &statuses,
                )
            })
        });
        Ok(finish(started, outcome))
    }
}

// ── env.list ───────────────────────────────────────────────────

struct EnvListTool(Arc<AgentService>);

#[async_trait]
impl Tool for EnvListTool {
    fn name(&self) -> &str {
        "env.list"
    }
    fn description(&self) -> &str {
        "List this agent's environments and, for each, the skills it binds (each to an ordered set of instances, the first being the one a call naming none gets) and MCP workspaces it binds (each to one variant), whether it is the default, and WARNINGS naming multi-instance skills (including databases) it leaves unbound, which pick any of their instances per call. You cannot switch the active environment — a human runs `cmd: env use <name>`."
    }
    fn input_schema(&self) -> Value {
        json!({"type": "object", "properties": {}, "additionalProperties": false})
    }
    async fn execute(&self, _input: Value) -> Result<ToolResult> {
        let started = Instant::now();
        let outcome = environments_of(&self.0).map(|environments| {
            json!({
                "environments": environments::environments_json(
                    &environments.defs,
                    environments.default.as_deref(),
                    &self.0.tools.skill_variants(),
                ),
            })
        });
        Ok(finish(started, outcome))
    }
}

// ── env.create ─────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct EnvCreateInput {
    name: String,
    #[serde(default)]
    description: Option<String>,
}

struct EnvCreateTool(Arc<AgentService>);

#[async_trait]
impl Tool for EnvCreateTool {
    fn name(&self) -> &str {
        "env.create"
    }
    fn description(&self) -> &str {
        "Create a new, EMPTY draft environment for this agent (a named bundle that binds skills + MCP workspaces to one coherent target, e.g. all-prod). A draft is not active for anyone — you fill it with `env.add`, and a human reviews it and activates it with `cmd: env use <name>` or makes it the default with `cmd: env default <name>`. Safe to call: creating a draft never changes what is currently active."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["name"],
            "properties": {
                "name": {"type": "string", "description": "Environment name, e.g. \"harbor-prod\"."},
                "description": {"type": "string", "description": "Optional human-readable description."}
            },
            "additionalProperties": false
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let started = Instant::now();
        let outcome = async {
            environments_of(&self.0)?;
            let input: EnvCreateInput = parsed(input)?;
            let actor = crate::user_config::current_identity_id();
            environments::update(
                &self.0,
                agent_edit(actor.as_deref()),
                environments::create(&input.name, input.description.clone(), true),
            )
            .await
            .map_err(|e| e.to_string())?;
            let name = input.name.trim();
            Ok(json!({
                "name": name,
                "default": false,
                "draft": true,
                "note": format!(
                    "Created draft environment `{name}`. Add bindings with `env.add`, then ask a \
                     human to activate it (`cmd: env use {name}`) or make it the default \
                     (`cmd: env default {name}`)."
                ),
            }))
        }
        .await;
        Ok(finish(started, outcome))
    }
}

// ── env.delete ─────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct EnvDeleteInput {
    name: String,
}

struct EnvDeleteTool(Arc<AgentService>);

#[async_trait]
impl Tool for EnvDeleteTool {
    fn name(&self) -> &str {
        "env.delete"
    }
    fn description(&self) -> &str {
        "Delete one of this agent's DRAFT environments (one made with `env.create` that no human has activated yet). You CANNOT delete an environment a human activated: that can change what is active, so it is human-only (`cmd: env delete <name>` or the admin UI)."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["name"],
            "properties": {
                "name": {"type": "string", "description": "Name of a draft environment to delete."}
            },
            "additionalProperties": false
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let started = Instant::now();
        let outcome = async {
            environments_of(&self.0)?;
            let input: EnvDeleteInput = parsed(input)?;
            let name = input.name.as_str();
            let actor = crate::user_config::current_identity_id();
            let delete = environments::delete(name);
            environments::update(&self.0, agent_edit(actor.as_deref()), |envs| {
                environments::only_draft(&envs, name)?;
                delete(envs)
            })
            .await
            .map_err(|e| e.to_string())?;
            Ok(json!({"deleted": name, "note": format!("Deleted environment `{name}`.")}))
        }
        .await;
        Ok(finish(started, outcome))
    }
}

// ── env.add ────────────────────────────────────────────────────

type Envs = workforce_common::settings::AgentEnvironmentsConfig;
type Transform = Box<dyn Fn(Envs) -> Result<Envs, String> + Send + Sync>;

#[derive(Debug, Deserialize)]
struct EnvAddInput {
    environment: String,
    target: String,
    member: String,
    #[serde(default)]
    instances: Vec<String>,
    #[serde(default)]
    variant: Option<String>,
}

struct EnvAddTool(Arc<AgentService>);

#[async_trait]
impl Tool for EnvAddTool {
    fn name(&self) -> &str {
        "env.add"
    }
    fn description(&self) -> &str {
        "Bind a skill or an MCP workspace in one of this agent's DRAFT environments. `target` \"skill\": `instances` is the ordered set of the skill's instances the environment allows, the first being the one a call naming none gets; it replaces any set the skill had. Multi-instance skills include the data adapters (e.g. linear, metabase, jira) AND databases (`member: database`). `target` \"mcp\": `variant` is the one variant of the workspace. You CANNOT edit an environment a human activated — that can change what is live, so it is human-only (`cmd: env add …` or the admin UI). Bindings are validated against this agent's loaded instances; an MCP workspace must have a server installed for that variant (installed but not yet authorised is allowed, with a note)."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["environment", "target", "member"],
            "properties": {
                "environment": {"type": "string", "description": "Name of a draft environment."},
                "target": {"type": "string", "enum": ["skill", "mcp"]},
                "member": {"type": "string", "description": "Skill name (e.g. \"linear\", \"metabase\", \"database\") or MCP workspace name."},
                "instances": {"type": "array", "items": {"type": "string"}, "description": "For a skill: its allowed instances, default first."},
                "variant": {"type": "string", "description": "For an MCP workspace: the variant to bind."}
            },
            "additionalProperties": false
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let started = Instant::now();
        let outcome = async {
            environments_of(&self.0)?;
            let input: EnvAddInput = parsed(input)?;
            let env = input.environment.as_str();
            let target = parse_env_target(&input.target).ok_or_else(|| {
                format!(
                    "Unknown target `{}` — use \"skill\" or \"mcp\".",
                    input.target
                )
            })?;
            let (bind, binding, note): (Transform, String, Option<String>) = match target {
                EnvTarget::Skill => {
                    validate_skill_binding(
                        &self.0.tools.skill_variants(),
                        &input.member,
                        &input.instances,
                    )?;
                    (
                        Box::new(environments::bind_skill(
                            env,
                            &input.member,
                            input.instances.clone(),
                        )),
                        input.instances.join(" "),
                        None,
                    )
                }
                EnvTarget::McpWorkspace => {
                    let variant = input
                        .variant
                        .as_deref()
                        .ok_or("An MCP binding needs a `variant`.")?;
                    let note =
                        check_mcp_binding(self.0.mcp_manager().as_deref(), &input.member, variant)?;
                    (
                        Box::new(environments::bind_mcp(env, &input.member, variant)),
                        variant.to_string(),
                        note,
                    )
                }
            };
            let actor = crate::user_config::current_identity_id();
            environments::update(&self.0, agent_edit(actor.as_deref()), |envs| {
                environments::only_draft(&envs, env)?;
                bind(envs)
            })
            .await
            .map_err(|e| e.to_string())?;
            let added =
                "Binding added to the draft. Ask a human to activate it or make it the default.";
            Ok(json!({
                "environment": env,
                "member": input.member,
                "binding": binding,
                "note": note.map_or_else(|| added.to_string(), |n| format!("{added}\n\n{n}")),
            }))
        }
        .await;
        Ok(finish(started, outcome))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use workforce_agent::llm::environment::SkillVariants;

    async fn service() -> Arc<AgentService> {
        let service = crate::environments::test_service().await;
        service.tools.set_instance_source(
            "linear",
            Arc::new(
                SkillVariants::of(
                    ["driftwerk".to_string(), "fabrikam-crm".to_string()],
                    "driftwerk",
                )
                .unwrap(),
            ),
        );
        service
    }

    async fn turned_on(service: &AgentService, default: Option<&str>) {
        let default = default.map(str::to_string);
        environments::update(service, EditContext::new("test", None), move |mut envs| {
            envs.enabled = Some(true);
            envs.defs.entry("work".to_string()).or_default();
            envs.defs.entry("scratch".to_string()).or_default().draft = true;
            envs.default = default.clone();
            Ok(envs)
        })
        .await
        .unwrap();
    }

    async fn run(tool: &dyn Tool, input: Value) -> ToolResult {
        tool.execute(input).await.unwrap()
    }

    #[tokio::test]
    async fn every_tool_refuses_while_environments_are_off() {
        let service = service().await;
        let result = run(&EnvListTool(Arc::clone(&service)), json!({})).await;
        assert!(!result.success);
        assert!(result.error.unwrap().contains("off for this agent"));
    }

    #[tokio::test]
    async fn add_binds_a_set_on_a_draft_and_refuses_the_default() {
        let service = service().await;
        turned_on(&service, Some("work")).await;
        let add = EnvAddTool(Arc::clone(&service));
        let ok = run(
            &add,
            json!({"environment": "scratch", "target": "skill", "member": "linear",
                   "instances": ["fabrikam-crm", "driftwerk"]}),
        )
        .await;
        assert!(ok.success, "{:?}", ok.error);
        assert_eq!(
            service.resolved.snapshot().environments.defs["scratch"].skills["linear"],
            vec!["fabrikam-crm", "driftwerk"]
        );
        let live = run(
            &add,
            json!({"environment": "work", "target": "skill", "member": "linear",
                   "instances": ["driftwerk"]}),
        )
        .await;
        assert!(live.error.unwrap().contains("human-only"));
        let unknown = run(
            &add,
            json!({"environment": "scratch", "target": "skill", "member": "linear",
                   "instances": ["acme"]}),
        )
        .await;
        assert!(unknown.error.unwrap().contains("`acme`"));
    }

    #[tokio::test]
    async fn delete_removes_a_draft_and_refuses_an_activated_environment() {
        let service = service().await;
        turned_on(&service, None).await;
        let delete = EnvDeleteTool(Arc::clone(&service));
        let activated = run(&delete, json!({"name": "work"})).await;
        assert!(activated.error.unwrap().contains("human-only"));
        let ok = run(&delete, json!({"name": "scratch"})).await;
        assert!(ok.success, "{:?}", ok.error);
        assert!(!service
            .resolved
            .snapshot()
            .environments
            .defs
            .contains_key("scratch"));
        let missing = run(&delete, json!({"name": "ghost"})).await;
        assert!(missing.error.unwrap().contains("No environment named"));
    }

    #[tokio::test]
    async fn a_name_the_agent_recreates_is_a_draft_no_session_adopts() {
        let service = service().await;
        turned_on(&service, Some("work")).await;
        environments::update(
            &service,
            EditContext::new("test", Some("admin")),
            environments::create("staging", None, false),
        )
        .await
        .unwrap();
        environments::update(
            &service,
            EditContext::new("test", Some("admin")),
            environments::delete("staging"),
        )
        .await
        .unwrap();
        let created = run(
            &EnvCreateTool(Arc::clone(&service)),
            json!({"name": "staging"}),
        )
        .await;
        assert!(created.success, "{:?}", created.error);
        let snapshot = service.resolved.snapshot();
        assert_eq!(
            snapshot.environments.active(Some("staging")).unwrap().name,
            "work"
        );
    }

    #[tokio::test]
    async fn create_adds_a_draft_that_is_not_the_default() {
        let service = service().await;
        turned_on(&service, Some("work")).await;
        let created = run(
            &EnvCreateTool(Arc::clone(&service)),
            json!({"name": "next"}),
        )
        .await;
        assert!(created.success, "{:?}", created.error);
        let envs = service.resolved.snapshot().environments.clone();
        assert!(envs.defs["next"].draft);
        assert_eq!(envs.default.as_deref(), Some("work"));
    }

    #[tokio::test]
    async fn list_shows_only_this_agents_environments() {
        let mine = service().await;
        let theirs = service().await;
        turned_on(&mine, Some("work")).await;
        environments::update(&theirs, EditContext::new("test", None), |mut envs| {
            envs.enabled = Some(true);
            envs.defs.entry("private-project".to_string()).or_default();
            Ok(envs)
        })
        .await
        .unwrap();
        let listed = run(&EnvListTool(Arc::clone(&mine)), json!({})).await;
        let names: Vec<&str> = listed.output["environments"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["scratch", "work"]);
    }
}
