//! Teams inbound receiver: `POST /api/messages`.
//!
//! The Azure Bot Connector pushes every Teams activity here — unlike Slack,
//! which the hub reaches over a socket. The route is a public sibling of the
//! gateway (no user-auth middleware); the request carries its own Bot Connector
//! JWT, which [`BotConnectorValidator`] verifies against the activity's
//! `serviceUrl` before we act on it. Any auth failure returns `403` before
//! dispatch.
//!
//! A verified [`Activity`] is routed by [`TeamsEventHandler`]:
//! - `message` → handed to the per-app echo loop, request answered `200`.
//! - `invoke`  → answered synchronously with the reserved [`InvokeResponse`]
//!   (Phase 0 has no interactive handlers; Phase 2 / DW-560 fills this in).
//! - anything else → `200`, nothing dispatched.

use std::collections::HashMap;
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use tokio::sync::mpsc;
use tracing::warn;

use workforce_skills::adapters::teams::{
    Activity, BotConnectorValidator, InvokeResponse, TeamsAction, TeamsDisposition,
    TeamsEventHandler, TeamsSurface,
};

/// One bot registration's inbound plumbing: the `aud` to enforce, the surface
/// whose serviceUrl cache an inbound activity primes, the activity router, and
/// the queue feeding its dispatch loop.
pub struct TeamsEndpoint {
    /// Microsoft App ID — the JWT `aud` the inbound auth enforces.
    pub app_id: String,
    pub surface: Arc<TeamsSurface>,
    pub handler: Arc<TeamsEventHandler>,
    pub action_tx: mpsc::Sender<TeamsAction>,
}

/// State for the `/api/messages` receiver: the shared connector-token validator
/// and every configured app, keyed by Microsoft App ID.
pub struct TeamsRouterState {
    pub validator: Arc<BotConnectorValidator>,
    pub apps: HashMap<String, TeamsEndpoint>,
}

/// Build the Teams inbound receiver router.
pub fn router(state: Arc<TeamsRouterState>) -> Router {
    Router::new()
        .route("/api/messages", post(receive_messages))
        .with_state(state)
}

async fn receive_messages(
    State(state): State<Arc<TeamsRouterState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Ok(activity) = serde_json::from_slice::<Activity>(&body) else {
        return (StatusCode::BAD_REQUEST, "invalid activity").into_response();
    };
    let recipient = activity.recipient.as_ref().and_then(|r| r.id.as_deref());
    let Some(endpoint) = resolve_app(recipient, &state.apps) else {
        return (StatusCode::UNAUTHORIZED, "unknown bot recipient").into_response();
    };
    let Some(token) = bearer_token(&headers) else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    let service_url = activity.service_url.clone().unwrap_or_default();
    if let Err(e) = state
        .validator
        .validate(&token, &endpoint.app_id, &service_url)
        .await
    {
        warn!(error = %e, "Teams inbound auth failed");
        return StatusCode::FORBIDDEN.into_response();
    }
    // Remember this conversation's serviceUrl so the surface can address an
    // outbound reply — an inbound activity is the only place it is exposed.
    if let Some(conversation_id) = activity.conversation.as_ref().and_then(|c| c.id.as_deref()) {
        if !service_url.is_empty() {
            endpoint
                .surface
                .record_service_url(conversation_id, &service_url);
        }
    }
    respond(endpoint.handler.handle(activity).await, &endpoint.action_tx)
}

/// Route an activity to its app by the recipient bot id (`28:<app-id>`). A
/// single-app deployment needs no match — the token's `aud` + signature already
/// bind the request to that app.
fn resolve_app<'a>(
    recipient_id: Option<&str>,
    apps: &'a HashMap<String, TeamsEndpoint>,
) -> Option<&'a TeamsEndpoint> {
    if let Some(id) = recipient_id {
        let bare = id.strip_prefix("28:").unwrap_or(id);
        if let Some(endpoint) = apps.get(bare) {
            return Some(endpoint);
        }
    }
    (apps.len() == 1).then(|| apps.values().next()).flatten()
}

