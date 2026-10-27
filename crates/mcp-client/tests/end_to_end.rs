//! End-to-end integration tests against a wiremock-faked MCP server.
//! Covers the full happy path plus every protocol edge case the
//! plan called out (session re-init on 404, SSE-on-POST,
//! pagination, cancellation, schema-trust limit enforcement).

use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;
use url::Url;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};
use workforce_mcp_client::{McpClient, McpError, McpLimits, StaticBearer};

fn client(server: &MockServer, limits: McpLimits) -> McpClient {
    client_at(&server.uri(), reqwest::Client::new(), limits)
}

fn client_at(endpoint: &str, http: reqwest::Client, limits: McpLimits) -> McpClient {
    McpClient::new(
        Url::parse(endpoint).unwrap(),
        Arc::new(http),
        Arc::new(StaticBearer::new("test-token")),
        limits,
    )
}

fn json_response(id: u64, result: Value) -> ResponseTemplate {
    ResponseTemplate::new(200)
        .insert_header("content-type", "application/json")
        .insert_header("mcp-session-id", "sess-1")
        .set_body_json(json!({"jsonrpc": "2.0", "id": id, "result": result}))
}

fn rpc_id(req: &Request) -> u64 {
    let v: Value = serde_json::from_slice(&req.body).unwrap();
    v["id"].as_u64().unwrap_or(0)
}

fn rpc_method(req: &Request) -> String {
    let v: Value = serde_json::from_slice(&req.body).unwrap();
    v["method"].as_str().unwrap_or("").to_string()
}

async fn mount_initialize(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/"))
        .and(BodyRpc("initialize"))
        .respond_with(|req: &Request| {
            json_response(
                rpc_id(req),
                json!({
                    "protocolVersion": "2025-06-18",
                    "serverInfo": {"name": "fake-mcp", "version": "0.1.0"},
                    "capabilities": {"tools": {}}
                }),
            )
        })
        .mount(server)
        .await;
}

/// Match POSTs whose JSON body has `method == expected`.
struct BodyRpc(&'static str);

impl wiremock::Match for BodyRpc {
    fn matches(&self, req: &Request) -> bool {
        rpc_method(req) == self.0
    }
}

#[tokio::test]
async fn initialize_then_tools_list_round_trip() {
    let server = MockServer::start().await;
    mount_initialize(&server).await;
    Mock::given(method("POST"))
        .and(BodyRpc("notifications/initialized"))
        .respond_with(ResponseTemplate::new(202))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(BodyRpc("tools/list"))
        .respond_with(|req: &Request| {
            json_response(
                rpc_id(req),
                json!({
                    "tools": [
                        {"name": "ping", "description": "round-trip", "inputSchema": {"type": "object"}},
                    ]
                }),
            )
        })
        .mount(&server)
        .await;

    let client = client(&server, McpLimits::default());
    let init = client.initialize().await.unwrap();
    assert_eq!(init.protocol_version, "2025-06-18");
    assert_eq!(init.server_info.unwrap().name, "fake-mcp");

    let tools = client.tools_list().await.unwrap();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, "ping");
}

#[tokio::test]
async fn initialize_sends_protocol_version_header_on_subsequent_requests() {
    let server = MockServer::start().await;
    mount_initialize(&server).await;
    Mock::given(method("POST"))
        .and(BodyRpc("notifications/initialized"))
        .respond_with(ResponseTemplate::new(202))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(BodyRpc("tools/list"))
        .and(header("mcp-protocol-version", "2025-06-18"))
        .respond_with(|req: &Request| json_response(rpc_id(req), json!({"tools": []})))
        .expect(1)
        .mount(&server)
        .await;

    let client = client(&server, McpLimits::default());
    client.initialize().await.unwrap();
    client.tools_list().await.unwrap();
}

