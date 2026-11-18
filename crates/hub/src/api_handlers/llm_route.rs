//! A person's own route from the web: whether their work runs on the hub's
//! models or on their own subscription. `/api/me/llm-route` is their
//! setting for every agent; `/api/me/agents/{id}/llm-route` their setting
//! for one agent, which wins. A route of a kind can only be chosen while
//! the hub's `[llm.personal_routes]` switch (and the agent) offers it and
//! the person has a working login of that kind of their own. Every change
//! is kept in the audit table beside the settings. These routes act for a
//! person signed in themselves, never for a paired device.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use workforce_common::personal_route::{
    AgentRoute, PersonalRoute, PersonalRouteKind, PersonalRoutesConfig,
};
use workforce_common::subscription::{AgentOwner, CredentialTier};

use crate::gateway::{AgentViewer, GatewayState};
use crate::hub_core::logins::cooldown::CooldownStore;
use crate::hub_core::logins::resolve::{login_state, Miss, Vaults};
use crate::hub_core::personal_routes::{PersonRouteStore, PersonalRoutes};
use crate::serve::AgentService;

type ApiError = (StatusCode, Json<serde_json::Value>);
type ApiResult<T> = Result<Json<T>, ApiError>;

fn err(status: StatusCode, msg: &str) -> ApiError {
    (status, Json(serde_json::json!({"error": msg})))
}

/// The kinds a route can run on today; the others are refused with a plain
/// reason rather than kept and silently left on the hub's route.
const ROUTABLE: [PersonalRouteKind; 1] = [PersonalRouteKind::ClaudeCode];

/// The person's every-agent setting as they see it.
#[derive(Debug, Serialize)]
pub struct MyRouteView {
    /// Whether this hub runs people's work on their own subscriptions.
    available: bool,
    /// The kinds they may pick here.
    allow: Vec<PersonalRouteKind>,
    route: Option<PersonalRoute>,
}

