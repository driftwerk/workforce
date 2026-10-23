//! An agent's own environments: named bundles in its `config.toml` that pin
//! its multi-instance skills to sets of instances and its MCP workspaces to one
//! variant each. Nothing here is shared between agents: definitions, the
//! default and the instance catalogue all come from the one agent.
//!
//! Activation is a macro: [`ResolvedEnvironments::scope`] expands the active
//! definition into the task-locals the runner scopes around a turn
//! (`ACTIVE_ENVIRONMENT`, `ACTIVE_SKILL_INSTANCES`, `ACTIVE_MCP_ENVS`).

mod edit;
mod migrate;

pub use edit::{
    bind_mcp, bind_skill, clear_default, create, delete, only_draft, promote, set_default,
    unbind_mcp, unbind_skill, update, write, EditError,
};
pub use migrate::{adopt_legacy_environments, migrate_legacy_environments};

#[cfg(test)]
use crate::serve::Compose as _;
use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::{json, Value};
use workforce_agent::llm::environment::SkillVariants;
use workforce_agent::tools::access::{ToolAccess, ToolAccessSnapshot};
use workforce_common::settings::{AgentEnvironmentsConfig, EnvironmentDef};

use crate::hosts::McpHost;
pub use crate::hub_core::environments::{Active, EnvironmentScope, ResolvedEnvironments};
use crate::resolved_config::ConfigCell;
use crate::serve::AgentService;

/// The agent-facing environment tools, hidden while the agent has
/// environments off.
pub const TOOL_NAMES: [&str; 5] = [
    "env.status",
    "env.list",
    "env.create",
    "env.add",
    "env.delete",
];

/// What a binding pins: an MCP workspace or a multi-instance skill.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnvTarget {
    McpWorkspace,
    Skill,
}

/// Parse a binding target keyword: `skill`, or `mcp` / `workspace` /
/// `mcp_workspace` for an MCP workspace.
pub fn parse_env_target(token: &str) -> Option<EnvTarget> {
    match token.to_lowercase().as_str() {
        "skill" => Some(EnvTarget::Skill),
        "mcp" | "workspace" | "mcp_workspace" => Some(EnvTarget::McpWorkspace),
        _ => None,
    }
}

/// Validate a skill binding against the agent's own instance catalogue.
pub fn validate_skill_binding(
    catalogue: &BTreeMap<String, SkillVariants>,
    skill: &str,
    instances: &[String],
) -> Result<(), String> {
    let Some(variants) = catalogue.get(skill) else {
        return Err(format!(
            "`{skill}` is not a multi-instance skill this agent has loaded."
        ));
    };
    if instances.is_empty() {
        return Err(format!("Name at least one instance of `{skill}`."));
    }
    let missing: Vec<&str> = instances
        .iter()
        .map(String::as_str)
        .filter(|instance| !variants.has(instance))
        .collect();
    if !missing.is_empty() {
        return Err(format!(
            "`{skill}` has no instance {}. Its instances are {}.",
            ticked(&missing),
            ticked(
                &variants
                    .instances
                    .iter()
                    .map(String::as_str)
                    .collect::<Vec<_>>()
            )
        ));
    }
    Ok(())
}

/// Validate an MCP workspace binding with installed-awareness: connected is
/// clean; installed but not connected (awaiting authorisation) is allowed with
/// a note; neither is refused, since the binding would point at nothing.
pub fn validate_mcp_binding(
    workspace: &str,
    variant: &str,
    connected: bool,
    installed: bool,
) -> Result<Option<String>, String> {
    if connected {
        Ok(None)
    } else if installed {
        Ok(Some(format!(
            "Note: the MCP server for workspace `{workspace}` / `{variant}` is installed but not \
             connected yet (e.g. awaiting authorisation). The binding is saved and will resolve \
             once it connects — check `env.status` and run `mcp.authorize <server>` if needed."
        )))
    } else {
        Err(format!(
            "No MCP server is installed for workspace `{workspace}` / `{variant}`. Install one \
             (it must register into workspace `{workspace}` with variant `{variant}`) before \
             binding it — otherwise the environment would point at nothing."
        ))
    }
}

/// [`validate_mcp_binding`] against the live MCP registry and `manager`'s
/// installed servers.
pub fn check_mcp_binding(
    manager: Option<&dyn McpHost>,
    workspace: &str,
    variant: &str,
) -> Result<Option<String>, String> {
    let connected = workforce_agent::llm::mcp_workspace::workspace_has_env(workspace, variant);
    let installed = manager.is_some_and(|m| {
        m.server_statuses()
            .iter()
            .any(|s| s.workspace.as_deref() == Some(workspace) && s.env.as_deref() == Some(variant))
    });
    validate_mcp_binding(workspace, variant, connected, installed)
}