#[tokio::test]
async fn tools_call_forwards_arguments_and_returns_result() {
    let server = MockServer::start().await;
    mount_initialize(&server).await;
    Mock::given(method("POST"))
        .and(BodyRpc("notifications/initialized"))
        .respond_with(ResponseTemplate::new(202))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(BodyRpc("tools/call"))
        .respond_with(|req: &Request| {
            let body: Value = serde_json::from_slice(&req.body).unwrap();
            assert_eq!(body["params"]["name"], "echo");
            assert_eq!(body["params"]["arguments"]["msg"], "hi");
            json_response(rpc_id(req), json!({"echoed": "hi"}))
        })
        .mount(&server)
        .await;

    let client = client(&server, McpLimits::default());
    client.initialize().await.unwrap();
    let result = client
        .tools_call("echo", json!({"msg": "hi"}), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(result["echoed"], "hi");
}

#[tokio::test]
async fn session_404_triggers_reinitialize_and_retry() {
    let server = MockServer::start().await;
    // First initialize (called by the test via .initialize()).
    Mock::given(method("POST"))
        .and(BodyRpc("initialize"))
        .respond_with(|req: &Request| {
            json_response(
                rpc_id(req),
                json!({
                    "protocolVersion": "2025-06-18",
                    "serverInfo": {"name": "fake", "version": "0.1.0"},
                    "capabilities": {}
                }),
            )
        })
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(BodyRpc("notifications/initialized"))
        .respond_with(ResponseTemplate::new(202))
        .mount(&server)
        .await;

    // First tools/list returns 404 (session expired). Second
    // succeeds. wiremock matches in registration order — we mount
    // the 404 first with `up_to_n_times(1)` so the second
    // matching request falls through to the 200 mock below.
    Mock::given(method("POST"))
        .and(BodyRpc("tools/list"))
        .respond_with(ResponseTemplate::new(404))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(BodyRpc("tools/list"))
        .respond_with(|req: &Request| json_response(rpc_id(req), json!({"tools": []})))
        .mount(&server)
        .await;

    let client = client(&server, McpLimits::default());
    client.initialize().await.unwrap();
    let tools = client.tools_list().await.unwrap();
    assert!(tools.is_empty());
    // Two `initialize` calls total: explicit + the 404-triggered
    // re-init. Spot-check by counting mock invocations.
    let inits = server
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .filter(|r| rpc_method(r) == "initialize")
        .count();
    assert_eq!(inits, 2, "session expiry must re-initialize");
}

#[tokio::test]
async fn sse_on_post_response_is_parsed() {
    let server = MockServer::start().await;
    mount_initialize(&server).await;
    Mock::given(method("POST"))
        .and(BodyRpc("notifications/initialized"))
        .respond_with(ResponseTemplate::new(202))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(BodyRpc("tools/call"))
        .respond_with(|req: &Request| {
            let id = rpc_id(req);
            // Server dribbles a heartbeat frame, then a non-matching
            // notification, then the actual matching reply — only
            // the matching one should be returned by the client.
            let body = format!(
                ": keepalive\n\nevent: message\ndata: {{\"jsonrpc\":\"2.0\",\"id\":999,\"result\":{{\"unrelated\":true}}}}\n\nevent: message\ndata: {{\"jsonrpc\":\"2.0\",\"id\":{id},\"result\":{{\"streamed\":true}}}}\n\n"
            );
            ResponseTemplate::new(200)
                .insert_header("mcp-session-id", "sess-1")
                .set_body_raw(body.into_bytes(), "text/event-stream")
        })
        .mount(&server)
        .await;

    let client = client(&server, McpLimits::default());
    client.initialize().await.unwrap();
    let result = client
        .tools_call("slow", json!({}), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(result["streamed"], true);
}

#[tokio::test]
async fn tools_list_pages_until_cursor_drains() {
    let server = MockServer::start().await;
    mount_initialize(&server).await;
    Mock::given(method("POST"))
        .and(BodyRpc("notifications/initialized"))
        .respond_with(ResponseTemplate::new(202))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(BodyRpc("tools/list"))
        .respond_with(|req: &Request| {
            let body: Value = serde_json::from_slice(&req.body).unwrap();
            let cursor = body["params"]["cursor"].as_str();
            let id = rpc_id(req);
            let result = match cursor {
                None => json!({
                    "tools": [{"name": "a", "description": "x", "inputSchema": {"type":"object"}}],
                    "nextCursor": "c1"
                }),
                Some("c1") => json!({
                    "tools": [{"name": "b", "description": "y", "inputSchema": {"type":"object"}}],
                    "nextCursor": "c2"
                }),
                Some("c2") => json!({
                    "tools": [{"name": "c", "description": "z", "inputSchema": {"type":"object"}}]
                }),
                Some(other) => panic!("unexpected cursor: {other}"),
            };
            json_response(id, result)
        })
        .mount(&server)
        .await;

    let client = client(&server, McpLimits::default());
    client.initialize().await.unwrap();
    let tools = client.tools_list().await.unwrap();
    let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(names, vec!["a", "b", "c"]);
}

#[tokio::test]
async fn cancellation_aborts_and_emits_notification() {
    let server = MockServer::start().await;
    mount_initialize(&server).await;
    Mock::given(method("POST"))
        .and(BodyRpc("notifications/initialized"))
        .respond_with(ResponseTemplate::new(202))
        .mount(&server)
        .await;
    // Slow tools/call — long enough for the cancellation to fire.
    Mock::given(method("POST"))
        .and(BodyRpc("tools/call"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_json(json!({"jsonrpc":"2.0","id":1,"result":{}}))
                .set_delay(Duration::from_secs(5)),
        )
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(BodyRpc("notifications/cancelled"))
        .respond_with(ResponseTemplate::new(202))
        .expect(1)
        .mount(&server)
        .await;

    let client = client(&server, McpLimits::default());
    client.initialize().await.unwrap();
    let cancel = CancellationToken::new();
    let cancel_clone = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        cancel_clone.cancel();
    });
    let err = client
        .tools_call("slow", json!({}), cancel)
        .await
        .unwrap_err();
    assert!(matches!(err, McpError::Cancelled), "got {err:?}");
}

#[tokio::test]
async fn schema_trust_rejects_malicious_tool_name() {
    let server = MockServer::start().await;
    mount_initialize(&server).await;
    Mock::given(method("POST"))
        .and(BodyRpc("notifications/initialized"))
        .respond_with(ResponseTemplate::new(202))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(BodyRpc("tools/list"))
        .respond_with(|req: &Request| {
            json_response(
                rpc_id(req),
                json!({
                    "tools": [{
                        "name": "; rm -rf /",
                        "description": "evil",
                        "inputSchema": {"type": "object"}
                    }]
                }),
            )
        })
        .mount(&server)
        .await;

    let client = client(&server, McpLimits::default());
    client.initialize().await.unwrap();
    let err = client.tools_list().await.unwrap_err();
    assert!(
        matches!(err, McpError::SchemaViolation(ref m) if m.contains("violates")),
        "got {err:?}"
    );
}

#[tokio::test]
async fn schema_trust_rejects_oversize_description() {
    let server = MockServer::start().await;
    mount_initialize(&server).await;
    Mock::given(method("POST"))
        .and(BodyRpc("notifications/initialized"))
        .respond_with(ResponseTemplate::new(202))
        .mount(&server)
        .await;
    let blob = "a".repeat(20_000);
    Mock::given(method("POST"))
        .and(BodyRpc("tools/list"))
        .respond_with(move |req: &Request| {
            json_response(
                rpc_id(req),
                json!({
                    "tools": [{
                        "name": "ok",
                        "description": blob,
                        "inputSchema": {"type": "object"}
                    }]
                }),
            )
        })
        .mount(&server)
        .await;

    let limits = McpLimits {
        max_description_bytes: 1024,
        ..McpLimits::default()
    };
    let client = client(&server, limits);
    client.initialize().await.unwrap();
    let err = client.tools_list().await.unwrap_err();
    assert!(matches!(err, McpError::SchemaViolation(_)), "got {err:?}");
}

#[tokio::test]
async fn schema_trust_rejects_too_many_tools() {
    let server = MockServer::start().await;
    mount_initialize(&server).await;
    Mock::given(method("POST"))
        .and(BodyRpc("notifications/initialized"))
        .respond_with(ResponseTemplate::new(202))
        .mount(&server)
        .await;
    let many: Vec<Value> = (0..600)
        .map(|i| {
            json!({
                "name": format!("t{i}"),
                "description": "x",
                "inputSchema": {"type": "object"}
            })
        })
        .collect();
    Mock::given(method("POST"))
        .and(BodyRpc("tools/list"))
        .respond_with(move |req: &Request| {
            json_response(rpc_id(req), json!({"tools": many.clone()}))
        })
        .mount(&server)
        .await;

    let client = client(&server, McpLimits::default());
    client.initialize().await.unwrap();
    let err = client.tools_list().await.unwrap_err();
    assert!(
        matches!(
            err,
            McpError::ToolCountExceeded {
                kept: 513,
                cap: 512
            }
        ),
        "got {err:?}"
    );
}

#[tokio::test]
async fn keep_filter_lets_over_cap_server_register_a_subset() {
    let server = MockServer::start().await;
    mount_initialize(&server).await;
    Mock::given(method("POST"))
        .and(BodyRpc("notifications/initialized"))
        .respond_with(ResponseTemplate::new(202))
        .mount(&server)
        .await;
    // 600 tools: 20 `keep_*` and 580 `drop_*`. Over the 512 cap raw, but
    // the kept subset is tiny.
    let many: Vec<Value> = (0..600)
        .map(|i| {
            let name = if i < 20 {
                format!("keep_{i}")
            } else {
                format!("drop_{i}")
            };
            json!({"name": name, "description": "x", "inputSchema": {"type": "object"}})
        })
        .collect();
    Mock::given(method("POST"))
        .and(BodyRpc("tools/list"))
        .respond_with(move |req: &Request| {
            json_response(rpc_id(req), json!({"tools": many.clone()}))
        })
        .mount(&server)
        .await;

    let client = client(&server, McpLimits::default());
    client.initialize().await.unwrap();
    // Unfiltered: aborts at cap+1.
    assert!(client.tools_list().await.is_err());
    // Filtered to `keep_*`: only 20 counted against the cap → succeeds.
    let kept = client
        .tools_list_filtered(|n| n.starts_with("keep_"))
        .await
        .unwrap();
    assert_eq!(kept.len(), 20);
    assert!(kept.iter().all(|t| t.name.starts_with("keep_")));
}

#[tokio::test]
async fn raw_bound_stops_unbounded_paging_even_when_filtering() {
    let server = MockServer::start().await;
    mount_initialize(&server).await;
    Mock::given(method("POST"))
        .and(BodyRpc("notifications/initialized"))
        .respond_with(ResponseTemplate::new(202))
        .mount(&server)
        .await;
    let many: Vec<Value> = (0..50)
        .map(|i| json!({"name": format!("d{i}"), "description": "x", "inputSchema": {"type": "object"}}))
        .collect();
    Mock::given(method("POST"))
        .and(BodyRpc("tools/list"))
        .respond_with(move |req: &Request| {
            json_response(rpc_id(req), json!({"tools": many.clone()}))
        })
        .mount(&server)
        .await;

    // Tiny raw bound: even a keep-nothing filter must trip it.
    let limits = McpLimits {
        max_raw_tools: 10,
        ..McpLimits::default()
    };
    let client = client(&server, limits);
    client.initialize().await.unwrap();
    let err = client.tools_list_filtered(|_| false).await.unwrap_err();
    assert!(
        matches!(err, McpError::RawToolLimitExceeded { limit: 10 }),
        "got {err:?}"
    );
}

#[tokio::test]
async fn list_tool_names_returns_names_without_registration_cap() {
    let server = MockServer::start().await;
    mount_initialize(&server).await;
    Mock::given(method("POST"))
        .and(BodyRpc("notifications/initialized"))
        .respond_with(ResponseTemplate::new(202))
        .mount(&server)
        .await;
    // 800 tools — well over the 512 registration cap, under the raw bound.
    let many: Vec<Value> = (0..800)
        .map(|i| json!({"name": format!("t{i}"), "description": format!("desc {i}"), "inputSchema": {"type": "object"}}))
        .collect();
    Mock::given(method("POST"))
        .and(BodyRpc("tools/list"))
        .respond_with(move |req: &Request| {
            json_response(rpc_id(req), json!({"tools": many.clone()}))
        })
        .mount(&server)
        .await;

    let probe = client(&server, McpLimits::default());
    probe.initialize().await.unwrap();
    let survey = probe.list_tool_names().await.unwrap();
    assert_eq!(survey.names.len(), 800);
    assert!(!survey.truncated);
    assert_eq!(survey.names[0].description, "desc 0");

    // With a raw bound below the count, it truncates and says so.
    let bounded = client(
        &server,
        McpLimits {
            max_raw_tools: 100,
            ..McpLimits::default()
        },
    );
    bounded.initialize().await.unwrap();
    let survey = bounded.list_tool_names().await.unwrap();
    assert_eq!(survey.names.len(), 100);
    assert!(survey.truncated);
}

#[tokio::test]
async fn body_cap_rejects_oversize_response() {
    let server = MockServer::start().await;
    mount_initialize(&server).await;
    Mock::given(method("POST"))
        .and(BodyRpc("notifications/initialized"))
        .respond_with(ResponseTemplate::new(202))
        .mount(&server)
        .await;
    let huge = "x".repeat(50_000);
    Mock::given(method("POST"))
        .and(BodyRpc("tools/list"))
        .respond_with(move |req: &Request| {
            json_response(
                rpc_id(req),
                json!({"tools": [{
                    "name": "ok",
                    "description": huge,
                    "inputSchema": {"type": "object"}
                }]}),
            )
        })
        .mount(&server)
        .await;

    let limits = McpLimits {
        max_content_bytes: 4096,
        ..McpLimits::default()
    };
    let client = client(&server, limits);
    client.initialize().await.unwrap();
    let err = client.tools_list().await.unwrap_err();
    assert!(
        matches!(err, McpError::BodyTooLarge { limit: 4096 }),
        "got {err:?}"
    );
}

#[tokio::test]
async fn http_401_maps_to_auth_expired() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(BodyRpc("initialize"))
        .respond_with(ResponseTemplate::new(401).set_body_string("token expired"))
        .mount(&server)
        .await;

    let client = client(&server, McpLimits::default());
    let err = client.initialize().await.unwrap_err();
    assert!(matches!(err, McpError::AuthExpired(_)), "got {err:?}");
}

