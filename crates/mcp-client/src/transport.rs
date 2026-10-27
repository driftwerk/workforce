//! HTTP transport for Streamable HTTP MCP. Wraps `reqwest` with
//! the protocol's specific framing rules.
//!
//! Wire rules (per <https://modelcontextprotocol.io>):
//! - Every JSON-RPC message is POSTed.
//! - `Content-Type: application/json`.
//! - `Accept: application/json, text/event-stream` so the server
//!   may answer either way.
//! - After `initialize`, every request carries `MCP-Protocol-Version`.
//! - The server may reply with a single JSON object OR with an SSE
//!   stream of frames; we dispatch on the response `Content-Type`.
//! - `Mcp-Session-Id` is set by the server on the initialize
//!   response and must be echoed on every subsequent request.

use std::sync::Arc;

use futures::StreamExt;
use reqwest::header::{HeaderMap, HeaderValue, ACCEPT, AUTHORIZATION, CONTENT_TYPE};
use reqwest::StatusCode;
use serde::Serialize;
use tracing::warn;
use url::Url;
use workforce_common::redact::redact_url;

use crate::auth::AuthProvider;
use crate::error::McpError;
use crate::jsonrpc::Response;
use crate::sse::SseParser;

const MCP_SESSION_HEADER: &str = "Mcp-Session-Id";
const MCP_PROTOCOL_HEADER: &str = "MCP-Protocol-Version";
const ACCEPT_VALUE: &str = "application/json, text/event-stream";
const POST: &str = "POST";

pub(crate) struct Transport {
    endpoint: Url,
    redacted_endpoint: Url,
    http: Arc<reqwest::Client>,
    auth: Arc<dyn AuthProvider>,
    max_content_bytes: usize,
}

impl Transport {
    pub fn new(
        endpoint: Url,
        http: Arc<reqwest::Client>,
        auth: Arc<dyn AuthProvider>,
        max_content_bytes: usize,
    ) -> Self {
        Self {
            redacted_endpoint: redact_url(&endpoint),
            endpoint,
            http,
            auth,
            max_content_bytes,
        }
    }

    /// POST a JSON-RPC request and return the parsed response, plus
    /// any session id the server set (initialize) or echoed.
    pub async fn post_request<P: Serialize>(
        &self,
        request: &P,
        session_id: Option<&str>,
        protocol_version: Option<&str>,
        request_id: u64,
    ) -> Result<PostOutcome, McpError> {
        let headers = self.build_headers(session_id, protocol_version).await?;
        let resp = self
            .http
            .post(self.endpoint.clone())
            .headers(headers)
            .json(request)
            .send()
            .await
            .map_err(send_failed)?;

        let status = resp.status();
        let new_session = resp
            .headers()
            .get(MCP_SESSION_HEADER)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);