/// Check every binding `next` adds or changes over `current` against the
/// agent's instances and MCP servers, returning the notes the MCP checks make.
pub fn check_changed_bindings(
    current: &AgentEnvironmentsConfig,
    next: &AgentEnvironmentsConfig,
    catalogue: &BTreeMap<String, SkillVariants>,
    manager: Option<&dyn McpHost>,
) -> Result<Vec<String>, String> {
    next.defs
        .iter()
        .flat_map(|(name, def)| {
            let before = current.defs.get(name);
            let skills = def
                .skills
                .iter()
                .filter(move |(skill, set)| before.and_then(|b| b.skills.get(*skill)) != Some(set))
                .map(|(skill, set)| validate_skill_binding(catalogue, skill, set).map(|()| None));
            let mcp = def
                .mcp
                .iter()
                .filter(move |(workspace, variant)| {
                    before.and_then(|b| b.mcp.get(*workspace)) != Some(variant)
                })
                .map(move |(workspace, variant)| check_mcp_binding(manager, workspace, variant));
            skills
                .chain(mcp)
                .map(move |checked| checked.map_err(|reason| format!("`{name}`: {reason}")))
        })
        .collect::<Result<Vec<Option<String>>, String>>()
        .map(|notes| notes.into_iter().flatten().collect())
}

/// Each multi-instance skill of the agent that `def` leaves unbound, which
/// keeps choosing per call.
pub fn environment_warnings(
    def: &EnvironmentDef,
    catalogue: &BTreeMap<String, SkillVariants>,
) -> Vec<String> {
    catalogue
        .iter()
        .filter(|(skill, _)| !def.skills.contains_key(*skill))
        .map(|(skill, variants)| {
            format!(
                "`{skill}` is not bound; each call may use any of its instances, defaulting to `{}`",
                variants.default
            )
        })
        .collect()
}

/// One environment as the API, the list tool and the UI see it.
pub fn environment_json(
    name: &str,
    def: &EnvironmentDef,
    is_default: bool,
    catalogue: &BTreeMap<String, SkillVariants>,
) -> Value {
    json!({
        "name": name,
        "default": is_default,
        "description": def.description,
        "draft": def.draft,
        "skills": def.skills,
        "mcp": def.mcp,
        "warnings": environment_warnings(def, catalogue),
    })
}

/// Every environment in `defs`, by name.
pub fn environments_json(
    defs: &BTreeMap<String, EnvironmentDef>,
    default: Option<&str>,
    catalogue: &BTreeMap<String, SkillVariants>,
) -> Vec<Value> {
    defs.iter()
        .map(|(name, def)| environment_json(name, def, default == Some(name.as_str()), catalogue))
        .collect()
}

/// One binding as a line: `skill `linear` → `a` (default), `b``.
pub fn skill_binding_line(skill: &str, instances: &[String]) -> String {
    let rendered: Vec<String> = instances
        .iter()
        .enumerate()
        .map(|(i, instance)| match (i, instances.len()) {
            (0, n) if n > 1 => format!("`{instance}` (default)"),
            _ => format!("`{instance}`"),
        })
        .collect();
    format!("skill `{skill}` → {}", rendered.join(", "))
}