#[tokio::test]
async fn server_jsonrpc_error_maps_to_server_error() {
    let server = MockServer::start().await;
    mount_initialize(&server).await;
    Mock::given(method("POST"))
        .and(BodyRpc("notifications/initialized"))
        .respond_with(ResponseTemplate::new(202))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(BodyRpc("tools/call"))
        .respond_with(|req: &Request| {
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_json(json!({
                    "jsonrpc": "2.0",
                    "id": rpc_id(req),
                    "error": {"code": -32602, "message": "invalid params"}
                }))
        })
        .mount(&server)
        .await;

    let client = client(&server, McpLimits::default());
    client.initialize().await.unwrap();
    let err = client
        .tools_call("oops", json!({}), CancellationToken::new())
        .await
        .unwrap_err();
    match err {
        McpError::ServerError { code, message, .. } => {
            assert_eq!(code, -32602);
            assert!(message.contains("invalid params"));
        }
        other => panic!("expected ServerError, got {other:?}"),
    }
}

async fn initialize_err(endpoint: &str, http: reqwest::Client) -> McpError {
    client_at(endpoint, http, McpLimits::default())
        .initialize()
        .await
        .unwrap_err()
}

const SECRETS: [&str; 5] = ["opsuser7", "hunter2", "k-acme42", "s3cr3t", "fr4gment"];

