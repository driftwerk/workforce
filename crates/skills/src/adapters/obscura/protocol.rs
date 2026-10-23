//! JSON-RPC 2.0 envelopes + NDJSON framing for the Obscura MCP stdio protocol.
//!
//! Obscura speaks MCP over stdin/stdout: one JSON message per line. We only
//! need request/response correlation by id, plus the minimal MCP handshake
//! (`initialize` → response → `notifications/initialized`). Pure types here;
//! the I/O loop lives in `client.rs`.

use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub(super) const PROTOCOL_VERSION: &str = "2024-11-05";
pub(super) const METHOD_INITIALIZE: &str = "initialize";
pub(super) const METHOD_NOTIFICATIONS_INITIALIZED: &str = "notifications/initialized";
pub(super) const METHOD_TOOLS_CALL: &str = "tools/call";

#[derive(Debug, Default)]
pub(super) struct IdGen(AtomicU64);

impl IdGen {
    pub(super) fn next(&self) -> u64 {
        self.0.fetch_add(1, Ordering::Relaxed)
    }
}

#[derive(Debug, Serialize)]
pub(super) struct Request<'a, P: Serialize> {
    pub jsonrpc: &'static str,
    pub id: u64,
    pub method: &'a str,
    pub params: P,
}

impl<'a, P: Serialize> Request<'a, P> {
    pub(super) fn new(id: u64, method: &'a str, params: P) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            method,
            params,
        }
    }
}

#[derive(Debug, Serialize)]
pub(super) struct Notification<'a, P: Serialize> {
    pub jsonrpc: &'static str,
    pub method: &'a str,
    pub params: P,
}

impl<'a, P: Serialize> Notification<'a, P> {
    pub(super) fn new(method: &'a str, params: P) -> Self {
        Self {
            jsonrpc: "2.0",
            method,
            params,
        }
    }
}

#[derive(Debug, Deserialize)]
pub(super) struct Response {
    #[serde(default)]
    pub id: Option<Value>,
    #[serde(default)]
    pub result: Option<Value>,
    #[serde(default)]
    pub error: Option<RpcError>,
}

#[derive(Debug, Deserialize)]
pub(super) struct RpcError {
    pub code: i32,
    pub message: String,
    #[serde(default)]
    #[allow(dead_code)]
    pub data: Option<Value>,
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "jsonrpc error {}: {}", self.code, self.message)
    }
}

pub(super) fn matches_id(response_id: &Value, expected: u64) -> bool {
    match response_id {
        Value::Number(n) => n.as_u64() == Some(expected),
        Value::String(s) => s.parse::<u64>().ok() == Some(expected),
        _ => false,
    }
}

/// MCP `initialize` params. Server replies with its capabilities; we don't
/// introspect them because we already know which tools we want to call.
#[derive(Debug, Serialize)]
pub(super) struct InitializeParams<'a> {
    #[serde(rename = "protocolVersion")]
    pub protocol_version: &'a str,
    pub capabilities: Value,
    #[serde(rename = "clientInfo")]
    pub client_info: ClientInfo<'a>,
}

#[derive(Debug, Serialize)]
pub(super) struct ClientInfo<'a> {
    pub name: &'a str,
    pub version: &'a str,
}

/// MCP `tools/call` params.
#[derive(Debug, Serialize)]
pub(super) struct ToolsCallParams<'a> {
    pub name: &'a str,
    pub arguments: Value,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn id_gen_is_monotonic() {
        let g = IdGen::default();
        assert_eq!(g.next(), 0);
        assert_eq!(g.next(), 1);
        assert_eq!(g.next(), 2);
    }

    #[test]
    fn request_serialises_with_jsonrpc_version_and_id() {
        let req = Request::new(7, "tools/call", json!({"k": "v"}));
        let s = serde_json::to_string(&req).unwrap();
        assert!(s.contains(r#""jsonrpc":"2.0""#));
        assert!(s.contains(r#""id":7"#));
        assert!(s.contains(r#""method":"tools/call""#));
    }

    #[test]
    fn notification_has_no_id() {
        let n = Notification::new("notifications/initialized", json!({}));
        let s = serde_json::to_string(&n).unwrap();
        assert!(!s.contains(r#""id":"#));
    }

    #[test]
    fn matches_id_handles_number_and_string() {
        assert!(matches_id(&json!(7u64), 7));
        assert!(matches_id(&json!("7"), 7));
        assert!(!matches_id(&json!(8u64), 7));
        assert!(!matches_id(&json!("seven"), 7));
        assert!(!matches_id(&json!(null), 7));
    }

    #[test]
    fn response_deserialises_result_branch() {
        let raw = r#"{"jsonrpc":"2.0","id":1,"result":{"ok":true}}"#;
        let r: Response = serde_json::from_str(raw).unwrap();
        assert_eq!(r.result.unwrap()["ok"], true);
        assert!(r.error.is_none());
    }

    #[test]
    fn response_deserialises_error_branch() {
        let raw = r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32601,"message":"unknown method"}}"#;
        let r: Response = serde_json::from_str(raw).unwrap();
        assert!(r.result.is_none());
        let e = r.error.unwrap();
        assert_eq!(e.code, -32601);
        assert!(e.message.contains("unknown method"));
    }
}
