//! Admin API for one agent's environments (admin scope on every route).
//!
//! - `GET /api/agents/{id}/environments` — its `[environments]`, each
//!   environment's bindings and warnings, and the instances and MCP workspaces it
//!   can bind, with the config's entity tag.
//! - `PUT /api/agents/{id}/environments` — replace its `[environments]`, only if
//!   `If-Match` names the config as it stands.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, OnceLock};

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use serde_json::{json, Value};

use workforce_common::settings::{AgentEnvironmentsConfig, AgentWorkspaceConfig};
use workforce_orchestrator::workflow_store::EditContext;

use crate::environments::{self, EditError};
use crate::gateway::{
    config_etag, config_write_refused, if_match_holds, resolve_agent, GatewayState,
};
use crate::serve::AgentService;

type Reply = Result<Json<Value>, (StatusCode, Json<Value>)>;

fn failure(status: StatusCode, error: impl std::fmt::Display) -> (StatusCode, Json<Value>) {
    (status, Json(json!({"error": error.to_string()})))
}

/// The agent's environments as its `config.toml` holds them, whether or not
/// they are on, with the body's entity tag.
fn view(service: &AgentService) -> Reply {
    let path = service
        .workspace
        .join(workforce_common::config_files::WORKSPACE_CONFIG);
    let body = workforce_common::config_files::read(&path)
        .map_err(|e| failure(StatusCode::INTERNAL_SERVER_ERROR, e))?
        .unwrap_or_default();
    let envs = AgentWorkspaceConfig::parse(&body)
        .map_err(|e| failure(StatusCode::UNPROCESSABLE_ENTITY, e))?
        .environments
        .unwrap_or_default();
    let catalogue = service.tools.skill_variants();
    let skills: Vec<Value> = catalogue
        .iter()
        .map(|(name, v)| json!({"name": name, "instances": v.instances, "default": v.default}))
        .collect();
    let mcp_workspaces: Vec<Value> = service
        .mcp_manager()
        .map(|m| m.server_statuses())
        .unwrap_or_default()
        .into_iter()
        .filter_map(|s| Some((s.workspace?, s.env?)))
        .fold(
            BTreeMap::<String, BTreeSet<String>>::new(),
            |mut by_workspace, (workspace, env)| {
                by_workspace.entry(workspace).or_default().insert(env);
                by_workspace
            },
        )
        .into_iter()
        .map(|(name, envs)| json!({"name": name, "envs": envs}))
        .collect();
    Ok(Json(json!({
        "allowed": service.resolved.snapshot().environments.allowed,
        "enabled": envs.enabled == Some(true),
        "default": envs.default,
        "environments": environments::environments_json(
            &envs.defs,
            envs.default.as_deref(),
            &catalogue,
        ),
        "options": {"skills": skills, "mcp_workspaces": mcp_workspaces},
        "etag": config_etag(&body),
    })))
}

async fn admin(
    state: &GatewayState,
    headers: &HeaderMap,
) -> Result<Option<String>, (StatusCode, Json<Value>)> {
    let caller =
        crate::gateway::require_scope(state, headers, None, crate::jwt::SCOPE_ADMIN).await?;
    Ok(caller.identity_id)
}

// ── GET /api/agents/{id}/environments ────────────────────────────────────────