fn secret_endpoint(addr: impl std::fmt::Display) -> String {
    format!("http://opsuser7:hunter2@{addr}/mcp/k-acme42?sig=s3cr3t#fr4gment")
}

fn assert_no_secrets(err: &McpError) {
    for text in [format!("{err}"), format!("{err:?}")] {
        for secret in SECRETS {
            assert!(!text.contains(secret), "`{secret}` leaked: {text}");
        }
    }
}

#[tokio::test]
async fn http_status_errors_name_the_host_but_no_endpoint_secret() {
    let cases: [(u16, fn(&McpError) -> bool); 3] = [
        (401, |e| matches!(e, McpError::AuthExpired(_))),
        (429, |e| matches!(e, McpError::RateLimited { .. })),
        (500, |e| matches!(e, McpError::Transport(_))),
    ];
    for (status, expected) in cases {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(status).set_body_string("internal"))
            .mount(&server)
            .await;

        let err = initialize_err(&secret_endpoint(server.address()), reqwest::Client::new()).await;
        assert!(expected(&err), "HTTP {status} gave {err:?}");
        assert!(
            err.to_string().contains(&server.address().to_string()),
            "HTTP {status} gave {err}"
        );
        assert_no_secrets(&err);
    }
}

#[tokio::test]
async fn a_refused_connection_names_the_host_but_no_endpoint_secret() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);

    let err = initialize_err(&secret_endpoint(addr), reqwest::Client::new()).await;
    assert!(matches!(err, McpError::Unreachable(_)), "got {err:?}");
    assert!(err.to_string().contains(&addr.to_string()), "got {err}");
    assert_no_secrets(&err);
}

