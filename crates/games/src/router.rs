use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::header::{AUTHORIZATION, CACHE_CONTROL, CONTENT_TYPE};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::{SinkExt, StreamExt};
use serde::Deserialize;

use crate::driver::{PlayerIdentity, TokenVerifier};
use crate::manager::{DrawingError, GameManager, JoinError, JoinOutcome};
use crate::protocol::{
    ClientGameMessage, CreateRoomRequest, CreateRoomResponse, GamesConfigView, RoomSummary,
    ServerGameMessage,
};

pub struct GamesState {
    pub manager: Arc<GameManager>,
    pub verifier: Arc<dyn TokenVerifier>,
}

pub fn router(state: Arc<GamesState>) -> Router {
    Router::new()
        .route("/api/config", get(games_config))
        .route("/api/rooms", get(list_rooms).post(create_room))
        .route("/api/rooms/{id}/art", get(room_art))
        .route("/api/rooms/{id}/drawing", post(post_drawing))
        .route("/ws", get(ws_handler))
        .with_state(state)
}

fn authenticate(state: &GamesState, headers: &HeaderMap) -> Option<PlayerIdentity> {
    let token = headers
        .get(AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")?;
    state.verifier.verify(token)
}

async fn list_rooms(
    State(state): State<Arc<GamesState>>,
    headers: HeaderMap,
) -> Result<Json<Vec<RoomSummary>>, StatusCode> {
    authenticate(&state, &headers).ok_or(StatusCode::UNAUTHORIZED)?;
    Ok(Json(state.manager.list_rooms()))
}

async fn games_config(
    State(state): State<Arc<GamesState>>,
    headers: HeaderMap,
) -> Result<Json<GamesConfigView>, StatusCode> {
    authenticate(&state, &headers).ok_or(StatusCode::UNAUTHORIZED)?;
    Ok(Json(GamesConfigView {
        max_bots: state.manager.max_bots(),
    }))
}

/// Cap on concurrently-live game rooms (audit G2). Idle/finished rooms reap
/// after ~90s, so this bounds create-spam, not legitimate concurrent play.
const MAX_ACTIVE_ROOMS: usize = 200;

async fn create_room(
    State(state): State<Arc<GamesState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<CreateRoomResponse>, StatusCode> {
    let host = authenticate(&state, &headers).ok_or(StatusCode::UNAUTHORIZED)?;
    // audit G2: cap concurrent rooms so create-spam can't accumulate unbounded
    // rooms + per-room tickers (each lives until ~ROOM_REAP_SECS idle).
    if state.manager.active_room_count() >= MAX_ACTIVE_ROOMS {
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    }
    let request: CreateRoomRequest = if body.is_empty() {
        CreateRoomRequest::default()
    } else {
        serde_json::from_slice(&body).map_err(|_| StatusCode::BAD_REQUEST)?
    };
    let room_id = state.manager.create_room(
        &host,
        request.kind,
        request.rounds,
        request.bots,
        request.difficulty,
        request.pack,
    );
    Ok(Json(CreateRoomResponse { room_id }))
}

#[derive(Deserialize)]
struct ArtQuery {
    token: String,
}

/// Serves the current round's generated image. The token rides in the query so a
/// plain `<img src>` can authenticate.
async fn room_art(
    State(state): State<Arc<GamesState>>,
    Path(room_id): Path<String>,
    Query(query): Query<ArtQuery>,
) -> Response {
    if state.verifier.verify(&query.token).is_none() {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    match state.manager.room_art(&room_id) {
        Some((bytes, mime)) => {
            let mut headers = HeaderMap::new();
            if let Ok(value) = HeaderValue::from_str(&mime) {
                headers.insert(CONTENT_TYPE, value);
            }
            headers.insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
            (headers, bytes).into_response()
        }
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

/// Cap on a single uploaded drawing snapshot (a 500–800px PNG is well under this).
const MAX_DRAWING_BYTES: usize = 400 * 1024;

#[derive(Deserialize)]
struct DrawingQuery {
    token: String,
    /// The drawer's round and per-round monotonic snapshot number, so the engine
    /// drops stale / out-of-order uploads (a backward-flickering canvas otherwise).
    round: Option<u32>,
    seq: Option<u32>,
}

/// Receives the drawer's PNG snapshot (Human Pictionary). Token in the query (like
/// `/art`) so the canvas client can POST without custom headers; the game enforces
/// that only the current drawer may upload.
async fn post_drawing(
    State(state): State<Arc<GamesState>>,
    Path(room_id): Path<String>,
    Query(query): Query<DrawingQuery>,
    body: Bytes,
) -> StatusCode {
    let Some(identity) = state.verifier.verify(&query.token) else {
        return StatusCode::UNAUTHORIZED;
    };
    if body.len() > MAX_DRAWING_BYTES {
        return StatusCode::PAYLOAD_TOO_LARGE;
    }
    match state.manager.apply_drawing(
        &room_id,
        &identity.id,
        body.to_vec(),
        query.round,
        query.seq,
    ) {
        Ok(()) => StatusCode::OK,
        Err(DrawingError::NotFound) => StatusCode::NOT_FOUND,
        Err(DrawingError::NotDrawer | DrawingError::WrongPhase) => StatusCode::FORBIDDEN,
    }
}

#[derive(Deserialize)]
struct WsQuery {
    token: String,
    room: String,
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    Query(query): Query<WsQuery>,
    State(state): State<Arc<GamesState>>,
) -> Response {
    match state.verifier.verify(&query.token) {
        Some(identity) => ws.on_upgrade(move |socket| play(socket, state, query.room, identity)),
        None => StatusCode::UNAUTHORIZED.into_response(),
    }
}

async fn play(
    mut socket: WebSocket,
    state: Arc<GamesState>,
    room_id: String,
    identity: PlayerIdentity,
) {
    let JoinOutcome {
        player_id,
        mut events,
    } = match state.manager.join(&room_id, &identity) {
        Ok(outcome) => outcome,
        Err(reason) => {
            let message = match reason {
                JoinError::NotFound => "Game not found".to_string(),
                JoinError::Rejected(message) => message,
            };
            let _ = socket
                .send(encode(&ServerGameMessage::Error { message }))
                .await;
            return;
        }
    };

    if socket
        .send(encode(&ServerGameMessage::Welcome {
            player_id: player_id.clone(),
        }))
        .await
        .is_err()
    {
        return;
    }

    let (mut sink, mut stream) = socket.split();
    loop {
        tokio::select! {
            event = events.recv() => match event {
                Ok(msg) => {
                    if sink.send(encode(&msg)).await.is_err() {
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            },
            inbound = stream.next() => match inbound {
                Some(Ok(Message::Text(text))) => {
                    let reply = match serde_json::from_str::<ClientGameMessage>(&text) {
                        Ok(msg) => state.manager.handle_message(&room_id, &player_id, msg).await,
                        Err(_) => Some(ServerGameMessage::Error {
                            message: "Unrecognised message".to_string(),
                        }),
                    };
                    if let Some(reply) = reply {
                        if sink.send(encode(&reply)).await.is_err() {
                            break;
                        }
                    }
                }
                Some(Ok(Message::Close(_))) | None => break,
                Some(Ok(_)) => {}
                Some(Err(_)) => break,
            },
        }
    }

    state.manager.on_disconnect(&room_id, &player_id);
}

fn encode(msg: &ServerGameMessage) -> Message {
    Message::Text(
        serde_json::to_string(msg)
            .unwrap_or_else(|_| "{}".into())
            .into(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::driver::testing::{
        AllowAllVerifier, EchoDriver, InMemoryRivals, StubArtist, StubGuesser,
    };
    use crate::game::RoomConfig;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    fn state() -> Arc<GamesState> {
        Arc::new(GamesState {
            manager: Arc::new(GameManager::new(
                Arc::new(EchoDriver),
                Arc::new(StubArtist),
                Arc::new(StubGuesser),
                Arc::new(InMemoryRivals::default()),
                vec!["penny_game".into()],
                RoomConfig::default(),
            )),
            verifier: Arc::new(AllowAllVerifier),
        })
    }

    #[tokio::test]
    async fn rooms_require_auth() {
        let app = router(state());
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/rooms")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn create_then_list_room() {
        let app = router(state());
        let created = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/rooms")
                    .header(AUTHORIZATION, "Bearer alice")
                    .body(Body::from(r#"{"kind":"reverse_pictionary"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(created.status(), StatusCode::OK);

        let listed = app
            .oneshot(
                Request::builder()
                    .uri("/api/rooms")
                    .header(AUTHORIZATION, "Bearer alice")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = axum::body::to_bytes(listed.into_body(), usize::MAX)
            .await
            .unwrap();
        let rooms: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(rooms.as_array().map(Vec::len), Some(1));
    }

    #[tokio::test]
    async fn art_requires_auth_and_404s_when_absent() {
        let app = router(state());
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/rooms/NOPE/art?token=alice")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }
}
