//! Changing an agent's environments: pure transforms over its
//! `[environments]` table, and the one write that lands them in its
//! `config.toml` (admitted, stale-checked, revisioned, hot-applied).

use std::fmt;

use serde::Serialize;
use toml_edit::{DocumentMut, Item, Table};
use workforce_common::settings::{AgentEnvironmentsConfig, AgentWorkspaceConfig, EnvironmentDef};
use workforce_orchestrator::workflow_store::EditContext;

use crate::agent_config_write::{self, Refused, Saved};
use crate::serve::AgentService;

type Envs = AgentEnvironmentsConfig;

/// Why an environment change did not land.
#[derive(Debug)]
pub enum EditError {
    /// The change itself is not one that can be made.
    Invalid(String),
    /// The config write refused it.
    Write(Refused),
}

impl fmt::Display for EditError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(reason) => f.write_str(reason),
            Self::Write(refused) => refused.fmt(f),
        }
    }
}

/// Apply `transform` to the agent's environments and save the result, once
/// more on a fresh read when someone else saved in between.
pub async fn update(
    service: &AgentService,
    edit: EditContext<'_>,
    transform: impl Fn(Envs) -> Result<Envs, String>,
) -> Result<Envs, EditError> {
    match write(service, edit, |_| true, &transform).await {
        Err(EditError::Write(Refused::Stale { .. })) => {
            write(service, edit, |_| true, &transform).await
        }
        outcome => outcome,
    }
    .map(|(envs, _)| envs)
}

/// Apply `transform` to the agent's environments as `config.toml` stands now
/// and save the result, provided `expects` accepts that file.
pub async fn write(
    service: &AgentService,
    edit: EditContext<'_>,
    expects: impl Fn(Option<&str>) -> bool,
    transform: impl Fn(Envs) -> Result<Envs, String>,
) -> Result<(Envs, Saved), EditError> {
    let path = service
        .workspace
        .join(workforce_common::config_files::WORKSPACE_CONFIG);
    let read = workforce_common::config_files::read(&path)
        .map_err(|e| EditError::Write(Refused::Io(format!("failed to read config.toml: {e}"))))?;
    let body = read.as_deref().unwrap_or_default();
    let current = AgentWorkspaceConfig::parse(body)
        .map_err(|e| EditError::Invalid(format!("config.toml does not parse: {e}")))?
        .environments
        .unwrap_or_default();
    let next = transform(current).map_err(EditError::Invalid)?;
    next.validate()
        .map_err(|e| EditError::Invalid(e.to_string()))?;
    let rendered = render(body, &next).map_err(EditError::Invalid)?;
    let store = crate::serve::shared_workflow_store(&service.config).await;
    let saved = agent_config_write::save(
        service,
        service.agent_id.as_str(),
        store.as_deref(),
        &rendered,
        edit,
        |current| current == read.as_deref() && expects(current),
    )
    .await
    .map_err(EditError::Write)?;
    Ok((next, saved))
}

/// `body` with its `[environments]` table replaced by `envs`, every other key
/// and comment kept.
pub(crate) fn render(body: &str, envs: &Envs) -> Result<String, String> {
    #[derive(Serialize)]
    struct Wrapped<'a> {
        environments: &'a Envs,
    }
    let mut doc: DocumentMut = body
        .parse()
        .map_err(|e| format!("config.toml does not parse: {e}"))?;
    let mut rendered: DocumentMut = toml::to_string(&Wrapped { environments: envs })
        .map_err(|e| e.to_string())?
        .parse()
        .map_err(|e| format!("{e}"))?;
    let table = rendered
        .remove("environments")
        .unwrap_or_else(|| Item::Table(Table::new()));
    doc.insert("environments", table);
    Ok(doc.to_string())
}

fn in_env(
    mut envs: Envs,
    env: &str,
    change: impl FnOnce(&mut EnvironmentDef) -> Result<(), String>,
) -> Result<Envs, String> {
    let def = envs
        .defs
        .get_mut(env)
        .ok_or_else(|| format!("No environment named `{env}`."))?;
    change(def)?;
    Ok(envs)
}