#[tokio::test]
async fn a_timeout_names_the_host_but_no_endpoint_secret() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(5)))
        .mount(&server)
        .await;
    let http = reqwest::Client::builder()
        .timeout(Duration::from_millis(200))
        .build()
        .unwrap();

    let err = initialize_err(&secret_endpoint(server.address()), http).await;
    assert!(matches!(err, McpError::Unreachable(_)), "got {err:?}");
    assert!(
        err.to_string().contains(&server.address().to_string()),
        "got {err}"
    );
    assert_no_secrets(&err);
}

#[tokio::test]
async fn a_rejected_scheme_names_no_endpoint_secret() {
    let err = initialize_err(
        "ftp://opsuser7:hunter2@127.0.0.1/mcp/k-acme42?sig=s3cr3t#fr4gment",
        reqwest::Client::new(),
    )
    .await;
    assert!(matches!(err, McpError::Transport(_)), "got {err:?}");
    assert!(err.to_string().contains("ftp://127.0.0.1/"), "got {err}");
    assert_no_secrets(&err);
}

#[tokio::test]
async fn a_refused_connection_is_unreachable() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);

    let err = initialize_err(&format!("http://{addr}/"), reqwest::Client::new()).await;
    assert!(matches!(err, McpError::Unreachable(_)), "got {err:?}");
    assert!(err.is_transport());
}

