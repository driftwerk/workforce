//! `GET /api/agents/{id}/settings` — the agent's effective settings, as the
//! `settings` tool reports them ([`crate::effective_settings`]).
//!
//! Anyone who can read the agent may ask. Install-wide rows are an admin's
//! (the owner token or a system admin's JWT); per-user rows are the caller's
//! own. `?section=` narrows to one section and `?key=` to one setting or
//! path; with neither, every section is returned.

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use serde::Deserialize;
use workforce_common::session::Session;

use super::errors::ApiError;
use crate::effective_settings::{Refusal, Report, Scope, Viewer};
use crate::gateway::{authorise_agent_reader, caller_is_admin, GatewayState};

#[derive(Debug, Deserialize)]
pub struct SettingsQuery {
    pub token: Option<String>,
    pub section: Option<String>,
    pub key: Option<String>,
}

pub async fn effective_settings(
    Path(agent): Path<String>,
    Query(query): Query<SettingsQuery>,
    headers: HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<Report>, ApiError> {
    let (service, caller) =
        authorise_agent_reader(&state, &headers, query.token.as_deref(), &agent).await?;
    let viewer = Viewer {
        admin: caller_is_admin(state.identity_store.as_ref(), &caller),
        person: caller.identity_id,
    };
    let scope = match (query.key, query.section) {
        (Some(key), _) => Scope::Key(key),
        (None, Some(section)) => Scope::Section(section),
        (None, None) => Scope::All,
    };
    let session = Session::new(
        "settings",
        viewer.person.as_deref().unwrap_or_default(),
        service.agent_id.as_str(),
    );
    crate::effective_settings::report(&service, &viewer, &session, &scope, true)
        .await
        .map(Json)
        .map_err(|refusal| {
            let status = match refusal {
                Refusal::NoIdentity => StatusCode::FORBIDDEN,
                Refusal::UnknownSection { .. } | Refusal::UnknownKey { .. } => {
                    StatusCode::NOT_FOUND
                }
            };
            ApiError::new(status, refusal.to_string())
        })
}
