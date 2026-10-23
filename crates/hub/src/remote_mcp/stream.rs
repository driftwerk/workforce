//! SSE on POST for the remote MCP calls that wait on an agent turn.
//!
//! `agent.chat` and `approval.decide` hold the request while the turn runs.
//! Answered as one JSON body that is a silent connection, and a proxy's idle
//! cut (Cloudflare's 100 s, an ALB's 60 s) drops it before the reply arrives,
//! taking the poll handle with it. A client that accepts `text/event-stream`
//! gets those calls as a stream instead: a keepalive comment every
//! [`KEEPALIVE`], then the JSON-RPC response. Only a streamed call may wait
//! past `chat_default_wait_secs`.

use std::convert::Infallible;
use std::future::Future;
use std::time::Duration;

use axum::http::{header, HeaderMap};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use futures::StreamExt;
use serde_json::Value;
use tokio::sync::oneshot;

use crate::mcp::normalise_tool_name;

/// Inside every idle budget between a caller and the hub.
pub(super) const KEEPALIVE: Duration = Duration::from_secs(25);

const STREAMED_TOOLS: [&str; 2] = ["agent.chat", "approval.decide"];

tokio::task_local! {
    static STREAMED: ();
}

/// Whether a request is answered as a stream: an SSE-accepting client calling
/// one of the waiting tools.
pub(super) fn streamed(headers: &HeaderMap, body: &str) -> bool {
    headers
        .get_all(header::ACCEPT)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .any(|v| v.to_ascii_lowercase().contains("text/event-stream"))
        && serde_json::from_str::<Value>(body).is_ok_and(|request| waits_on_a_turn(&request))
}

fn waits_on_a_turn(request: &Value) -> bool {
    request["method"] == "tools/call"
        && !request["id"].is_null()
        && request["params"]["name"]
            .as_str()
            .map(normalise_tool_name)
            .is_some_and(|name| {
                STREAMED_TOOLS
                    .iter()
                    .any(|tool| normalise_tool_name(tool) == name)
            })
}

/// Whether the running call answers on a stream.
pub(super) fn active() -> bool {
    STREAMED.try_with(|_| ()).is_ok()
}

/// Answer `call` as an SSE stream: keepalives while it runs, then its JSON-RPC
/// response. The call is spawned, so a caller who hangs up does not cancel it:
/// a turn it started still registers, and the reply still lands in the session.
pub(super) fn respond<F>(call: F, keepalive: Duration) -> Response
where
    F: Future<Output = Option<String>> + Send + 'static,
{
    let (tx, rx) = oneshot::channel();
    tokio::spawn(async move {
        if let Some(response) = STREAMED.scope((), call).await {
            let _ = tx.send(response);
        }
    });
    let events = futures::stream::once(rx).filter_map(|response| async move {
        response
            .ok()
            .map(|data| Ok::<_, Infallible>(Event::default().event("message").data(data)))
    });
    Sse::new(events)
        .keep_alive(KeepAlive::new().interval(keepalive).text("keepalive"))
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    const RESPONSE: &str = r#"{"jsonrpc":"2.0","id":1,"result":{}}"#;

    fn headers(accept: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(header::ACCEPT, accept.parse().unwrap());
        headers
    }

    fn call(name: &str) -> String {
        serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": { "name": name, "arguments": {} }
        })
        .to_string()
    }

    async fn body_of(response: Response) -> String {
        let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .unwrap();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    #[test]
    fn only_an_sse_caller_of_a_waiting_tool_streams() {
        let sse = headers("application/json, text/event-stream");
        assert!(streamed(&sse, &call("agent_chat")));
        assert!(streamed(&sse, &call("approval.decide")));
        assert!(!streamed(&sse, &call("agent.poll")));
        assert!(!streamed(&sse, &call("jira.search")));
        assert!(!streamed(&headers("application/json"), &call("agent_chat")));
        let notification =
            r#"{"jsonrpc":"2.0","method":"tools/call","params":{"name":"agent_chat"}}"#;
        assert!(!streamed(&sse, notification));
        assert!(!streamed(
            &sse,
            r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#
        ));
        assert!(!streamed(&sse, "not json"));
    }

    #[tokio::test]
    async fn a_slow_call_keeps_the_stream_alive_and_ends_with_its_response() {
        let response = respond(
            async {
                tokio::time::sleep(Duration::from_millis(120)).await;
                Some(RESPONSE.to_string())
            },
            Duration::from_millis(20),
        );
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            "text/event-stream"
        );
        let body = body_of(response).await;
        let keepalive = body
            .find("keepalive")
            .expect("a keepalive before the reply");
        let reply = body
            .find(&format!("data: {RESPONSE}"))
            .expect("the response as a data event");
        assert!(keepalive < reply, "{body}");
    }

    #[tokio::test]
    async fn the_call_knows_it_answers_on_a_stream() {
        assert!(!active());
        let response = respond(async { Some(active().to_string()) }, KEEPALIVE);
        assert!(body_of(response).await.contains("data: true"));
    }
}