pub async fn get_environments(
    Path(id): Path<String>,
    headers: HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> Reply {
    admin(&state, &headers).await?;
    let service = resolve_agent(&state, &id)?;
    view(&service)
}

// ── PUT /api/agents/{id}/environments ────────────────────────────────────────

pub async fn put_environments(
    Path(id): Path<String>,
    headers: HeaderMap,
    State(state): State<Arc<GatewayState>>,
    Json(body): Json<AgentEnvironmentsConfig>,
) -> Reply {
    let actor = admin(&state, &headers).await?;
    let service = resolve_agent(&state, &id)?;
    let notes = replace(
        &service,
        EditContext::new("web", actor.as_deref()),
        |current| if_match_holds(&headers, current),
        body,
    )
    .await
    .map_err(|e| match e {
        EditError::Invalid(reason) => failure(StatusCode::BAD_REQUEST, reason),
        EditError::Write(refused) => config_write_refused(&id, refused),
    })?;
    let Json(mut saved) = view(&service)?;
    saved["notes"] = json!(notes);
    Ok(Json(saved))
}

/// Replace the agent's environments with `body`, each added or changed binding
/// checked against what the agent can reach, returning the checks' notes.
async fn replace(
    service: &AgentService,
    edit: EditContext<'_>,
    expects: impl Fn(Option<&str>) -> bool,
    body: AgentEnvironmentsConfig,
) -> Result<Vec<String>, EditError> {
    let catalogue = service.tools.skill_variants();
    let manager = service.mcp_manager();
    let notes = OnceLock::new();
    environments::write(service, edit, expects, |current| {
        let checked =
            environments::check_changed_bindings(&current, &body, &catalogue, manager.as_deref())?;
        let _ = notes.set(checked);
        Ok(body.clone())
    })
    .await?;
    Ok(notes.into_inner().unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use workforce_agent::llm::environment::SkillVariants;
    use workforce_common::settings::EnvironmentDef;

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

    fn body(instances: &[&str]) -> AgentEnvironmentsConfig {
        AgentEnvironmentsConfig {
            enabled: Some(true),
            default: Some("work".into()),
            defs: [(
                "work".to_string(),
                EnvironmentDef {
                    skills: [(
                        "linear".to_string(),
                        instances.iter().map(|i| i.to_string()).collect(),
                    )]
                    .into(),
                    ..Default::default()
                },
            )]
            .into(),
        }
    }

    fn edit() -> EditContext<'static> {
        EditContext::new("web", Some("admin"))
    }

    #[tokio::test]
    async fn view_shows_the_agents_table_options_and_etag_even_when_off() {
        let service = service().await;
        let Json(empty) = view(&service).unwrap();
        assert_eq!(empty["enabled"], false);
        assert_eq!(empty["environments"], json!([]));
        assert_eq!(empty["options"]["skills"][0]["name"], "linear");
        assert!(empty["etag"].as_str().unwrap().starts_with('"'));
    }

    #[tokio::test]
    async fn replace_round_trips_and_hot_applies() {
        let service = service().await;
        let notes = replace(
            &service,
            edit(),
            |_| true,
            body(&["fabrikam-crm", "driftwerk"]),
        )
        .await
        .unwrap();
        assert!(notes.is_empty());
        let Json(view) = view(&service).unwrap();
        assert_eq!(view["default"], "work");
        assert_eq!(
            view["environments"][0]["skills"]["linear"],
            json!(["fabrikam-crm", "driftwerk"])
        );
        assert_eq!(
            service.resolved.snapshot().environments.defs["work"].skills["linear"],
            vec!["fabrikam-crm", "driftwerk"]
        );
    }

    #[tokio::test]
    async fn replace_refuses_an_instance_the_agent_lacks_and_a_stale_tag() {
        let service = service().await;
        let unknown = replace(&service, edit(), |_| true, body(&["acme"]))
            .await
            .unwrap_err();
        assert!(
            matches!(&unknown, EditError::Invalid(r) if r.contains("`acme`")),
            "{unknown}"
        );
        let stale = replace(&service, edit(), |_| false, body(&["driftwerk"]))
            .await
            .unwrap_err();
        assert!(matches!(
            stale,
            EditError::Write(crate::agent_config_write::Refused::Stale { .. })
        ));
        let draft_default = AgentEnvironmentsConfig {
            defs: [(
                "work".to_string(),
                EnvironmentDef {
                    draft: true,
                    ..Default::default()
                },
            )]
            .into(),
            ..body(&["driftwerk"])
        };
        let refused = replace(&service, edit(), |_| true, draft_default)
            .await
            .unwrap_err();
        assert!(
            matches!(&refused, EditError::Invalid(r) if r.contains("draft")),
            "{refused}"
        );
    }
}
