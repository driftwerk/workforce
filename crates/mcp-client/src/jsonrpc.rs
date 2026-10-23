//! JSON-RPC 2.0 envelopes. Workforce already has server-side
//! handlers in `crates/hub/src/mcp.rs`; those are for the
//! workforce-as-MCP-server path. This module is the *client-side*
//! mirror: we issue requests, the server replies, we send
//! notifications fire-and-forget.

use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// JSON-RPC method names we use. Keeping them as constants makes
/// typos compile errors and simplifies grep.
pub mod method {
    pub const INITIALIZE: &str = "initialize";
    pub const TOOLS_LIST: &str = "tools/list";
    pub const TOOLS_CALL: &str = "tools/call";
    pub const NOTIFICATIONS_INITIALIZED: &str = "notifications/initialized";
    pub const NOTIFICATIONS_CANCELLED: &str = "notifications/cancelled";
}

/// Process-local monotonic JSON-RPC id generator. Wrapping in
/// `AtomicU64` not `Mutex` because the only operation is
/// `fetch_add(1)`.
#[derive(Debug, Default)]
pub(crate) struct RequestIdGen(AtomicU64);

impl RequestIdGen {
    pub fn next(&self) -> u64 {
        self.0.fetch_add(1, Ordering::Relaxed)
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct Request<'a, P: Serialize> {
    pub jsonrpc: &'static str,
    pub id: u64,
    pub method: &'a str,
    pub params: P,
}

impl<'a, P: Serialize> Request<'a, P> {
    pub fn new(id: u64, method: &'a str, params: P) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            method,
            params,
        }
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct Notification<'a, P: Serialize> {
    pub jsonrpc: &'static str,
    pub method: &'a str,
    pub params: P,
}

impl<'a, P: Serialize> Notification<'a, P> {
    pub fn new(method: &'a str, params: P) -> Self {
        Self {
            jsonrpc: "2.0",
            method,
            params,
        }
    }
}

/// Response envelope. Exactly one of `result` / `error` is `Some`
/// per the spec; we don't enforce that statically.
#[derive(Debug, Deserialize)]
pub(crate) struct Response {
    #[serde(default)]
    pub id: Option<Value>,
    #[serde(default)]
    pub result: Option<Value>,
    #[serde(default)]
    pub error: Option<ErrorEnvelope>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ErrorEnvelope {
    pub code: i32,
    pub message: String,
    #[serde(default)]
    pub data: Option<Value>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_gen_is_monotonic() {
        let gen = RequestIdGen::default();
        assert_eq!(gen.next(), 0);
        assert_eq!(gen.next(), 1);
        assert_eq!(gen.next(), 2);
    }

    #[test]
    fn request_serialises_with_jsonrpc_version() {
        let req = Request::new(7, method::TOOLS_LIST, serde_json::json!({"cursor": "x"}));
        let s = serde_json::to_string(&req).unwrap();
        assert!(s.contains(r#""jsonrpc":"2.0""#));
        assert!(s.contains(r#""id":7"#));
        assert!(s.contains(r#""method":"tools/list""#));
        assert!(s.contains(r#""cursor":"x""#));
    }

    #[test]
    fn notification_has_no_id() {
        let n = Notification::new(method::NOTIFICATIONS_INITIALIZED, serde_json::json!({}));
        let s = serde_json::to_string(&n).unwrap();
        assert!(!s.contains(r#""id""#));
        assert!(s.contains(r#""method":"notifications/initialized""#));
    }

    #[test]
    fn response_parses_result_branch() {
        let raw = r#"{"jsonrpc":"2.0","id":1,"result":{"ok":true}}"#;
        let r: Response = serde_json::from_str(raw).unwrap();
        assert_eq!(r.id, Some(serde_json::json!(1)));
        assert!(r.result.is_some());
        assert!(r.error.is_none());
    }

    #[test]
    fn response_parses_error_branch() {
        let raw = r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32601,"message":"unknown"}}"#;
        let r: Response = serde_json::from_str(raw).unwrap();
        assert!(r.result.is_none());
        let e = r.error.unwrap();
        assert_eq!(e.code, -32601);
        assert_eq!(e.message, "unknown");
    }
}
