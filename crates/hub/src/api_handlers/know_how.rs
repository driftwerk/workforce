//! Admin know-how API: an agent's learned entries in full (never intents or
//! who produced them), their review, and erasing one person's
//! contributions.
//!
//! - `GET    /api/agents/{id}/know-how`                  — every live entry.
//! - `POST   /api/agents/{id}/know-how/{entry}/pin`      — show it to everyone.
//! - `POST   /api/agents/{id}/know-how/{entry}/retire`   — remove it for good.
//! - `DELETE /api/agents/{id}/know-how/{entry}`          — delete it; turns may learn it again.
//! - `DELETE /api/identities/{id}/know-how`              — erase what one identity contributed, on every agent.
//!
//! Every route takes the owner token or a system admin's JWT
//! ([`crate::gateway::require_admin_action`]). The review routes answer 503
//! while the install has know-how off; erasing works regardless, since what
//! was learned before stays stored.

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use serde_json::json;
use workforce_common::settings::{KnowHowConfig, ProgramsMode};

use super::errors::ApiError;
use super::resolve_agent;
use crate::gateway::{require_admin_action, GatewayState, WsQuery};
use crate::know_how::KnowHow;
use crate::serve::AgentService;

type ApiResult = Result<Json<serde_json::Value>, ApiError>;

fn off() -> ApiError {
    ApiError::new(
        StatusCode::SERVICE_UNAVAILABLE,
        "know-how is off on this install",
    )
}

fn failed(e: eyre::Report) -> ApiError {
    tracing::warn!(target: "know_how", error = %e, "know-how admin request failed");
    ApiError::internal("know-how store error")
}

/// The agent, its know-how config (the install's when the agent has not
/// turned it on: entries it learned before still need review) and the store.
async fn agent_know_how(
    state: &GatewayState,
    agent: &str,
) -> Result<(Arc<AgentService>, KnowHowConfig, Arc<KnowHow>), ApiError> {
    let service = resolve_agent(state, agent)?;
    if service.config.know_how.mode == ProgramsMode::Off {
        return Err(off());
    }
    let cfg = service
        .know_how()
        .unwrap_or_else(|| service.config.know_how.clone());
    let kh = KnowHow::shared(&service.config).await;
    Ok((service, cfg, kh))
}