/// Turn a routed disposition into the HTTP response: `message` work is enqueued
/// and answered `200`; `invoke` returns its synchronous body; everything else is
/// a bare `200`. Kept pure of the validator so it is unit-testable without the
/// network (the `403` path is exercised by the live echo test; the token
/// verification itself is unit-tested in the adapter's `auth.rs`).
fn respond(disposition: TeamsDisposition, action_tx: &mpsc::Sender<TeamsAction>) -> Response {
    match disposition {
        TeamsDisposition::Async(action) => {
            if let Err(e) = action_tx.try_send(action) {
                warn!(error = %e, "Teams dispatch queue unavailable; message dropped");
            }
            StatusCode::OK.into_response()
        }
        TeamsDisposition::Invoke(_) => {
            (StatusCode::OK, Json(InvokeResponse::not_implemented())).into_response()
        }
        TeamsDisposition::Ignore => StatusCode::OK.into_response(),
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use workforce_skills::adapters::teams::{
        ConversationAccount, ConversationReference, InboundMessage, InvokeActivity, TeamsIdentity,
    };

    fn endpoint(app_id: &str) -> TeamsEndpoint {
        let (action_tx, _rx) = mpsc::channel(8);
        TeamsEndpoint {
            app_id: app_id.to_string(),
            surface: Arc::new(TeamsSurface::new(app_id, "secret")),
            handler: Arc::new(TeamsEventHandler::new()),
            action_tx,
        }
    }

    fn apps(ids: &[&str]) -> HashMap<String, TeamsEndpoint> {
        ids.iter()
            .map(|id| (id.to_string(), endpoint(id)))
            .collect()
    }

    fn process_action() -> TeamsAction {
        TeamsAction::ProcessMessage(InboundMessage {
            identity: TeamsIdentity::default(),
            channel: "teams:19:c".into(),
            text: "hi".into(),
            activity_id: "a1".into(),
            personal: false,
            reference: ConversationReference {
                activity_id: None,
                user: None,
                bot: None,
                conversation: ConversationAccount::default(),
                channel_id: None,
                service_url: "https://smba.example/".into(),
            },
        })
    }

    fn invoke_disposition() -> TeamsDisposition {
        TeamsDisposition::Invoke(InvokeActivity {
            name: "adaptiveCard/action".into(),
            value: serde_json::Value::Null,
            identity: TeamsIdentity::default(),
            reference: ConversationReference {
                activity_id: None,
                user: None,
                bot: None,
                conversation: ConversationAccount::default(),
                channel_id: None,
                service_url: "https://smba.example/".into(),
            },
        })
    }

    #[test]
    fn resolve_app_matches_recipient_bot_id() {
        let apps = apps(&["app-a", "app-b"]);
        let ep = resolve_app(Some("28:app-b"), &apps).expect("matched");
        assert_eq!(ep.app_id, "app-b");
    }

    #[test]
    fn resolve_app_tolerates_bare_recipient_id() {
        let apps = apps(&["app-a", "app-b"]);
        let ep = resolve_app(Some("app-a"), &apps).expect("matched");
        assert_eq!(ep.app_id, "app-a");
    }

    #[test]
    fn resolve_app_falls_back_to_sole_app() {
        let apps = apps(&["only"]);
        assert!(resolve_app(None, &apps).is_some());
        assert!(resolve_app(Some("28:mismatch"), &apps).is_some());
    }

    #[test]
    fn resolve_app_requires_match_when_multiple() {
        let apps = apps(&["app-a", "app-b"]);
        assert!(resolve_app(None, &apps).is_none());
        assert!(resolve_app(Some("28:unknown"), &apps).is_none());
    }

    #[test]
    fn respond_enqueues_async_and_answers_ok() {
        let (tx, mut rx) = mpsc::channel(8);
        let response = respond(TeamsDisposition::Async(process_action()), &tx);
        assert_eq!(response.status(), StatusCode::OK);
        assert!(matches!(rx.try_recv(), Ok(TeamsAction::ProcessMessage(_))));
    }

    #[test]
    fn respond_ignore_is_bare_ok() {
        let (tx, mut rx) = mpsc::channel(8);
        let response = respond(TeamsDisposition::Ignore, &tx);
        assert_eq!(response.status(), StatusCode::OK);
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn respond_invoke_returns_not_implemented_body() {
        let (tx, _rx) = mpsc::channel(8);
        let response = respond(invoke_disposition(), &tx);
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let parsed: InvokeResponse = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(parsed.status, 501);
    }

    fn state(ids: &[&str]) -> Arc<TeamsRouterState> {
        Arc::new(TeamsRouterState {
            validator: Arc::new(BotConnectorValidator::new()),
            apps: apps(ids),
        })
    }

    fn activity_body(recipient_id: &str) -> Bytes {
        Bytes::from(
            serde_json::to_vec(&serde_json::json!({
                "type": "message",
                "id": "a1",
                "serviceUrl": "https://smba.example/",
                "from": { "id": "29:user" },
                "recipient": { "id": recipient_id },
                "conversation": { "id": "19:c" },
                "text": "hi"
            }))
            .unwrap(),
        )
    }

    // The receiver's pre-validate branches short-circuit before the validator is
    // ever consulted, so they are exercisable without the network.

    #[tokio::test]
    async fn invalid_body_is_bad_request() {
        let response = receive_messages(
            State(state(&["app-a"])),
            HeaderMap::new(),
            Bytes::from("not json"),
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn unknown_recipient_is_unauthorized() {
        // Two apps so the single-app fallback does not apply.
        let response = receive_messages(
            State(state(&["app-a", "app-b"])),
            HeaderMap::new(),
            activity_body("28:nope"),
        )
        .await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn missing_bearer_is_unauthorized() {
        // Single app resolves via fallback; the absent Authorization header
        // short-circuits before validation.
        let response = receive_messages(
            State(state(&["app-a"])),
            HeaderMap::new(),
            activity_body("28:app-a"),
        )
        .await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }
}