        if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
            return Err(McpError::AuthExpired(format!(
                "server returned HTTP {} on {POST} {}",
                status.as_u16(),
                self.redacted_endpoint
            )));
        }
        if status == StatusCode::NOT_FOUND {
            return Ok(PostOutcome::SessionExpired);
        }
        if status == StatusCode::TOO_MANY_REQUESTS {
            let retry_after_secs = resp
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.trim().parse().ok());
            let body = resp
                .text()
                .await
                .unwrap_or_else(|_| "<no body>".to_string());
            return Err(McpError::RateLimited {
                message: format!("HTTP 429 from {}: {}", self.redacted_endpoint, scrub(body)),
                retry_after_secs,
            });
        }
        if !status.is_success() {
            let body = resp
                .text()
                .await
                .unwrap_or_else(|_| "<no body>".to_string());
            return Err(McpError::Transport(format!(
                "HTTP {} from {}: {}",
                status.as_u16(),
                self.redacted_endpoint,
                scrub(body)
            )));
        }

        let content_type = resp
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_ascii_lowercase();
        let response = if content_type.starts_with("text/event-stream") {
            self.read_sse_until(resp, request_id).await?
        } else {
            self.read_json(resp).await?
        };
        Ok(PostOutcome::Response {
            response,
            session_id: new_session,
        })
    }

    /// POST a JSON-RPC notification fire-and-forget. Notifications
    /// have no response; we discard the body but log non-2xx.
    pub async fn post_notification<P: Serialize>(
        &self,
        notification: &P,
        session_id: Option<&str>,
        protocol_version: Option<&str>,
    ) -> Result<(), McpError> {
        let headers = self.build_headers(session_id, protocol_version).await?;
        let resp = self
            .http
            .post(self.endpoint.clone())
            .headers(headers)
            .json(notification)
            .send()
            .await
            .map_err(send_failed)?;
        let status = resp.status();
        if !status.is_success() && status != StatusCode::ACCEPTED {
            warn!(
                status = %status,
                "MCP notification returned non-success status; ignoring"
            );
        }
        Ok(())
    }

    async fn build_headers(
        &self,
        session_id: Option<&str>,
        protocol_version: Option<&str>,
    ) -> Result<HeaderMap, McpError> {
        let mut headers = HeaderMap::new();
        let auth = self.auth.authorization_header().await?;
        // An empty value is `NoAuth`: the request carries no header rather
        // than an empty one, which servers read as a malformed credential.
        if !auth.is_empty() {
            let auth = HeaderValue::from_str(&auth).map_err(|_| {
                McpError::Transport("authorization header contains invalid bytes".into())
            })?;
            headers.insert(AUTHORIZATION, auth);
        }
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        headers.insert(ACCEPT, HeaderValue::from_static(ACCEPT_VALUE));
        if let Some(sid) = session_id {
            if let Ok(v) = HeaderValue::from_str(sid) {
                headers.insert(MCP_SESSION_HEADER, v);
            }
        }
        if let Some(pv) = protocol_version {
            if let Ok(v) = HeaderValue::from_str(pv) {
                headers.insert(MCP_PROTOCOL_HEADER, v);
            }
        }
        Ok(headers)
    }

    async fn read_json(&self, resp: reqwest::Response) -> Result<Response, McpError> {
        let bytes = read_capped(resp, self.max_content_bytes).await?;
        serde_json::from_slice(&bytes)
            .map_err(|e| McpError::Transport(format!("malformed JSON response: {e}")))
    }

    /// Read SSE frames until we see one whose JSON-RPC `id` matches
    /// the request id we're awaiting, then stop. Other frames
    /// (server-initiated notifications, progress) are discarded for
    /// v1; resources/prompts will need to handle them in v2.
    async fn read_sse_until(
        &self,
        resp: reqwest::Response,
        request_id: u64,
    ) -> Result<Response, McpError> {
        let mut parser = SseParser::new();
        let mut total = 0usize;
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| McpError::Transport(describe(e)))?;
            total = total.saturating_add(chunk.len());
            if total > self.max_content_bytes {
                return Err(McpError::BodyTooLarge {
                    limit: self.max_content_bytes,
                });
            }
            for payload in parser.feed(&chunk) {
                let response: Response = match serde_json::from_str(&payload) {
                    Ok(r) => r,
                    Err(e) => {
                        warn!(error = %e, "skipping malformed SSE frame");
                        continue;
                    }
                };
                if response
                    .id
                    .as_ref()
                    .is_some_and(|v| matches_id(v, request_id))
                {
                    return Ok(response);
                }
            }
        }
        Err(McpError::Transport(
            "SSE stream ended before a matching response arrived".into(),
        ))
    }
}

#[derive(Debug)]
pub(crate) enum PostOutcome {
    Response {
        response: Response,
        session_id: Option<String>,
    },
    /// Server returned 404 — the session id is stale; caller should
    /// re-`initialize` and retry once.
    SessionExpired,
}

fn matches_id(value: &serde_json::Value, expected: u64) -> bool {
    value.as_u64() == Some(expected)
}

async fn read_capped(resp: reqwest::Response, limit: usize) -> Result<Vec<u8>, McpError> {
    let mut buf = Vec::new();
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| McpError::Transport(describe(e)))?;
        if buf.len() + chunk.len() > limit {
            return Err(McpError::BodyTooLarge { limit });
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(buf)
}

/// Classify a failed `send`: a connect or timeout failure means no
/// response ever arrived, so the server is unreachable; anything else
/// failed after the connection was made. reqwest reports a failed TLS
/// handshake as a connect error.
fn send_failed(e: reqwest::Error) -> McpError {
    let error = if e.is_connect() || e.is_timeout() {
        McpError::Unreachable
    } else {
        McpError::Transport
    };
    error(describe(e))
}

/// The scrubbed error with its whole source chain: reqwest's own text
/// omits the cause (refused, DNS, TLS, early EOF).
fn describe(e: reqwest::Error) -> String {
    let e = match e.url().map(redact_url) {
        Some(url) => e.with_url(url),
        None => e,
    };
    let chain = std::iter::successors(Some(&e as &dyn std::error::Error), |e| e.source())
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(": ");
    scrub(chain)
}

