//! HTTP trigger receiver: `POST /triggers/{path}` authenticates a caller and
//! starts the workflow the path resolves to, either streaming its progress
//! back (SSE) or returning the instance id and running it detached.
//!
//! A trigger answers on a minted path (`t.{hex}`), which is unaffected by who
//! owns the workflow, and on the owner-derived path it has always answered on
//! — bare `{trigger_id}` for a system definition, `{owner}/{trigger_id}` for a
//! member one. The two forms are structurally disjoint, so one index resolves
//! both; the `form` log field records which one a caller used.
//!
//! Authentication is verified before any workflow work begins; every failure
//! mode returns first. Shared matching/secret primitives live in
//! [`crate::trigger_auth`] so this path and the webhook path do not diverge.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use tokio::sync::broadcast;
use tracing::{info, warn};

use workforce_orchestrator::workflow::definition::{
    HttpResponseMode, TriggerAuth, TriggerDefinition,
};
use workforce_orchestrator::workflow::instance::{TriggerInfo, TriggerSource};

use crate::hub_core::workflows::WorkflowStreamEvent;
use crate::serve::AgentService;
use crate::trigger_auth::{
    any_vault_secret_satisfies, hmac_sha256_header_matches, match_rules_pass, secrets_match,
    trigger_secret_stores,
};
use crate::trigger_oidc::{resolve_run_as, OidcVerifier};
use crate::workflow_runtime::trigger_path_form;

/// State for the HTTP trigger router.
pub struct TriggerHttpState {
    pub service: Arc<AgentService>,
    pub runtime: Arc<dyn crate::hosts::WorkflowHost>,
    pub oidc: Arc<OidcVerifier>,
}

/// Build the HTTP trigger receiver router.
pub fn router(state: Arc<TriggerHttpState>) -> Router {
    Router::new()
        // One wildcard resolves all three forms — minted `/triggers/t.{hex}`,
        // bare `/triggers/{id}` and owner-namespaced `/triggers/{owner}/{id}`.
        // A second, more specific route would put the compatibility guarantee
        // at the mercy of routing precedence.
        .route("/triggers/{*id}", post(receive_trigger))
        .with_state(state)
}

async fn receive_trigger(
    State(state): State<Arc<TriggerHttpState>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Some((workflow_id, trigger)) = state.runtime.resolve_http_trigger(&id).await else {
        return (StatusCode::NOT_FOUND, "unknown trigger").into_response();
    };
    let TriggerDefinition::Http {
        auth,
        match_rules,
        response,
        ..
    } = trigger
    else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };

    let run_as = match authenticate(&state, &auth, &headers, &body).await {
        Ok(run_as) => run_as,
        Err(status) => {
            warn!(trigger_path = %id, workflow_id = %workflow_id,
                  "HTTP trigger authentication failed");
            return status.into_response();
        }
    };

    let payload: serde_json::Value = serde_json::from_slice(&body)
        .unwrap_or_else(|_| serde_json::json!({"raw": String::from_utf8_lossy(&body).to_string()}));
    if !match_rules_pass(&payload, &match_rules) {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            "trigger match rules not satisfied",
        )
            .into_response();
    }

    let trigger_info = TriggerInfo {
        source: TriggerSource::Http { id: id.clone() },
        payload,
    };

    info!(trigger_path = %id, form = %trigger_path_form(&id), workflow_id = %workflow_id,
          "HTTP trigger fired");

    match response {
        HttpResponseMode::FireForget => match Arc::clone(&state.runtime)
            .start_workflow_from_trigger(&workflow_id, trigger_info, None, None, None, run_as)
            .await
        {
            Ok(instance_id) => (
                StatusCode::ACCEPTED,
                Json(serde_json::json!({ "instance_id": instance_id })),
            )
                .into_response(),
            Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
        },
        HttpResponseMode::Streaming => match Arc::clone(&state.runtime)
            .start_workflow_streaming(&workflow_id, trigger_info, run_as)
            .await
        {
            Ok((_instance_id, rx)) => sse_response(rx),
            Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
        },
    }
}

/// Authenticate the caller per the trigger's auth mode. Returns the identity to
/// run the workflow as (only OIDC with `run_as_caller` yields one), or the
/// status to return on failure.
async fn authenticate(
    state: &TriggerHttpState,
    auth: &TriggerAuth,
    headers: &HeaderMap,
    body: &[u8],
) -> Result<Option<String>, StatusCode> {
    let stores = trigger_secret_stores(&state.service);
    match auth {
        TriggerAuth::Bearer { token } => {
            let presented = bearer_token(headers).ok_or(StatusCode::UNAUTHORIZED)?;
            verified(any_vault_secret_satisfies(&stores, token, |secret| {
                secrets_match(&presented, secret)
            }))
            .map(|()| None)
        }
        TriggerAuth::Hmac { secret } => {
            let sig = signature_header(headers).ok_or(StatusCode::UNAUTHORIZED)?;
            verified(any_vault_secret_satisfies(&stores, secret, |s| {
                hmac_sha256_header_matches(s, body, &sig)
            }))
            .map(|()| None)
        }
        TriggerAuth::Oidc(oidc) => {
            let token = bearer_token(headers).ok_or(StatusCode::UNAUTHORIZED)?;
            let caller = state
                .oidc
                .verify(&token, oidc)
                .await
                .ok_or(StatusCode::UNAUTHORIZED)?;
            if !oidc.run_as_caller {
                return Ok(None);
            }
            let run_as = state
                .service
                .identity_store
                .read()
                .clone()
                .and_then(|store| resolve_run_as(store.as_ref(), &caller));
            Ok(run_as)
        }
    }
}

fn verified(ok: bool) -> Result<(), StatusCode> {
    ok.then_some(()).ok_or(StatusCode::UNAUTHORIZED)
}

/// The `Authorization: Bearer <token>` value, if present.
fn bearer_token(headers: &HeaderMap) -> Option<String> {
    headers
        .get(axum::http::header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
        .map(str::to_string)
}

/// The `X-Signature-256: sha256=<hex>` header value, if present.
fn signature_header(headers: &HeaderMap) -> Option<String> {
    headers
        .get("x-signature-256")?
        .to_str()
        .ok()
        .map(str::to_string)
}

/// Stream workflow events to the caller as Server-Sent Events, ending the
/// stream once a terminal event is delivered. A client disconnect ends the
/// stream only — the workflow continues to completion.
fn sse_response(rx: broadcast::Receiver<WorkflowStreamEvent>) -> Response {
    let stream = futures::stream::unfold(Some(rx), |state| async move {
        let mut rx = state?;
        loop {
            match rx.recv().await {
                Ok(event) => {
                    let terminal = matches!(
                        event,
                        WorkflowStreamEvent::Completed { .. } | WorkflowStreamEvent::Failed { .. }
                    );
                    let next = if terminal { None } else { Some(rx) };
                    return Some((
                        Ok::<Event, std::convert::Infallible>(sse_event(&event)),
                        next,
                    ));
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    });
    Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response()
}

fn sse_event(event: &WorkflowStreamEvent) -> Event {
    let name = match event {
        WorkflowStreamEvent::StepStarted { .. } => "step_started",
        WorkflowStreamEvent::StepCompleted { .. } => "step_completed",
        WorkflowStreamEvent::Completed { .. } => "completed",
        WorkflowStreamEvent::Failed { .. } => "failed",
    };
    Event::default()
        .event(name)
        .json_data(event)
        .unwrap_or_else(|_| Event::default().event(name))
}