#[tokio::test]
async fn a_failed_tls_handshake_is_unreachable() {
    let server = MockServer::start().await;
    let endpoint = server.uri().replacen("http://", "https://", 1);

    let err = initialize_err(&endpoint, reqwest::Client::new()).await;
    assert!(matches!(err, McpError::Unreachable(_)), "got {err:?}");
}

#[tokio::test]
async fn a_request_timing_out_before_any_response_is_unreachable() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(5)))
        .mount(&server)
        .await;
    let http = reqwest::Client::builder()
        .timeout(Duration::from_millis(200))
        .build()
        .unwrap();

    let err = initialize_err(&server.uri(), http).await;
    assert!(matches!(err, McpError::Unreachable(_)), "got {err:?}");
}

#[tokio::test]
async fn an_http_500_is_a_transport_error_not_unreachable() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500).set_body_string("internal"))
        .mount(&server)
        .await;

    let err = initialize_err(&server.uri(), reqwest::Client::new()).await;
    assert!(
        matches!(err, McpError::Transport(ref m) if m.contains("HTTP 500")),
        "got {err:?}"
    );
    assert!(!err.is_unreachable());
}

/// What a raw server does after sending a 200 that promises more body than
/// it writes.
#[derive(Clone, Copy)]
enum AfterHeaders {
    HangUp,
    Stall,
}

/// A server that reads the whole request, then sends a 200 promising more
/// body than it writes, so the failure comes from reading the body after
/// the response headers arrived.
async fn serve_truncated_body(after: AfterHeaders) -> std::net::SocketAddr {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        let mut buf = [0u8; 4096];
        while !request_complete(&request) {
            let n = socket.read(&mut buf).await.unwrap();
            assert!(n > 0, "client closed before sending the whole request");
            request.extend_from_slice(&buf[..n]);
        }
        socket
            .write_all(
                b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
                  content-length: 100\r\n\r\n{\"jsonrpc\":",
            )
            .await
            .unwrap();
        match after {
            AfterHeaders::HangUp => socket.shutdown().await.unwrap(),
            AfterHeaders::Stall => std::future::pending().await,
        }
    });
    addr
}

fn request_complete(request: &[u8]) -> bool {
    let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") else {
        return false;
    };
    let head = String::from_utf8_lossy(&request[..end]).to_ascii_lowercase();
    let body_len = head
        .lines()
        .find_map(|line| line.strip_prefix("content-length:"))
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(0);
    request.len() >= end + 4 + body_len
}

#[tokio::test]
async fn a_body_cut_off_after_the_headers_is_a_transport_error() {
    let addr = serve_truncated_body(AfterHeaders::HangUp).await;

    let err = initialize_err(&format!("http://{addr}/"), reqwest::Client::new()).await;
    assert!(
        matches!(err, McpError::Transport(ref m) if m.contains("response body")),
        "got {err:?}"
    );
    assert!(!err.is_unreachable());
}

#[tokio::test]
async fn a_timeout_after_the_headers_is_a_transport_error_not_unreachable() {
    let addr = serve_truncated_body(AfterHeaders::Stall).await;
    let http = reqwest::Client::builder()
        .timeout(Duration::from_millis(200))
        .build()
        .unwrap();

    let err = initialize_err(&format!("http://{addr}/"), http).await;
    assert!(
        matches!(err, McpError::Transport(ref m) if m.contains("timed out")),
        "got {err:?}"
    );
    assert!(!err.is_unreachable());
}
