//! A person's Desk conversations with the hub's agents, as Desk reads them
//! back: which changed since it last looked (`GET /api/desk/conversations`),
//! one conversation's messages either side of a cursor
//! (`GET /api/desk/conversations/{agent}`), and closing a room's
//! conversation once the room is gone
//! (`POST /api/desk/conversations/{agent}/close`). The hub reads them from
//! the sessions each Desk channel keeps for that person and agent, and only
//! the person they belong to may read them, so the hub decides who is
//! asking from the request itself.

use std::sync::Arc;

use async_trait::async_trait;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::json;
use workforce_core::desk::{DeskMark, DESK_CHANNEL};

/// The most messages one page holds, and how many a page holds unasked.
pub const PAGE_MAX: usize = 200;

/// Who said a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Author {
    User,
    Agent,
}

/// Whose a message is, as Desk shows it: the person's own (typed, or a
/// suggestion they said yes to), the overseer's (a nudge or task it sent
/// this agent), or the agent's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SpeakerKind {
    You,
    Overseer,
    Agent,
}

/// Who said a message: `{"kind": "you"|"overseer"|"agent", "id", "name"}`.
/// The person is always `{"kind": "you", "id": "you", "name": "You"}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Speaker {
    pub kind: SpeakerKind,
    pub id: String,
    pub name: String,
}

impl Speaker {
    pub fn you() -> Self {
        Self {
            kind: SpeakerKind::You,
            id: "you".into(),
            name: "You".into(),
        }
    }

    pub fn overseer(id: &str, name: &str) -> Self {
        Self {
            kind: SpeakerKind::Overseer,
            id: id.into(),
            name: name.into(),
        }
    }

    pub fn agent(id: &str, name: &str) -> Self {
        Self {
            kind: SpeakerKind::Agent,
            id: id.into(),
            name: name.into(),
        }
    }
}

/// One message of the conversation. `text` is what the live `desk` channel
/// delivers as a finished reply's `content`: its rich blocks are fenced
/// inside it, and secrets read as they read live.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Said {
    pub id: String,
    /// `user` or `agent`, as the transcript has it; [`Said::author`] says
    /// who.
    pub from: Author,
    /// Who said it: a `user` message the overseer sent on is the overseer's.
    pub author: Speaker,
    pub text: String,
    pub at: DateTime<Utc>,
    /// What an overseer's message mentions to act on in place.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub desk: Option<crate::crew::Extras>,
    /// The message's place in a Desk conversation: its room and thread, who
    /// said it, and why it was delivered outside a turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mark: Option<DeskMark>,
    /// The files it carries. An agent's file its turn didn't answer with
    /// is a message of its own: no text, its files, and an id that is no
    /// transcript message's (`file:<artifact_id>`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<crate::attachments::Attachment>,
}

/// Where a page of a conversation sits. A cursor names a message of the
/// transcript: a file that is a message of its own (`file:…`) is none.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cursor<'a> {
    /// The newest messages.
    Newest,
    /// The messages just before this one; `more` says older ones remain.
    Before(&'a str),
    /// The messages just after this one; `more` says newer ones remain.
    After(&'a str),
}

/// A conversation of the caller's that changed since they last looked.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Active {
    pub agent_id: String,
    pub channel: String,
    pub updated_at: DateTime<Utc>,
}

/// A room's conversation to close: the room is gone, and the ids of its
/// messages Desk had not placed in it before it went.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Close {
    pub channel: String,
    #[serde(default)]
    pub undelivered: Vec<String>,
}

/// What closing did: how many undelivered messages went to the DM.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Closed {
    pub copied: usize,
}

/// A page of the conversation, oldest first; `more` when messages remain
/// beyond it in the direction it was read.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
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

/// The hub's side: whose conversations a request asks for, and what they
/// hold.
#[async_trait]
pub trait Conversations: Send + Sync {
    /// At most `limit` messages of the caller's conversation with `agent` on
    /// the Desk `channel`, at `cursor`.
    async fn page(
        &self,
        headers: &HeaderMap,
        token: Option<&str>,
        agent: &str,
        channel: &str,
        cursor: Cursor<'_>,
        limit: usize,
    ) -> Result<Page, ConversationError>;

    /// The caller's Desk conversations, with any agent, that changed after
    /// `since` (all of them without it), least recently changed first.
    async fn active(
        &self,
        headers: &HeaderMap,
        token: Option<&str>,
        since: Option<DateTime<Utc>>,
    ) -> Result<Vec<Active>, ConversationError>;

