//! One-time move of the hub-wide environments that used to live in
//! `workflow_defs` (`kind = "environment"`) into each agent's own
//! `config.toml`. Every agent used to see every one of them under one shared
//! default, so each gets them all and keeps exactly the lock it had. Which
//! instances an agent reaches is not known at start (per-person credentials
//! resolve only with a person in scope), so nothing is filtered by it;
//! operators prune per agent afterwards.

use std::sync::Arc;

use serde::Deserialize;
use workforce_common::settings::{
    AgentEnvironmentsConfig, AgentWorkspaceConfig, EnvironmentDef, EnvironmentsConfig,
    ENVIRONMENT_CLEAR_WORDS,
};
use workforce_orchestrator::workflow_store::{EditContext, WorkflowStore};

use super::edit::{write, EditError};
use crate::agent_config_write::Refused;
use crate::serve::AgentService;

const LEGACY_KIND: &str = "environment";
const VIA: &str = "environments_migration";

#[derive(Deserialize)]
struct LegacyDef {
    name: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    default: bool,
    #[serde(default)]
    members: Vec<LegacyMember>,
}

#[derive(Deserialize)]
struct LegacyMember {
    target: LegacyTarget,
    name: String,
    variant: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum LegacyTarget {
    McpWorkspace,
    Skill,
}

/// The legacy rows that parse, by name, with their ids. One that does not parse
/// is left in place and reported, never deleted.
async fn legacy_rows(store: &WorkflowStore) -> eyre::Result<Vec<(String, LegacyDef)>> {
    let mut rows: Vec<(String, LegacyDef)> = store
        .list(LEGACY_KIND)
        .await?
        .into_iter()
        .filter_map(|row| match serde_json::from_str(&row.toml_source) {
            Ok(def) => Some((row.id, def)),
            Err(error) => {
                tracing::error!(id = %row.id, %error, "a legacy environment does not parse; left in place");
                None
            }
        })
        .collect();
    rows.sort_by(|a, b| a.1.name.cmp(&b.1.name));
    Ok(rows)
}

/// Move the legacy environments into every agent of `services` that has no
/// `[environments]` table yet. The rows that moved are deleted only once all
/// of `services` have a table and `complete` says no configured agent failed to
/// load, so an interrupted run is finished by the next start.
pub async fn migrate_legacy_environments(
    store: &WorkflowStore,
    install: &EnvironmentsConfig,
    services: &[Arc<AgentService>],
    complete: bool,
) -> eyre::Result<()> {
    let rows = legacy_rows(store).await?;
    if rows.is_empty() {
        return Ok(());
    }
    let mut adopted = true;
    for service in services {
        if let Err(error) = adopt(service, &config_for(&rows, install)).await {
            adopted = false;
            tracing::error!(agent = %service.agent_id, %error, "legacy environments not moved; this agent runs without them");
        }
    }
    if !(adopted && complete) {
        return Ok(());
    }
    for (id, _) in &rows {
        store
            .delete(id, LEGACY_KIND, None, EditContext::new(VIA, None))
            .await?;
    }
    tracing::info!(
        environments = rows.len(),
        agents = services.len(),
        "moved hub-wide environments into each agent's config"
    );
    Ok(())
}

/// Give an agent that loaded after the start-up move the legacy environments
/// still waiting for it.
pub async fn adopt_legacy_environments(
    store: &WorkflowStore,
    install: &EnvironmentsConfig,
    service: &AgentService,
) -> eyre::Result<()> {
    let rows = legacy_rows(store).await?;
    if rows.is_empty() {
        return Ok(());
    }
    adopt(service, &config_for(&rows, install))
        .await
        .map_err(|e| eyre::eyre!("{e}"))
}

fn config_for(
    rows: &[(String, LegacyDef)],
    install: &EnvironmentsConfig,
) -> AgentEnvironmentsConfig {
    let defs: Vec<&LegacyDef> = rows.iter().map(|(_, def)| def).collect();
    legacy_config(&defs, install.enabled == Some(true))
}

/// Legacy definitions as an agent's `[environments]`. Their default, and the
/// switch that made them bite, carry over only when the install had
/// environments explicitly on, the one state in which they were active.
fn legacy_config(legacy: &[&LegacyDef], active: bool) -> AgentEnvironmentsConfig {
    let named: Vec<&LegacyDef> = legacy
        .iter()
        .copied()
        .filter(|def| {
            !def.name.trim().is_empty() && !ENVIRONMENT_CLEAR_WORDS.contains(&def.name.as_str())
        })
        .collect();
    AgentEnvironmentsConfig {
        enabled: active.then_some(true),
        default: named
            .iter()
            .find(|def| def.default)
            .filter(|_| active)
            .map(|def| def.name.clone()),
        defs: named
            .iter()
            .map(|def| (def.name.clone(), converted(def)))
            .collect(),
    }
}

fn converted(def: &LegacyDef) -> EnvironmentDef {
    let (skills, mcp): (Vec<&LegacyMember>, Vec<&LegacyMember>) = def
        .members
        .iter()
        .partition(|m| matches!(m.target, LegacyTarget::Skill));
    EnvironmentDef {
        description: def.description.clone(),
        draft: false,
        skills: skills
            .into_iter()
            .map(|m| (m.name.clone(), vec![m.variant.clone()]))
            .collect(),
        mcp: mcp
            .into_iter()
            .map(|m| (m.name.clone(), m.variant.clone()))
            .collect(),
    }
}

fn has_environments(body: Option<&str>) -> bool {
    body.and_then(|body| AgentWorkspaceConfig::parse(body).ok())
        .is_some_and(|config| config.environments.is_some())
}

/// Give `service` the legacy environments unless it already has a table. A
/// replica that wrote it first wins; this one then finds it there.
async fn adopt(service: &AgentService, envs: &AgentEnvironmentsConfig) -> Result<(), EditError> {
    let attempt = || {
        write(
            service,
            EditContext::new(VIA, None),
            |current| !has_environments(current),
            |_| Ok(envs.clone()),
        )
    };
    let path = service
        .workspace
        .join(workforce_common::config_files::WORKSPACE_CONFIG);
    let present = || {
        has_environments(
            workforce_common::config_files::read(&path)
                .ok()
                .flatten()
                .as_deref(),
        )
    };
    if present() {
        return Ok(());
    }
    match attempt().await {
        Ok(_) => Ok(()),
        Err(EditError::Write(Refused::Stale { .. })) if present() => Ok(()),
        Err(EditError::Write(Refused::Stale { .. })) => attempt().await.map(|_| ()),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::environments::test_service as service;

    fn legacy(json: &str) -> Vec<LegacyDef> {
        serde_json::from_str(json).unwrap()
    }

    fn config(json: &str, active: bool) -> AgentEnvironmentsConfig {
        let rows = legacy(json);
        legacy_config(&rows.iter().collect::<Vec<_>>(), active)
    }

    const ROWS: &str = r#"[
        {"name": "driftwerk", "default": true, "members": [
            {"target": "skill", "name": "linear", "variant": "driftwerk"},
            {"target": "mcp_workspace", "name": "harbor", "variant": "prod"}
        ]},
        {"name": "staging", "description": "the other one", "members": [
            {"target": "skill", "name": "metabase", "variant": "staging"}
        ]}
    ]"#;

    #[test]
    fn an_active_install_carries_the_default_and_the_switch() {
        let envs = config(ROWS, true);
        assert_eq!(envs.enabled, Some(true));
        assert_eq!(envs.default.as_deref(), Some("driftwerk"));
        assert_eq!(envs.defs["driftwerk"].skills["linear"], vec!["driftwerk"]);
        assert_eq!(envs.defs["driftwerk"].mcp["harbor"], "prod");
        assert_eq!(
            envs.defs["staging"].description.as_deref(),
            Some("the other one")
        );
        envs.validate().unwrap();
    }

    #[test]
    fn an_inactive_install_carries_the_definitions_only() {
        let envs = config(ROWS, false);
        assert_eq!(envs.enabled, None);
        assert_eq!(envs.default, None);
        assert_eq!(envs.defs.len(), 2);
    }

    #[test]
    fn the_default_carries_only_with_its_definition_and_reserved_names_drop() {
        let rows = legacy(ROWS);
        let envs = legacy_config(&[&rows[1]], true);
        assert_eq!(envs.default, None);
        assert_eq!(envs.defs.len(), 1);
        let reserved = legacy(r#"[{"name": "none", "default": true}]"#);
        let envs = legacy_config(&reserved.iter().collect::<Vec<_>>(), true);
        assert!(envs.defs.is_empty() && envs.default.is_none());
        envs.validate().unwrap();
    }

    #[test]
    fn only_a_table_counts_as_already_migrated() {
        assert!(!has_environments(None));
        assert!(!has_environments(Some("[llm]\nvoice_model = \"x\"\n")));
        assert!(has_environments(Some("[environments]\n")));
    }

    #[tokio::test]
    async fn adopt_writes_once_and_leaves_an_existing_table_alone() {
        let service = service().await;
        let envs = config(ROWS, true);
        adopt(&service, &envs).await.unwrap();
        let written = AgentWorkspaceConfig::load(&service.workspace)
            .unwrap()
            .unwrap()
            .environments
            .unwrap();
        assert_eq!(written, envs);
        adopt(&service, &AgentEnvironmentsConfig::default())
            .await
            .unwrap();
        let kept = AgentWorkspaceConfig::load(&service.workspace)
            .unwrap()
            .unwrap()
            .environments
            .unwrap();
        assert_eq!(kept, envs);
        assert!(service.resolved.snapshot().environments.enabled);
    }

    async fn with_store<F, Fut>(test: F)
    where
        F: FnOnce(Arc<WorkflowStore>) -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        workforce_postgres::test_support::with_test_pool(
            WorkflowStore::build_pool,
            |pool| async move { test(WorkflowStore::new(pool).await.expect("store")).await },
        )
        .await;
    }

    async fn seed_row(store: &WorkflowStore, id: &str, source: &str) {
        store
            .save(
                id,
                LEGACY_KIND,
                source,
                None,
                None,
                workforce_core::access::Visibility::Global,
                workforce_orchestrator::workflow_store::RunAsWrite::Unchanged,
                EditContext::new("test", None),
            )
            .await
            .unwrap();
    }

    async fn seed(store: &WorkflowStore) {
        let rows: Vec<serde_json::Value> = serde_json::from_str(ROWS).unwrap();
        for row in rows {
            seed_row(store, row["name"].as_str().unwrap(), &row.to_string()).await;
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn every_agent_gets_them_and_the_rows_go_once_all_have() {
        with_store(|store| async move {
            seed(&store).await;
            let agents = vec![service().await, service().await];
            let install = EnvironmentsConfig {
                enabled: Some(true),
            };
            migrate_legacy_environments(&store, &install, &agents, true)
                .await
                .unwrap();
            for agent in &agents {
                let envs = agent.resolved.snapshot().environments.clone();
                assert_eq!(envs.default.as_deref(), Some("driftwerk"));
                assert_eq!(envs.defs.len(), 2);
            }
            assert!(store.list(LEGACY_KIND).await.unwrap().is_empty());
            migrate_legacy_environments(&store, &install, &agents, true)
                .await
                .unwrap();
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_row_that_does_not_parse_is_never_deleted() {
        with_store(|store| async move {
            seed(&store).await;
            seed_row(&store, "broken", "{not json").await;
            let agents = vec![service().await];
            migrate_legacy_environments(&store, &EnvironmentsConfig::default(), &agents, true)
                .await
                .unwrap();
            let left: Vec<String> = store
                .list(LEGACY_KIND)
                .await
                .unwrap()
                .into_iter()
                .map(|row| row.id)
                .collect();
            assert_eq!(left, ["broken"]);
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_agent_that_loads_later_adopts_what_is_still_waiting() {
        with_store(|store| async move {
            seed(&store).await;
            let late = service().await;
            let install = EnvironmentsConfig {
                enabled: Some(true),
            };
            adopt_legacy_environments(&store, &install, &late)
                .await
                .unwrap();
            assert_eq!(
                late.resolved.snapshot().environments.default.as_deref(),
                Some("driftwerk")
            );
            assert_eq!(store.list(LEGACY_KIND).await.unwrap().len(), 2);
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_rows_stay_while_an_agent_failed_to_load() {
        with_store(|store| async move {
            seed(&store).await;
            let agents = vec![service().await];
            migrate_legacy_environments(&store, &EnvironmentsConfig::default(), &agents, false)
                .await
                .unwrap();
            assert_eq!(store.list(LEGACY_KIND).await.unwrap().len(), 2);
            assert!(!agents[0].resolved.snapshot().environments.enabled);
        })
        .await;
    }
}
