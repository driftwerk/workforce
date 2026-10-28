//! `GET /api/desk/conversations/{agent}`: a person's direct conversation
//! with one agent, as Desk shows it after a restart. The hub reads it from
//! the session the `desk` channel keeps for that person and agent, and only
//! the person it belongs to may read it, so the hub decides who is asking
//! from the request itself.

use std::sync::Arc;

use async_trait::async_trait;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::json;

/// The most messages one page holds, and how many a page holds unasked.
pub const PAGE_MAX: usize = 200;

/// Who said a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Author {
    User,
    Agent,
}

/// One message of the conversation. `text` is what the live `desk` channel
/// delivers as a finished reply's `content`: its rich blocks are fenced
/// inside it, and secrets read as they read live.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Said {
    pub id: String,
    pub from: Author,
    pub text: String,
    pub at: DateTime<Utc>,
    /// What an overseer's message mentions to act on in place.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub desk: Option<crate::crew::Extras>,
}

/// A page of the conversation, oldest first; `more` when older messages
/// remain before its first.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Page {
    pub messages: Vec<Said>,
    pub more: bool,
}

/// Why a conversation could not be read. A message never carries what the
/// conversation says.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConversationError {
    #[error("not signed in")]
    Unauthorized,
    #[error("{0}")]
    Forbidden(String),
    #[error("{0}")]
    NotFound(String),
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    Unavailable(String),
    #[error("{0}")]
    Internal(String),
}

impl IntoResponse for ConversationError {
    fn into_response(self) -> Response {
        let status = match &self {
            Self::Unauthorized => StatusCode::UNAUTHORIZED,
            Self::Forbidden(_) => StatusCode::FORBIDDEN,
            Self::NotFound(_) => StatusCode::NOT_FOUND,
            Self::Invalid(_) => StatusCode::BAD_REQUEST,
            Self::Unavailable(_) => StatusCode::SERVICE_UNAVAILABLE,
            Self::Internal(e) => {
                tracing::error!(error = %e, "desk conversation read failed");
                StatusCode::INTERNAL_SERVER_ERROR
            }
        };
        (status, Json(json!({ "error": self.to_string() }))).into_response()
    }
}

/// The hub's side: whose conversation a request asks for, and the page.
#[async_trait]
pub trait Conversations: Send + Sync {
    /// The page of the caller's conversation with `agent` that ends just
    /// before the message `before` (the newest page without one), holding at
    /// most `limit` messages.
    async fn page(
        &self,
        headers: &HeaderMap,
        token: Option<&str>,
        agent: &str,
        before: Option<&str>,
        limit: usize,
    ) -> Result<Page, ConversationError>;
}

#[derive(Deserialize)]
struct PageQuery {
    token: Option<String>,
    before: Option<String>,
    limit: Option<usize>,
}

async fn conversation(
    State(c): State<Arc<dyn Conversations>>,
    headers: HeaderMap,
    Path(agent): Path<String>,
    Query(q): Query<PageQuery>,
) -> Response {
    let limit = q.limit.unwrap_or(PAGE_MAX).clamp(1, PAGE_MAX);
    let before = q.before.as_deref().filter(|b| !b.trim().is_empty());
    c.page(&headers, q.token.as_deref(), &agent, before, limit)
        .await
        .map(Json)
        .into_response()
}

/// `GET /api/desk/conversations/{agent}` on `conversations`.
pub fn router(conversations: Arc<dyn Conversations>) -> Router {
    Router::new()
        .route("/api/desk/conversations/{agent}", get(conversation))
        .with_state(conversations)
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use axum::body::{to_bytes, Body};
    use axum::http::Request;
    use serde_json::Value;
    use tower::ServiceExt;

    use super::*;

    /// Answers one fixed message, and remembers what it was asked.
    #[derive(Default)]
    struct Asked(Mutex<Vec<(String, Option<String>, usize)>>);

    #[async_trait]
    impl Conversations for Asked {
        async fn page(
            &self,
            headers: &HeaderMap,
            token: Option<&str>,
            agent: &str,
            before: Option<&str>,
            limit: usize,
        ) -> Result<Page, ConversationError> {
            let bearer = headers
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.strip_prefix("Bearer "));
            if bearer.or(token) != Some("device-token") {
                return Err(ConversationError::Unauthorized);
            }
            if agent == "nobody" {
                return Err(ConversationError::NotFound("no agent named nobody".into()));
            }
            self.0
                .lock()
                .unwrap()
                .push((agent.into(), before.map(str::to_string), limit));
            Ok(Page {
                messages: vec![Said {
                    id: "m1".into(),
                    from: Author::Agent,
                    text: "Hello".into(),
                    at: "2026-10-28T09:00:00Z".parse().unwrap(),
                    desk: None,
                }],
                more: true,
            })
        }
    }

    async fn get(asked: Arc<Asked>, uri: &str, auth: Option<&str>) -> (StatusCode, Value) {
        let mut request = Request::get(uri);
        if let Some(auth) = auth {
            request = request.header("authorization", auth);
        }
        let response = router(asked)
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), 1 << 20).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    #[tokio::test]
    async fn a_page_is_the_messages_oldest_first_and_whether_more_remain() {
        let asked = Arc::new(Asked::default());
        let (status, body) = get(
            Arc::clone(&asked),
            "/api/desk/conversations/scout?limit=50&before=m9",
            Some("Bearer device-token"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body,
            json!({
                "messages": [{
                    "id": "m1", "from": "agent", "text": "Hello",
                    "at": "2026-10-28T09:00:00Z",
                }],
                "more": true,
            })
        );
        assert_eq!(
            asked.0.lock().unwrap()[0],
            ("scout".into(), Some("m9".into()), 50)
        );
    }

    #[tokio::test]
    async fn the_limit_is_held_to_a_page_and_a_blank_cursor_is_none() {
        let asked = Arc::new(Asked::default());
        for (query, limit) in [("", PAGE_MAX), ("?limit=0", 1), ("?limit=9000", PAGE_MAX)] {
            let uri = format!("/api/desk/conversations/scout{query}");
            let (status, _) = get(Arc::clone(&asked), &uri, Some("Bearer device-token")).await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(asked.0.lock().unwrap().pop().unwrap().2, limit, "{query}");
        }
        get(
            Arc::clone(&asked),
            "/api/desk/conversations/scout?before=&token=device-token",
            None,
        )
        .await;
        assert_eq!(asked.0.lock().unwrap().pop().unwrap().1, None);
    }

    #[tokio::test]
    async fn the_hubs_refusals_are_answered_as_they_are() {
        let asked = Arc::new(Asked::default());
        let (status, _) = get(Arc::clone(&asked), "/api/desk/conversations/scout", None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        let (status, body) = get(
            asked,
            "/api/desk/conversations/nobody",
            Some("Bearer device-token"),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["error"], "no agent named nobody");
    }
}