    /// Close the caller's room conversation with `agent`: later deliveries
    /// go to the DM, and so do the undelivered messages. Closing twice
    /// changes nothing more.
    async fn close(
        &self,
        headers: &HeaderMap,
        token: Option<&str>,
        agent: &str,
        close: Close,
    ) -> Result<Closed, ConversationError>;
}

#[derive(Deserialize)]
struct PageQuery {
    token: Option<String>,
    channel: Option<String>,
    before: Option<String>,
    after: Option<String>,
    limit: Option<usize>,
}

#[derive(Deserialize)]
struct ActiveQuery {
    token: Option<String>,
    since: Option<DateTime<Utc>>,
}

#[derive(Deserialize)]
struct TokenQuery {
    token: Option<String>,
}

fn present(value: &Option<String>) -> Option<&str> {
    value.as_deref().filter(|v| !v.trim().is_empty())
}

async fn conversation(
    State(c): State<Arc<dyn Conversations>>,
    headers: HeaderMap,
    Path(agent): Path<String>,
    Query(q): Query<PageQuery>,
) -> Response {
    let limit = q.limit.unwrap_or(PAGE_MAX).clamp(1, PAGE_MAX);
    let cursor = match (present(&q.before), present(&q.after)) {
        (None, None) => Cursor::Newest,
        (Some(before), None) => Cursor::Before(before),
        (None, Some(after)) => Cursor::After(after),
        (Some(_), Some(_)) => {
            return ConversationError::Invalid("a page is before a message or after one".into())
                .into_response()
        }
    };
    let channel = present(&q.channel).unwrap_or(DESK_CHANNEL);
    c.page(&headers, q.token.as_deref(), &agent, channel, cursor, limit)
        .await
        .map(Json)
        .into_response()
}

async fn active(
    State(c): State<Arc<dyn Conversations>>,
    headers: HeaderMap,
    Query(q): Query<ActiveQuery>,
) -> Response {
    c.active(&headers, q.token.as_deref(), q.since)
        .await
        .map(|conversations| Json(json!({ "conversations": conversations })))
        .into_response()
}

async fn close(
    State(c): State<Arc<dyn Conversations>>,
    headers: HeaderMap,
    Path(agent): Path<String>,
    Query(q): Query<TokenQuery>,
    Json(body): Json<Close>,
) -> Response {
    c.close(&headers, q.token.as_deref(), &agent, body)
        .await
        .map(Json)
        .into_response()
}