/// The person's setting for one agent as they see it.
#[derive(Debug, Serialize)]
pub struct MyAgentRouteView {
    agent_id: String,
    /// Whether the agent runs people's work on their own subscriptions.
    available: bool,
    allow: Vec<PersonalRouteKind>,
    /// Their setting for this agent: a route, `"hub"`, or none (as in their
    /// every-agent setting).
    route: Option<AgentRoute>,
    every_agent: Option<PersonalRoute>,
    /// The route their work on this agent takes; none is the hub's.
    effective: Option<PersonalRoute>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteWrite<T> {
    /// The new setting; none clears it.
    #[serde(default = "none")]
    route: Option<T>,
}

fn none<T>() -> Option<T> {
    None
}

fn routes(state: &GatewayState) -> Result<PersonalRoutes, ApiError> {
    state
        .registry
        .host()
        .map(|host| host.subscriptions().personal_routes().clone())
        .ok_or_else(|| err(StatusCode::SERVICE_UNAVAILABLE, "no agents on this hub"))
}

fn store(routes: &PersonalRoutes) -> Result<Arc<dyn PersonRouteStore>, ApiError> {
    routes.store().ok_or_else(|| {
        err(
            StatusCode::SERVICE_UNAVAILABLE,
            "this hub keeps no route settings (it has no Postgres)",
        )
    })
}

fn store_failed(e: eyre::Report) -> ApiError {
    tracing::error!(error = %format!("{e:#}"), "route settings store failed");
    err(
        StatusCode::INTERNAL_SERVER_ERROR,
        "the route settings could not be read or saved",
    )
}

fn agent_for(
    state: &GatewayState,
    claims: crate::jwt::Claims,
    agent_id: &str,
) -> Result<Arc<AgentService>, ApiError> {
    state
        .registry
        .get(agent_id)
        .filter(|_| AgentViewer::new(state, claims.into()).can_see(agent_id))
        .ok_or_else(|| crate::gateway::not_found_agent(agent_id))
}

/// The switch as it stands for `service`: the hub's, narrowed by the
/// agent's own `[llm.personal_routes]`.
fn switch_for(routes: &PersonalRoutes, service: &AgentService) -> PersonalRoutesConfig {
    routes.for_agent(service.resolved.snapshot().personal_routes.as_ref())
}

/// Whether `person` has a working login of their own for `kind`, at one of
/// `tiers` in `vaults`: one at its usage limit still counts; one never set,
/// or refused, does not.
fn has_own_login(
    kind: PersonalRouteKind,
    vaults: &Vaults<'_>,
    tiers: &[CredentialTier],
    person: Uuid,
) -> bool {
    let Some(kind) = kind.login_kind() else {
        return false;
    };
    let now = Utc::now();
    tiers.iter().any(|&tier| {
        matches!(
            login_state(
                kind,
                tier,
                vaults,
                Some(person),
                &AgentOwner::Unowned,
                CooldownStore::global(),
                now,
            ),
            Ok(_) | Err(Miss::Cooling { .. })
        )
    })
}

/// Why `route` can't be chosen where `switch` holds, the person holding
/// a working login of its kind or not; `None` when it can.
fn refusal(route: PersonalRoute, switch: &PersonalRoutesConfig, has_login: bool) -> Option<String> {
    let title = route.kind.title();
    if !switch.enabled {
        return Some("this hub doesn't run people's work on their own subscriptions".into());
    }
    if !switch.allow.contains(&route.kind) {
        return Some(format!(
            "running your work on your {title} isn't offered here"
        ));
    }
    if !ROUTABLE.contains(&route.kind) {
        return Some(format!(
            "running your work on your {title} isn't available yet"
        ));
    }
    if !has_login {
        return Some(format!(
            "connect your own {title} login in My Settings first"
        ));
    }
    None
}

fn refused(reason: String) -> ApiError {
    (
        StatusCode::CONFLICT,
        Json(serde_json::json!({"error": reason})),
    )
}

/// `GET /api/me/llm-route` — the caller's setting for every agent.
pub async fn get_my_route(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
) -> ApiResult<MyRouteView> {
    let (person, _) = super::identity::require_person(&state, &headers)?;
    let routes = routes(&state)?;
    let switch = routes.switch();
    let route = match routes.store() {
        Some(store) => {
            store
                .settings(person, None)
                .await
                .map_err(store_failed)?
                .every_agent
        }
        None => None,
    };
    Ok(Json(MyRouteView {
        available: switch.enabled && routes.store().is_some(),
        allow: switch.allow,
        route,
    }))
}

/// `PUT /api/me/llm-route` — sets or clears the caller's setting for every
/// agent. A route needs the caller's own login of its kind for the whole
/// hub (in My Settings), since it serves every agent.
pub async fn put_my_route(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    Json(write): Json<RouteWrite<PersonalRoute>>,
) -> ApiResult<MyRouteView> {
    let (person, _) = super::identity::require_person(&state, &headers)?;
    let routes = routes(&state)?;
    let store = store(&routes)?;
    let switch = routes.switch();
    if let Some(route) = write.route {
        let global = Vaults {
            agent: None,
            global: state.vault.as_ref(),
        };
        let has_login = has_own_login(route.kind, &global, &[CredentialTier::GlobalUser], person);
        if let Some(reason) = refusal(route, &switch, has_login) {
            return Err(refused(reason));
        }
    }
    let actor = person.to_string();
    let before = store
        .set_every_agent(person, write.route, &actor)
        .await
        .map_err(store_failed)?;
    routes.forget(person);
    tracing::info!(
        target: "llm_route",
        person = %person,
        before = ?before,
        after = ?write.route,
        "a person's every-agent route changed"
    );
    Ok(Json(MyRouteView {
        available: switch.enabled,
        allow: switch.allow,
        route: write.route,
    }))
}

async fn agent_view(
    store: Option<&Arc<dyn PersonRouteStore>>,
    switch: PersonalRoutesConfig,
    person: Uuid,
    agent_id: &str,
) -> ApiResult<MyAgentRouteView> {
    let settings = match store {
        Some(store) => store
            .settings(person, Some(agent_id))
            .await
            .map_err(store_failed)?,
        None => Default::default(),
    };
    let effective = settings
        .chosen()
        .filter(|route| switch.admits(route.kind) && ROUTABLE.contains(&route.kind));
    Ok(Json(MyAgentRouteView {
        agent_id: agent_id.to_string(),
        available: switch.enabled && store.is_some(),
        allow: switch.allow,
        route: settings.this_agent,
        every_agent: settings.every_agent,
        effective,
    }))
}

/// `GET /api/me/agents/{id}/llm-route` — the caller's setting for one
/// agent, their every-agent one, and the route their work there takes.
pub async fn get_my_agent_route(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    Path(agent_id): Path<String>,
) -> ApiResult<MyAgentRouteView> {
    let (person, claims) = super::identity::require_person(&state, &headers)?;
    let service = agent_for(&state, claims, &agent_id)?;
    let routes = routes(&state)?;
    agent_view(
        routes.store().as_ref(),
        switch_for(&routes, &service),
        person,
        &agent_id,
    )
    .await
}

/// `PUT /api/me/agents/{id}/llm-route` — sets the caller's setting for one
/// agent (a route, or `"hub"` to keep the agent on the hub's route), or
/// clears it back to their every-agent setting.
pub async fn put_my_agent_route(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    Path(agent_id): Path<String>,
    Json(write): Json<RouteWrite<AgentRoute>>,
) -> ApiResult<MyAgentRouteView> {
    let (person, claims) = super::identity::require_person(&state, &headers)?;
    let service = agent_for(&state, claims, &agent_id)?;
    let routes = routes(&state)?;
    let store = store(&routes)?;
    let switch = switch_for(&routes, &service);
    if let Some(AgentRoute::Personal(route)) = write.route {
        let has_login = has_own_login(
            route.kind,
            &service.logins.vaults(),
            &[CredentialTier::AgentUser, CredentialTier::GlobalUser],
            person,
        );
        if let Some(reason) = refusal(route, &switch, has_login) {
            return Err(refused(reason));
        }
    }
    let actor = person.to_string();
    let before = store
        .set_for_agent(person, &agent_id, write.route, &actor)
        .await
        .map_err(store_failed)?;
    routes.forget(person);
    tracing::info!(
        target: "llm_route",
        person = %person,
        agent = %agent_id,
        before = ?before,
        after = ?write.route,
        "a person's route for an agent changed"
    );
    agent_view(Some(&store), switch, person, &agent_id).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use workforce_common::personal_route::RouteFallback;

    const CLAUDE: PersonalRoute = PersonalRoute {
        kind: PersonalRouteKind::ClaudeCode,
        fallback: RouteFallback::Wait,
    };

    fn on() -> PersonalRoutesConfig {
        PersonalRoutesConfig {
            enabled: true,
            ..Default::default()
        }
    }

    #[test]
    fn a_route_is_refused_off_unoffered_unbuilt_or_without_a_login() {
        assert_eq!(refusal(CLAUDE, &on(), true), None);
        assert!(refusal(CLAUDE, &PersonalRoutesConfig::default(), true)
            .unwrap()
            .contains("doesn't run"));
        let codex_only = PersonalRoutesConfig {
            enabled: true,
            allow: vec![PersonalRouteKind::Codex],
        };
        assert!(refusal(CLAUDE, &codex_only, true)
            .unwrap()
            .contains("isn't offered"));
        for kind in [PersonalRouteKind::Codex, PersonalRouteKind::Devin] {
            let route = PersonalRoute {
                kind,
                fallback: RouteFallback::Hub,
            };
            assert!(refusal(route, &on(), true)
                .unwrap()
                .contains("isn't available yet"));
        }
        assert!(refusal(CLAUDE, &on(), false)
            .unwrap()
            .contains("connect your own Claude login"));
    }

    #[test]
    fn a_write_with_no_route_clears_the_setting() {
        let clear: RouteWrite<AgentRoute> = serde_json::from_str("{}").unwrap();
        assert_eq!(clear.route, None);
        let null: RouteWrite<AgentRoute> = serde_json::from_str(r#"{"route":null}"#).unwrap();
        assert_eq!(null.route, None);
        let hub: RouteWrite<AgentRoute> = serde_json::from_str(r#"{"route":"hub"}"#).unwrap();
        assert_eq!(hub.route, Some(AgentRoute::Hub));
        assert!(serde_json::from_str::<RouteWrite<PersonalRoute>>(r#"{"route":"hub"}"#).is_err());
        assert!(serde_json::from_str::<RouteWrite<AgentRoute>>(r#"{"rout":"hub"}"#).is_err());
    }
}