/// Create an environment; a `draft` is never active until promoted.
pub fn create(
    name: &str,
    description: Option<String>,
    draft: bool,
) -> impl Fn(Envs) -> Result<Envs, String> {
    let name = name.trim().to_string();
    move |mut envs| {
        if name.is_empty() {
            return Err("An environment needs a name.".to_string());
        }
        if envs.defs.contains_key(&name) {
            return Err(format!("Environment `{name}` already exists."));
        }
        envs.defs.insert(
            name.clone(),
            EnvironmentDef {
                description: description.clone(),
                draft,
                ..Default::default()
            },
        );
        Ok(envs)
    }
}

/// Delete an environment; deleting the default leaves none.
pub fn delete(name: &str) -> impl Fn(Envs) -> Result<Envs, String> {
    let name = name.to_string();
    move |mut envs| {
        envs.defs
            .remove(&name)
            .ok_or_else(|| format!("No environment named `{name}`."))?;
        if envs.default.as_deref() == Some(name.as_str()) {
            envs.default = None;
        }
        Ok(envs)
    }
}

/// Bind `skill` to `instances`, replacing any set it had; the first instance
/// is the one a call naming none gets.
pub fn bind_skill(
    env: &str,
    skill: &str,
    instances: Vec<String>,
) -> impl Fn(Envs) -> Result<Envs, String> {
    let (env, skill) = (env.to_string(), skill.to_string());
    move |envs| {
        in_env(envs, &env, |def| {
            def.skills.insert(skill.clone(), instances.clone());
            Ok(())
        })
    }
}

/// Drop `instance` from `skill`'s set, or the whole binding when `instance` is
/// `None` or it was the last.
pub fn unbind_skill(
    env: &str,
    skill: &str,
    instance: Option<String>,
) -> impl Fn(Envs) -> Result<Envs, String> {
    let (env, skill) = (env.to_string(), skill.to_string());
    move |envs| {
        in_env(envs, &env, |def| {
            let set = def
                .skills
                .get_mut(&skill)
                .ok_or_else(|| format!("`{env}` does not bind `{skill}`."))?;
            match &instance {
                None => set.clear(),
                Some(instance) => {
                    let before = set.len();
                    set.retain(|i| i != instance);
                    if set.len() == before {
                        return Err(format!("`{env}` does not bind `{skill}` to `{instance}`."));
                    }
                }
            }
            if set.is_empty() {
                def.skills.remove(&skill);
            }
            Ok(())
        })
    }
}

pub fn bind_mcp(
    env: &str,
    workspace: &str,
    variant: &str,
) -> impl Fn(Envs) -> Result<Envs, String> {
    let (env, workspace, variant) = (env.to_string(), workspace.to_string(), variant.to_string());
    move |envs| {
        in_env(envs, &env, |def| {
            def.mcp.insert(workspace.clone(), variant.clone());
            Ok(())
        })
    }
}

pub fn unbind_mcp(env: &str, workspace: &str) -> impl Fn(Envs) -> Result<Envs, String> {
    let (env, workspace) = (env.to_string(), workspace.to_string());
    move |envs| {
        in_env(envs, &env, |def| {
            def.mcp
                .remove(&workspace)
                .map(|_| ())
                .ok_or_else(|| format!("`{env}` does not bind MCP workspace `{workspace}`."))
        })
    }
}

/// Make `name` the default, promoting it if it is a draft.
pub fn set_default(name: &str) -> impl Fn(Envs) -> Result<Envs, String> {
    let promote = promote(name);
    let name = name.to_string();
    move |envs| {
        let mut envs = promote(envs)?;
        envs.default = Some(name.clone());
        Ok(envs)
    }
}

/// Promote a draft so it can be active; a no-op on one that is not.
pub fn promote(name: &str) -> impl Fn(Envs) -> Result<Envs, String> {
    let name = name.to_string();
    move |envs| {
        in_env(envs, &name, |def| {
            def.draft = false;
            Ok(())
        })
    }
}