pub(crate) fn ticked(items: &[&str]) -> String {
    items
        .iter()
        .map(|i| format!("`{i}`"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Hides the `env.*` tools from an agent whose environments are off.
pub struct EnvironmentToolAccess {
    resolved: ConfigCell,
}

impl EnvironmentToolAccess {
    pub fn policy(resolved: ConfigCell) -> Arc<dyn ToolAccess> {
        Arc::new(Self { resolved })
    }
}

impl ToolAccess for EnvironmentToolAccess {
    fn snapshot(&self) -> ToolAccessSnapshot {
        if self.resolved.snapshot().environments.enabled {
            return ToolAccessSnapshot::default();
        }
        ToolAccessSnapshot::new(
            TOOL_NAMES
                .iter()
                .map(|tool| {
                    (
                        tool.to_string(),
                        "Environments are off for this agent.".to_string(),
                    )
                })
                .collect(),
        )
    }
}

/// A bare agent over a fresh workspace. Boxed: the service future is large
/// enough to overflow a test thread's stack when two are built in one test.
#[cfg(test)]
pub(crate) async fn test_service() -> Arc<crate::serve::AgentService> {
    let tmp = tempfile::tempdir().unwrap();
    let mut cfg = workforce_common::settings::WorkforceConfig::default();
    cfg.hub.data_dir = tmp.path().to_string_lossy().to_string();
    cfg.memory.db_path = std::path::Path::new(&cfg.hub.data_dir)
        .join("memory.db")
        .to_string_lossy()
        .into_owned();
    cfg.llm.api_key = Some("test-key".into());
    std::mem::forget(tmp);
    Box::pin(crate::serve::AgentService::from_config(cfg, None))
        .await
        .expect("service")
}

/// Resolve the per-turn environment scope for `session`: its `cmd: env use`
/// choice, else this agent's default. `session = None` (sub-agents,
/// background runs) resolves to the default. With environments off only
/// the session's manual `cmd: use` MCP overrides are carried.
pub(crate) fn resolve_scope(
    service: &AgentService,
    session: Option<&workforce_common::session::Session>,
) -> crate::environments::EnvironmentScope {
    let manual_mcp_envs = session.map(|s| s.active_mcp_envs()).unwrap_or_default();
    let override_name = session.and_then(|s| s.active_environment());
    service
        .resolved
        .snapshot()
        .environments
        .scope(override_name.as_deref(), manual_mcp_envs)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use workforce_common::settings::EnvironmentsConfig;

    fn def(skills: &[(&str, &[&str])], mcp: &[(&str, &str)]) -> EnvironmentDef {
        EnvironmentDef {
            description: None,
            draft: false,
            skills: skills
                .iter()
                .map(|(s, i)| (s.to_string(), i.iter().map(|x| x.to_string()).collect()))
                .collect(),
            mcp: mcp
                .iter()
                .map(|(w, v)| (w.to_string(), v.to_string()))
                .collect(),
        }
    }

    fn agent(enabled: Option<bool>, default: Option<&str>) -> AgentEnvironmentsConfig {
        AgentEnvironmentsConfig {
            enabled,
            default: default.map(str::to_string),
            defs: [
                (
                    "work".to_string(),
                    def(
                        &[("linear", &["driftwerk", "fabrikam-crm"])],
                        &[("harbor", "prod")],
                    ),
                ),
                (
                    "staging".to_string(),
                    def(&[("metabase", &["staging"])], &[]),
                ),
            ]
            .into(),
        }
    }

    fn install(enabled: Option<bool>) -> EnvironmentsConfig {
        EnvironmentsConfig { enabled }
    }

    fn catalogue(entries: &[(&str, &[&str])]) -> BTreeMap<String, SkillVariants> {
        entries
            .iter()
            .filter_map(|(skill, instances)| {
                SkillVariants::of(instances.iter().map(|i| i.to_string()), instances[0])
                    .map(|v| (skill.to_string(), v))
            })
            .collect()
    }

    #[test]
    fn the_install_kill_switch_beats_the_agent() {
        let resolved =
            ResolvedEnvironments::resolve(&install(Some(false)), Some(&agent(Some(true), None)));
        assert_eq!(resolved, ResolvedEnvironments::default());
    }

    #[test]
    fn an_agent_has_environments_off_until_it_turns_them_on() {
        assert!(!ResolvedEnvironments::resolve(&install(None), Some(&agent(None, None))).enabled);
        assert!(!ResolvedEnvironments::resolve(&install(Some(true)), None).enabled);
        let on = ResolvedEnvironments::resolve(&install(None), Some(&agent(Some(true), None)));
        assert!(on.enabled);
        assert_eq!(on.defs.len(), 2);
    }

    #[test]
    fn active_prefers_an_owned_override_and_falls_back_to_the_default() {
        let resolved = ResolvedEnvironments::resolve(
            &install(None),
            Some(&agent(Some(true), Some("staging"))),
        );
        let named = resolved.active(Some("work")).unwrap();
        assert_eq!((named.name, named.is_override), ("work", true));
        let stale = resolved.active(Some("another-agents-env")).unwrap();
        assert_eq!((stale.name, stale.is_override), ("staging", false));
        let same = resolved.active(Some("staging")).unwrap();
        assert!(!same.is_override);
        assert_eq!(resolved.active(None).unwrap().name, "staging");
    }

    #[test]
    fn no_default_and_no_override_is_no_environment() {
        let resolved =
            ResolvedEnvironments::resolve(&install(None), Some(&agent(Some(true), None)));
        assert!(resolved.active(None).is_none());
        let scope = resolved.scope(None, [("ws".to_string(), "x".to_string())].into());
        assert_eq!(scope.environment, None);
        assert_eq!(scope.mcp_envs.len(), 1);
        assert!(scope.skill_instances.is_empty());
    }

    #[test]
    fn scope_expands_sets_and_lets_manual_mcp_overrides_win() {
        let resolved =
            ResolvedEnvironments::resolve(&install(None), Some(&agent(Some(true), Some("work"))));
        let scope = resolved.scope(None, [("harbor".to_string(), "demo".to_string())].into());
        assert_eq!(scope.environment.as_deref(), Some("work"));
        assert_eq!(scope.mcp_envs["harbor"], "demo");
        assert_eq!(
            scope.skill_instances["linear"],
            vec!["driftwerk", "fabrikam-crm"]
        );
    }

    #[test]
    fn a_draft_is_never_active_even_when_a_session_names_it() {
        let mut config = agent(Some(true), Some("staging"));
        config.defs.get_mut("work").unwrap().draft = true;
        let resolved = ResolvedEnvironments::resolve(&install(None), Some(&config));
        let active = resolved.active(Some("work")).unwrap();
        assert_eq!((active.name, active.is_override), ("staging", false));
    }

    #[test]
    fn the_snapshot_carries_the_install_switch() {
        assert!(ResolvedEnvironments::resolve(&install(None), None).allowed);
        assert!(!ResolvedEnvironments::resolve(&install(Some(false)), None).allowed);
    }

    #[test]
    fn skill_bindings_are_checked_against_the_agents_catalogue() {
        let cat = catalogue(&[("linear", &["driftwerk", "fabrikam-crm"])]);
        let set = |items: &[&str]| items.iter().map(|i| i.to_string()).collect::<Vec<_>>();
        assert!(
            validate_skill_binding(&cat, "linear", &set(&["fabrikam-crm", "driftwerk"])).is_ok()
        );
        let missing = validate_skill_binding(&cat, "linear", &set(&["acme"])).unwrap_err();
        assert!(missing.contains("`acme`"), "{missing}");
        assert!(validate_skill_binding(&cat, "jira", &set(&["x"])).is_err());
        assert!(validate_skill_binding(&cat, "linear", &[]).is_err());
    }

    #[test]
    fn validate_mcp_binding_allows_installed_warns_and_rejects_absent() {
        assert_eq!(
            validate_mcp_binding("harbor", "staging", true, false),
            Ok(None)
        );
        let warn = validate_mcp_binding("harbor", "staging", false, true).unwrap();
        assert!(warn.unwrap().contains("awaiting authorisation"));
        let err = validate_mcp_binding("harbor", "staging", false, false).unwrap_err();
        assert!(err.contains("No MCP server is installed"));
    }

    #[test]
    fn only_bindings_that_changed_are_checked() {
        let cat = catalogue(&[("linear", &["driftwerk", "fabrikam-crm"])]);
        let current = agent(Some(true), None);
        let mut next = current.clone();
        assert_eq!(
            check_changed_bindings(&current, &next, &cat, None),
            Ok(vec![])
        );
        next.defs
            .get_mut("staging")
            .unwrap()
            .skills
            .insert("linear".into(), vec!["fabrikam-crm".into()]);
        assert_eq!(
            check_changed_bindings(&current, &next, &cat, None),
            Ok(vec![])
        );
        next.defs
            .get_mut("staging")
            .unwrap()
            .skills
            .insert("linear".into(), vec!["acme".into()]);
        let err = check_changed_bindings(&current, &next, &cat, None).unwrap_err();
        assert!(
            err.starts_with("`staging`:") && err.contains("`acme`"),
            "{err}"
        );
        next.defs
            .get_mut("staging")
            .unwrap()
            .mcp
            .insert("ghost".into(), "prod".into());
        next.defs.get_mut("staging").unwrap().skills.clear();
        let err = check_changed_bindings(&current, &next, &cat, None).unwrap_err();
        assert!(err.contains("No MCP server is installed"), "{err}");
    }

    #[test]
    fn warnings_name_only_unbound_skills_of_this_agent() {
        let cat = catalogue(&[("linear", &["a", "b"]), ("metabase", &["prod", "staging"])]);
        let warnings = environment_warnings(&def(&[("linear", &["a"])], &[]), &cat);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("`metabase`"));
    }

    #[test]
    fn a_skill_binding_line_marks_the_default_of_a_set() {
        let set = vec!["driftwerk".to_string(), "fabrikam-crm".to_string()];
        assert_eq!(
            skill_binding_line("linear", &set),
            "skill `linear` → `driftwerk` (default), `fabrikam-crm`"
        );
        assert_eq!(
            skill_binding_line("metabase", &["prod".to_string()]),
            "skill `metabase` → `prod`"
        );
    }

    #[test]
    fn two_agents_with_the_same_environment_name_resolve_only_their_own() {
        let mine =
            ResolvedEnvironments::resolve(&install(None), Some(&agent(Some(true), Some("work"))));
        let theirs = ResolvedEnvironments::resolve(
            &install(None),
            Some(&AgentEnvironmentsConfig {
                enabled: Some(true),
                default: Some("work".into()),
                defs: [("work".to_string(), def(&[("linear", &["acme"])], &[]))].into(),
            }),
        );
        assert_eq!(
            mine.scope(None, HashMap::new()).skill_instances["linear"],
            vec!["driftwerk", "fabrikam-crm"]
        );
        assert_eq!(
            theirs.scope(None, HashMap::new()).skill_instances["linear"],
            vec!["acme"]
        );
    }
}