/// `GET /api/agents/{id}/know-how`
pub async fn list_entries(
    Path(agent): Path<String>,
    Query(query): Query<WsQuery>,
    headers: HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> ApiResult {
    require_admin_action(&state, &headers, query.token.as_deref()).await?;
    let (_, cfg, kh) = agent_know_how(&state, &agent).await?;
    let entries = kh.admin_entries(&agent, &cfg).await.map_err(failed)?;
    Ok(Json(json!({
        "agent_id": agent,
        "min_users": cfg.min_users,
        "entries": entries,
    })))
}

/// `POST /api/agents/{id}/know-how/{entry}/pin`
pub async fn pin_entry(
    Path((agent, entry)): Path<(String, i64)>,
    Query(query): Query<WsQuery>,
    headers: HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> ApiResult {
    require_admin_action(&state, &headers, query.token.as_deref()).await?;
    let (_, cfg, kh) = agent_know_how(&state, &agent).await?;
    match kh.pin(&agent, &cfg, entry).await.map_err(failed)? {
        true => Ok(Json(json!({ "id": entry, "done": "pin" }))),
        false => Err(ApiError::conflict(
            "only an entry others already see, or an addition awaiting review, can be pinned",
        )),
    }
}

/// `POST /api/agents/{id}/know-how/{entry}/retire`
pub async fn retire_entry(
    Path((agent, entry)): Path<(String, i64)>,
    Query(query): Query<WsQuery>,
    headers: HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> ApiResult {
    require_admin_action(&state, &headers, query.token.as_deref()).await?;
    let (_, _, kh) = agent_know_how(&state, &agent).await?;
    match kh.retire(&agent, entry).await.map_err(failed)? {
        true => Ok(Json(json!({ "id": entry, "done": "retire" }))),
        false => Err(ApiError::not_found("entry not found")),
    }
}

/// `DELETE /api/agents/{id}/know-how/{entry}`
pub async fn delete_entry(
    Path((agent, entry)): Path<(String, i64)>,
    Query(query): Query<WsQuery>,
    headers: HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> ApiResult {
    require_admin_action(&state, &headers, query.token.as_deref()).await?;
    let (_, _, kh) = agent_know_how(&state, &agent).await?;
    match kh.delete(&agent, entry).await.map_err(failed)? {
        true => Ok(Json(json!({ "id": entry, "done": "delete" }))),
        false => Err(ApiError::not_found("entry not found")),
    }
}

/// `DELETE /api/identities/{id}/know-how`
pub async fn erase_identity(
    Path(identity): Path<String>,
    Query(query): Query<WsQuery>,
    headers: HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> ApiResult {
    require_admin_action(&state, &headers, query.token.as_deref()).await?;
    let identity = uuid::Uuid::parse_str(&identity)
        .map_err(|_| ApiError::bad_request("invalid identity id"))?;
    let service = state
        .registry
        .all_services()
        .into_iter()
        .next()
        .ok_or_else(ApiError::service_unavailable)?;
    let kh = KnowHow::shared(&service.config).await;
    let touched = kh
        .forget_identity(&identity.to_string())
        .await
        .map_err(failed)?;
    Ok(Json(
        json!({ "identity_id": identity, "entries_touched": touched }),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::serve::Compose as _;
    use axum::response::IntoResponse;

    #[tokio::test]
    async fn every_route_needs_an_admin() {
        let state = crate::remote_mcp::testing::fixture(false).state;
        let no_token = || {
            Query(WsQuery {
                token: None,
                agent_id: None,
                channel: None,
                rich: None,
                desk: None,
                accept: None,
            })
        };
        let status = |r: ApiResult| r.unwrap_err().into_response().status();
        assert_eq!(
            status(
                list_entries(
                    Path("a".into()),
                    no_token(),
                    HeaderMap::new(),
                    State(Arc::clone(&state))
                )
                .await
            ),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            status(
                pin_entry(
                    Path(("a".into(), 1)),
                    no_token(),
                    HeaderMap::new(),
                    State(Arc::clone(&state))
                )
                .await
            ),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            status(
                retire_entry(
                    Path(("a".into(), 1)),
                    no_token(),
                    HeaderMap::new(),
                    State(Arc::clone(&state))
                )
                .await
            ),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            status(
                delete_entry(
                    Path(("a".into(), 1)),
                    no_token(),
                    HeaderMap::new(),
                    State(Arc::clone(&state))
                )
                .await
            ),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            status(
                erase_identity(
                    Path(uuid::Uuid::new_v4().to_string()),
                    no_token(),
                    HeaderMap::new(),
                    State(Arc::clone(&state))
                )
                .await
            ),
            StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn an_unknown_agent_or_identity_is_refused() {
        let state = crate::remote_mcp::testing::fixture(false).state;
        let token = || {
            Query(WsQuery {
                token: Some("gateway-token".into()),
                agent_id: None,
                channel: None,
                rich: None,
                desk: None,
                accept: None,
            })
        };
        let status = |r: ApiResult| r.unwrap_err().into_response().status();
        assert_eq!(
            status(
                list_entries(
                    Path("nobody".into()),
                    token(),
                    HeaderMap::new(),
                    State(Arc::clone(&state))
                )
                .await
            ),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            status(
                erase_identity(
                    Path("not-a-uuid".into()),
                    token(),
                    HeaderMap::new(),
                    State(Arc::clone(&state))
                )
                .await
            ),
            StatusCode::BAD_REQUEST
        );
    }

    #[tokio::test]
    async fn with_know_how_off_review_is_refused_but_erasing_still_works() {
        let state = crate::remote_mcp::testing::fixture(false).state;
        let tmp = tempfile::tempdir().unwrap();
        let mut cfg = workforce_common::settings::WorkforceConfig::default();
        cfg.hub.data_dir = tmp.path().to_string_lossy().to_string();
        cfg.memory.db_path = std::path::Path::new(&cfg.hub.data_dir)
            .join("memory.db")
            .to_string_lossy()
            .into_owned();
        cfg.llm.api_key = Some("test-key".into());
        cfg.know_how.mode = ProgramsMode::Off;
        let service = AgentService::from_config(cfg, None)
            .await
            .expect("test service");
        state.registry.insert("a", service);
        let token = || {
            Query(WsQuery {
                token: Some("gateway-token".into()),
                agent_id: None,
                channel: None,
                rich: None,
                desk: None,
                accept: None,
            })
        };
        let listed = list_entries(
            Path("a".into()),
            token(),
            HeaderMap::new(),
            State(Arc::clone(&state)),
        )
        .await;
        assert_eq!(
            listed.unwrap_err().into_response().status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
        let erased = erase_identity(
            Path(uuid::Uuid::new_v4().to_string()),
            token(),
            HeaderMap::new(),
            State(Arc::clone(&state)),
        )
        .await
        .expect("erasing works while know-how is off");
        assert_eq!(erased.0["entries_touched"], 0);
    }
}