/// Refuse to change `name` unless it is a draft, the only kind the agent may
/// change, read from `config.toml` as it stands at the write.
pub fn only_draft(envs: &Envs, name: &str) -> Result<(), String> {
    match envs.defs.get(name) {
        None => Err(format!("No environment named `{name}`.")),
        Some(def) if def.draft => Ok(()),
        Some(_) => Err(format!(
            "`{name}` is not a draft — a person activated it, so changing it is human-only."
        )),
    }
}

pub fn clear_default() -> impl Fn(Envs) -> Result<Envs, String> {
    |mut envs| {
        envs.default = None;
        Ok(envs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::environments::test_service as service;

    fn set(items: &[&str]) -> Vec<String> {
        items.iter().map(|i| i.to_string()).collect()
    }

    fn with_work() -> Envs {
        create("work", None, false)(Envs::default()).unwrap()
    }

    #[test]
    fn create_refuses_a_duplicate_and_a_blank_name() {
        let envs = with_work();
        assert!(create("work", None, false)(envs.clone()).is_err());
        assert!(create("  ", None, false)(envs).is_err());
    }

    #[test]
    fn deleting_the_default_leaves_none() {
        let envs = set_default("work")(with_work()).unwrap();
        let envs = delete("work")(envs).unwrap();
        assert!(envs.defs.is_empty());
        assert_eq!(envs.default, None);
        assert!(delete("work")(envs).is_err());
    }

    #[test]
    fn bind_skill_replaces_the_set_in_order() {
        let envs = bind_skill("work", "linear", set(&["a"]))(with_work()).unwrap();
        let envs = bind_skill("work", "linear", set(&["fabrikam-crm", "driftwerk"]))(envs).unwrap();
        assert_eq!(
            envs.defs["work"].skills["linear"],
            set(&["fabrikam-crm", "driftwerk"])
        );
        assert!(bind_skill("nope", "linear", set(&["a"]))(envs).is_err());
    }

    #[test]
    fn unbind_skill_drops_one_instance_then_the_binding() {
        let envs = bind_skill("work", "linear", set(&["a", "b"]))(with_work()).unwrap();
        let envs = unbind_skill("work", "linear", Some("a".into()))(envs).unwrap();
        assert_eq!(envs.defs["work"].skills["linear"], set(&["b"]));
        assert!(unbind_skill("work", "linear", Some("x".into()))(envs.clone()).is_err());
        let envs = unbind_skill("work", "linear", Some("b".into()))(envs).unwrap();
        assert!(!envs.defs["work"].skills.contains_key("linear"));
        let envs = bind_skill("work", "jira", set(&["a", "b"]))(envs).unwrap();
        let envs = unbind_skill("work", "jira", None)(envs).unwrap();
        assert!(envs.defs["work"].skills.is_empty());
    }

    #[test]
    fn mcp_bindings_are_one_variant_each() {
        let envs = bind_mcp("work", "harbor", "prod")(with_work()).unwrap();
        let envs = bind_mcp("work", "harbor", "staging")(envs).unwrap();
        assert_eq!(envs.defs["work"].mcp["harbor"], "staging");
        let envs = unbind_mcp("work", "harbor")(envs).unwrap();
        assert!(unbind_mcp("work", "harbor")(envs).is_err());
    }

    #[test]
    fn setting_the_default_promotes_a_draft() {
        let envs = create("next", None, true)(Envs::default()).unwrap();
        assert!(only_draft(&envs, "next").is_ok());
        let envs = set_default("next")(envs).unwrap();
        assert!(!envs.defs["next"].draft);
        assert!(only_draft(&envs, "next").is_err());
        assert!(only_draft(&envs, "ghost").is_err());
    }

    #[test]
    fn promote_makes_a_draft_activatable() {
        let envs = create("next", None, true)(Envs::default()).unwrap();
        assert!(!promote("next")(envs).unwrap().defs["next"].draft);
    }

    #[test]
    fn the_default_must_name_an_environment_and_can_be_cleared() {
        assert!(set_default("ghost")(with_work()).is_err());
        let envs = set_default("work")(with_work()).unwrap();
        assert_eq!(envs.default.as_deref(), Some("work"));
        assert_eq!(clear_default()(envs).unwrap().default, None);
    }

    const BODY: &str = "# Per-agent configuration\n\
[llm]\n\
voice_model = \"gemini-3.8-flash\" # spoken turns\n\
\n\
[environments]\n\
enabled = true\n";

    #[test]
    fn render_replaces_only_the_environments_table() {
        let envs = Envs {
            enabled: Some(true),
            default: Some("work".into()),
            defs: [(
                "work".to_string(),
                EnvironmentDef {
                    description: Some("Driftwerk and Fabrikam".into()),
                    draft: false,
                    skills: [("linear".to_string(), set(&["driftwerk", "fabrikam-crm"]))].into(),
                    mcp: [("harbor".to_string(), "prod".to_string())].into(),
                },
            )]
            .into(),
        };
        let body = render(BODY, &envs).unwrap();
        assert!(body.starts_with("# Per-agent configuration"), "{body}");
        assert!(
            body.contains("voice_model = \"gemini-3.8-flash\" # spoken turns"),
            "{body}"
        );
        let parsed = AgentWorkspaceConfig::parse(&body).unwrap();
        assert_eq!(parsed.environments, Some(envs));
    }

    #[test]
    fn render_keeps_later_tables_intact_around_a_table_in_the_middle() {
        let body = "[llm]\nvoice_model = \"x\"\n\n[environments]\nenabled = true\n\n\
                    [compaction]\nkeep_recent_messages = 12 # keep\n";
        let envs = bind_skill("work", "linear", set(&["a", "b"]))(with_work()).unwrap();
        let rendered = render(body, &envs).unwrap();
        let parsed = AgentWorkspaceConfig::parse(&rendered).unwrap();
        assert_eq!(parsed.environments, Some(envs));
        assert_eq!(
            parsed.compaction.unwrap().keep_recent_messages,
            Some(12),
            "{rendered}"
        );
        assert!(
            rendered.contains("keep_recent_messages = 12 # keep"),
            "{rendered}"
        );
    }

    #[test]
    fn render_keeps_an_empty_table() {
        let body = render(BODY, &Envs::default()).unwrap();
        let parsed = AgentWorkspaceConfig::parse(&body).unwrap();
        assert_eq!(parsed.environments, Some(Envs::default()));
    }

    fn edit_ctx() -> EditContext<'static> {
        EditContext::new("test", Some("admin"))
    }

    #[tokio::test]
    async fn update_writes_config_toml_and_hot_applies_it() {
        let service = service().await;
        let envs = update(&service, edit_ctx(), |mut envs: Envs| {
            envs.enabled = Some(true);
            Ok(envs)
        })
        .await
        .unwrap();
        assert_eq!(envs.enabled, Some(true));
        update(&service, edit_ctx(), create("work", None, false))
            .await
            .unwrap();
        update(&service, edit_ctx(), set_default("work"))
            .await
            .unwrap();
        let resolved = service.resolved.snapshot();
        assert!(resolved.environments.enabled);
        assert_eq!(resolved.environments.default.as_deref(), Some("work"));
    }

    #[tokio::test]
    async fn an_invalid_change_leaves_the_file_alone() {
        let service = service().await;
        update(&service, edit_ctx(), create("work", None, false))
            .await
            .unwrap();
        let err = update(&service, edit_ctx(), set_default("ghost"))
            .await
            .unwrap_err();
        assert!(matches!(err, EditError::Invalid(_)), "{err}");
        let written = AgentWorkspaceConfig::load(&service.workspace)
            .unwrap()
            .unwrap()
            .environments
            .unwrap();
        assert_eq!(written.default, None);
    }

    #[tokio::test]
    async fn write_refuses_a_file_the_caller_did_not_expect() {
        let service = service().await;
        let err = write(&service, edit_ctx(), |_| false, create("work", None, false))
            .await
            .unwrap_err();
        assert!(
            matches!(err, EditError::Write(Refused::Stale { .. })),
            "{err}"
        );
    }
}