/// The Desk conversation routes on `conversations`.
pub fn router(conversations: Arc<dyn Conversations>) -> Router {
    Router::new()
        .route("/api/desk/conversations", get(active))
        .route("/api/desk/conversations/{agent}", get(conversation))
        .route("/api/desk/conversations/{agent}/close", post(close))
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

    /// What a route asked the hub for.
    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Ask {
        Page(String, String, String, usize),
        Active(Option<DateTime<Utc>>),
        Close(String, Close),
    }

    /// Answers fixed values, and remembers what it was asked.
    #[derive(Default)]
    struct Asked(Mutex<Vec<Ask>>);

    impl Asked {
        fn check(&self, headers: &HeaderMap, token: Option<&str>) -> Result<(), ConversationError> {
            let bearer = headers
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.strip_prefix("Bearer "));
            match bearer.or(token) {
                Some("device-token") => Ok(()),
                _ => Err(ConversationError::Unauthorized),
            }
        }

        fn last(&self) -> Ask {
            self.0.lock().unwrap().last().cloned().unwrap()
        }
    }

    #[async_trait]
    impl Conversations for Asked {
        async fn page(
            &self,
            headers: &HeaderMap,
            token: Option<&str>,
            agent: &str,
            channel: &str,
            cursor: Cursor<'_>,
            limit: usize,
        ) -> Result<Page, ConversationError> {
            self.check(headers, token)?;
            if agent == "nobody" {
                return Err(ConversationError::NotFound("no agent named nobody".into()));
            }
            self.0.lock().unwrap().push(Ask::Page(
                agent.into(),
                channel.into(),
                format!("{cursor:?}"),
                limit,
            ));
            Ok(Page {
                messages: vec![Said {
                    id: "m1".into(),
                    from: Author::Agent,
                    author: Speaker::agent("scout", "Scout"),
                    text: "Hello".into(),
                    at: "2026-10-28T09:00:00Z".parse().unwrap(),
                    desk: None,
                    mark: None,
                    attachments: Vec::new(),
                }],
                more: true,
            })
        }

        async fn active(
            &self,
            headers: &HeaderMap,
            token: Option<&str>,
            since: Option<DateTime<Utc>>,
        ) -> Result<Vec<Active>, ConversationError> {
            self.check(headers, token)?;
            self.0.lock().unwrap().push(Ask::Active(since));
            Ok(vec![Active {
                agent_id: "scout".into(),
                channel: "desk".into(),
                updated_at: "2026-10-28T09:00:00Z".parse().unwrap(),
            }])
        }

        async fn close(
            &self,
            headers: &HeaderMap,
            token: Option<&str>,
            agent: &str,
            close: Close,
        ) -> Result<Closed, ConversationError> {
            self.check(headers, token)?;
            let copied = close.undelivered.len();
            self.0.lock().unwrap().push(Ask::Close(agent.into(), close));
            Ok(Closed { copied })
        }
    }

    async fn call(
        asked: Arc<Asked>,
        request: axum::http::request::Builder,
        body: Body,
    ) -> (StatusCode, Value) {
        let response = router(asked)
            .oneshot(request.body(body).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), 1 << 20).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    async fn get(asked: Arc<Asked>, uri: &str, auth: Option<&str>) -> (StatusCode, Value) {
        let request = Request::get(uri);
        let request = match auth {
            Some(auth) => request.header("authorization", auth),
            None => request,
        };
        call(asked, request, Body::empty()).await
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
                    "id": "m1", "from": "agent",
                    "author": {"kind": "agent", "id": "scout", "name": "Scout"},
                    "text": "Hello",
                    "at": "2026-10-28T09:00:00Z",
                }],
                "more": true,
            })
        );
        assert_eq!(
            asked.last(),
            Ask::Page("scout".into(), "desk".into(), "Before(\"m9\")".into(), 50)
        );
    }

    #[tokio::test]
    async fn a_page_names_its_channel_and_reads_forward_after_a_message() {
        let asked = Arc::new(Asked::default());
        let (status, _) = get(
            Arc::clone(&asked),
            "/api/desk/conversations/scout?channel=desk:room-1&after=m2",
            Some("Bearer device-token"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            asked.last(),
            Ask::Page(
                "scout".into(),
                "desk:room-1".into(),
                "After(\"m2\")".into(),
                PAGE_MAX
            )
        );
        let (status, body) = get(
            asked,
            "/api/desk/conversations/scout?after=m2&before=m1",
            Some("Bearer device-token"),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    }

    #[tokio::test]
    async fn the_limit_is_held_to_a_page_and_a_blank_cursor_is_none() {
        let asked = Arc::new(Asked::default());
        for (query, limit) in [("", PAGE_MAX), ("?limit=0", 1), ("?limit=9000", PAGE_MAX)] {
            let uri = format!("/api/desk/conversations/scout{query}");
            let (status, _) = get(Arc::clone(&asked), &uri, Some("Bearer device-token")).await;
            assert_eq!(status, StatusCode::OK);
            let Ask::Page(_, _, _, got) = asked.last() else {
                panic!("a page")
            };
            assert_eq!(got, limit, "{query}");
        }
        get(
            Arc::clone(&asked),
            "/api/desk/conversations/scout?before=&token=device-token",
            None,
        )
        .await;
        assert_eq!(
            asked.last(),
            Ask::Page("scout".into(), "desk".into(), "Newest".into(), PAGE_MAX)
        );
    }

    #[tokio::test]
    async fn what_changed_since_lists_conversations() {
        let asked = Arc::new(Asked::default());
        let (status, body) = get(
            Arc::clone(&asked),
            "/api/desk/conversations?since=2026-10-28T08:00:00Z",
            Some("Bearer device-token"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body,
            json!({"conversations": [
                {"agent_id": "scout", "channel": "desk", "updated_at": "2026-10-28T09:00:00Z"}
            ]})
        );
        assert_eq!(
            asked.last(),
            Ask::Active(Some("2026-10-28T08:00:00Z".parse().unwrap()))
        );
    }

    #[tokio::test]
    async fn closing_names_the_room_and_what_it_never_showed() {
        let asked = Arc::new(Asked::default());
        let (status, body) = call(
            Arc::clone(&asked),
            Request::post("/api/desk/conversations/scout/close")
                .header("authorization", "Bearer device-token")
                .header("content-type", "application/json"),
            Body::from(r#"{"channel": "desk:room-1", "undelivered": ["m1", "m2"]}"#),
        )
        .await;
        assert_eq!((status, body), (StatusCode::OK, json!({"copied": 2})));
        assert_eq!(
            asked.last(),
            Ask::Close(
                "scout".into(),
                Close {
                    channel: "desk:room-1".into(),
                    undelivered: vec!["m1".into(), "m2".into()],
                }
            )
        );
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