fn scrub(input: String) -> String {
    workforce_common::redact::redact_secrets(&input)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_id_accepts_unsigned_form() {
        assert!(matches_id(&serde_json::json!(7u64), 7));
        assert!(matches_id(&serde_json::json!(7i64), 7));
    }

    #[test]
    fn matches_id_rejects_string_form() {
        // Some buggy servers send IDs as strings; we don't accept
        // them — match strictly to the integer we sent.
        assert!(!matches_id(&serde_json::json!("7"), 7));
    }

    // The edge's live-token fix depends on the transport reading the auth
    // provider on EVERY request; a rotating provider proves the header is
    // re-fetched, not cached at construction.
    #[tokio::test]
    async fn build_headers_reads_auth_on_every_call() {
        struct Rotating(std::sync::Mutex<Vec<&'static str>>);
        #[async_trait::async_trait]
        impl AuthProvider for Rotating {
            async fn authorization_header(&self) -> Result<String, McpError> {
                Ok(self.0.lock().unwrap().remove(0).to_string())
            }
        }
        let transport = Transport::new(
            Url::parse("http://mcp.test/").unwrap(),
            Arc::new(reqwest::Client::new()),
            Arc::new(Rotating(std::sync::Mutex::new(vec![
                "Bearer one",
                "Bearer two",
            ]))),
            1024,
        );
        let first = transport.build_headers(None, None).await.unwrap();
        let second = transport.build_headers(None, None).await.unwrap();
        assert_eq!(first[AUTHORIZATION].to_str().unwrap(), "Bearer one");
        assert_eq!(second[AUTHORIZATION].to_str().unwrap(), "Bearer two");
    }

    #[tokio::test]
    async fn no_auth_sends_no_authorization_header() {
        let transport = Transport::new(
            Url::parse("http://mcp.test/").unwrap(),
            Arc::new(reqwest::Client::new()),
            Arc::new(crate::auth::NoAuth),
            1024,
        );
        let headers = transport.build_headers(None, None).await.unwrap();
        assert!(!headers.contains_key(AUTHORIZATION));
        assert_eq!(headers[CONTENT_TYPE], "application/json");
    }

    struct NoAuth;
    #[async_trait::async_trait]
    impl AuthProvider for NoAuth {
        async fn authorization_header(&self) -> Result<String, McpError> {
            Ok("Bearer test".to_string())
        }
    }

    #[tokio::test]
    async fn http_429_maps_to_the_typed_rate_limit_error_with_the_header_hint() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(429)
                    .insert_header("retry-after", "30")
                    .set_body_string("slow down"),
            )
            .mount(&server)
            .await;
        let transport = Transport::new(
            Url::parse(&server.uri()).unwrap(),
            Arc::new(reqwest::Client::new()),
            Arc::new(NoAuth),
            1024,
        );
        let err = transport
            .post_request(&serde_json::json!({"jsonrpc":"2.0"}), None, None, 1)
            .await
            .unwrap_err();
        match err {
            McpError::RateLimited {
                message,
                retry_after_secs,
            } => {
                assert_eq!(retry_after_secs, Some(30));
                assert!(message.contains("HTTP 429"), "{message}");
            }
            other => panic!("expected RateLimited, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn http_429_names_the_host_but_no_endpoint_secret() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(429).set_body_string("slow down"))
            .mount(&server)
            .await;
        let addr = server.address();
        let transport = Transport::new(
            Url::parse(&format!(
                "http://opsuser7:hunter2@{addr}/mcp/k-acme42?sig=s3cr3t#fr4gment"
            ))
            .unwrap(),
            Arc::new(reqwest::Client::new()),
            Arc::new(NoAuth),
            1024,
        );
        let err = transport
            .post_request(&serde_json::json!({"jsonrpc":"2.0"}), None, None, 1)
            .await
            .unwrap_err();
        let McpError::RateLimited { message, .. } = err else {
            panic!("expected RateLimited, got {err:?}");
        };
        assert!(message.contains(&addr.to_string()), "{message}");
        for secret in ["opsuser7", "hunter2", "k-acme42", "s3cr3t", "fr4gment"] {
            assert!(!message.contains(secret), "{message}");
        }
    }

    #[tokio::test]
    async fn http_429_without_the_header_still_maps_typed() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(429))
            .mount(&server)
            .await;
        let transport = Transport::new(
            Url::parse(&server.uri()).unwrap(),
            Arc::new(reqwest::Client::new()),
            Arc::new(NoAuth),
            1024,
        );
        let err = transport
            .post_request(&serde_json::json!({"jsonrpc":"2.0"}), None, None, 1)
            .await
            .unwrap_err();
        assert!(
            matches!(
                err,
                McpError::RateLimited {
                    retry_after_secs: None,
                    ..
                }
            ),
            "{err:?}"
        );
    }
}
