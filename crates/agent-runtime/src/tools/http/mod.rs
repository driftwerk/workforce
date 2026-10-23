//! Generic outbound HTTP tools (`http.*`) — an essential built-in family
//! (registered in the core toolset, not a loadable skill) for calling services
//! that have no dedicated adapter.
//!
//!
//! Two modes, and the boundary between them is the security core. An *endpoint*
//! is operator-provisioned in the vault (namespace `http/`, denied to the
//! agent's `vault.*` tools): the agent names it and supplies a path, and the
//! base URL and credential come from configuration. An *ad-hoc* request names a
//! full URL and carries no stored credential. The invariant that falls out is
//! that a *stored* credential is only ever sent to the endpoint it is
//! namespaced under, so no agent-chosen host — and no host an injected
//! instruction names — can receive one.
//!
//! Either mode may also carry a `[[secret:<name>]]` the user set for this
//! conversation, or a `[[vault:<scope>:<key>]]` a workflow declared. Neither is
//! bound to an endpoint: the first is pinned by the user having supplied it for
//! this task (the trust `browser.fill` already acts on), the second by its
//! workflow's `allowed_hosts`. Every form resolves in
//! [`crate::security::credential_refs`], never in the agent's view of its own
//! arguments.
//!
//! Deliberately named `http.*` rather than `web.*`: `WORKER_TOOL_PATTERNS`
//! (`hub::serve`) grants autonomous workers `web.*` as a glob, so a `web.`-
//! prefixed name would silently hand every worker sub-agent an arbitrary-host
//! authenticated request.

pub mod config;
mod request;
mod tools;

use std::sync::Arc;
use std::time::Duration;

use eyre::{eyre, Result};
use serde_json::{json, Value};
use workforce_common::settings::SecretStore;

use crate::runner::{CURRENT_AGENT_ID, CURRENT_USER_ID};
use crate::security::external_content::{tokenize_for_egress, wrap_external_content};
use crate::security::ssrf::{check_url_for_ssrf, ssrf_safe_client};
use crate::tools::Tool;

use config::{EndpointConfig, MAX_TIMEOUT_MS};
use request::{Method, PreparedRequest};

pub use request::is_mutating_request;

/// Every `http.*` tool name, in registration order.
pub const TOOL_NAMES: &[&str] = &["http.request", "http.endpoints"];

/// Ceiling on the response body read from the wire.
const MAX_BODY_SIZE: usize = 5 * 1024 * 1024;

/// The message shown when an agent names an endpoint that isn't configured, or
/// asks for one when none exist. Names the operator command, and says plainly
/// not to ask the user for the secret — the same posture as `database.*`.
fn setup_hint(configured: &[EndpointConfig]) -> String {
    let known = match configured {
        [] => "No HTTP endpoints are configured.".to_string(),
        endpoints => format!(
            "Configured endpoints: {}.",
            endpoints
                .iter()
                .map(|c| c.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    };
    format!(
        "{known} You cannot set one up yourself — the `http/*` vault namespace is off-limits \
         to the vault.* tools, and you cannot issue a `cmd:`. Tell the USER to run these in \
         this channel; each line is a command they type, not something you call:\n\
         • `cmd: endpoint list` — what exists and what each one is still missing.\n\
         • `cmd: endpoint add <name> <https://host>` — names the host. ADMIN ONLY, because \
         whoever sets it decides where every credential under it is sent.\n\
         • `cmd: endpoint auth <name> <bearer|basic|header|none>` — admin only.\n\
         • `cmd: endpoint cred <name> <key> <value>` — ANY user, stored against their own \
         identity. You then reference it as `[[vault:<key>]]`. Reserved keys `token`, \
         `username`, `password` become the endpoint's own auth instead.\n\
         An operator can equivalently run `workforce vault set http/endpoints/<name>/…` on \
         the host. For a ONE-OFF call no endpoint is needed at all: ask for \
         `cmd: secret <name> <value>` and pass `[[secret:<name>]]` against an absolute `url`. \
         Either way the value is swapped in server-side and you never see it. Do not ask the \
         user to paste a credential as ordinary chat text, and never claim the call is \
         impossible — one of these routes always applies."
    )
}

/// Shared state for the `http.*` tools: the credential store plus the two
/// redirect policies. Clients are built once — each owns a connection pool, so
/// building per request would discard connection reuse.
pub struct HttpRuntime {
    store: Arc<dyn SecretStore>,
    following: reqwest::Client,
    direct: reqwest::Client,
}

impl HttpRuntime {
    pub fn new(store: Arc<dyn SecretStore>) -> Arc<Self> {
        let ceiling = Duration::from_millis(MAX_TIMEOUT_MS);
        Arc::new(Self {
            store,
            following: ssrf_safe_client(ceiling, true),
            direct: ssrf_safe_client(ceiling, false),
        })
    }

    /// Configured endpoints, re-read each call so vault edits take effect
    /// without a restart.
    pub fn endpoints(&self) -> Vec<EndpointConfig> {
        config::list_endpoints(self.store.as_ref())
    }

    /// The credential store, for the secret-free reporting `http.endpoints`
    /// does over it.
    pub fn store(&self) -> Arc<dyn SecretStore> {
        Arc::clone(&self.store)
    }

    fn load_endpoint(&self, name: &str) -> Result<EndpointConfig> {
        config::read_endpoint(self.store.as_ref(), name).ok_or_else(|| {
            eyre!(
                "unknown endpoint '{name}'. {}",
                setup_hint(&self.endpoints())
            )
        })
    }

    /// Apply the network policy, then issue the request. The SSRF check lives
    /// here rather than in [`request::prepare`] so it guards every dispatch
    /// path, including a URL that only became concrete after query building.
    async fn send(&self, prepared: PreparedRequest) -> Result<Value> {
        if let Err(reason) = check_url_for_ssrf(&prepared.url) {
            // The refused request is the one an auditor most wants recorded, so
            // it is audited before the early return rather than only in dispatch.
            audit(
                prepared.method,
                prepared.endpoint.as_deref(),
                &prepared.url,
                None,
            );
            return Err(eyre!("{reason}"));
        }
        self.dispatch(prepared).await
    }

    /// Issue one request and shape the response for the model.
    async fn dispatch(&self, prepared: PreparedRequest) -> Result<Value> {
        let client = if prepared.follow_redirects {
            &self.following
        } else {
            &self.direct
        };

        let mut builder = client
            .request(prepared.method.into(), prepared.url.clone())
            .headers(prepared.headers)
            .timeout(prepared.timeout);
        if let Some(body) = prepared.body {
            if let Some(content_type) = body.content_type {
                builder = builder.header(reqwest::header::CONTENT_TYPE, content_type);
            }
            builder = builder.body(body.bytes);
        }

        let outcome = builder.send().await;
        audit(
            prepared.method,
            prepared.endpoint.as_deref(),
            &prepared.url,
            outcome.as_ref().map(reqwest::Response::status).ok(),
        );

        let response = outcome.map_err(|e| {
            let reason = if e.is_timeout() {
                format!("timed out after {}ms", prepared.timeout.as_millis())
            } else if e.is_connect() {
                format!("could not connect: {e}")
            } else {
                e.to_string()
            };
            eyre!("request failed: {reason}")
        })?;

        shape_response(response, prepared.max_chars).await
    }
}

/// Whether a content type declares JSON.
fn declares_json(headers: &reqwest::header::HeaderMap) -> bool {
    headers
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| {
            let v = v.to_ascii_lowercase();
            v.contains("application/json") || v.contains("+json")
        })
}

/// Read the response and render it for the model: status and headers always, a
/// parsed JSON body when the content type says so, otherwise text truncated to
/// `max_chars`. A non-2xx is a successful *tool* call — the status carries the
/// outcome and a 4xx body is usually the diagnostic the agent needs.
///
/// Every string that reaches the model is tokenized first, so a credential in
/// the response (the access token a login endpoint returns, a `set-cookie`
/// session value) becomes a `[[secret:…]]` marker rather than cleartext in the
/// transcript. Tokenizing before the injection scan makes the scan's own
/// tokenization pass a documented no-op.
async fn shape_response(response: reqwest::Response, max_chars: usize) -> Result<Value> {
    let status = response.status();
    let url = response.url().to_string();
    let is_json = declares_json(response.headers());
    let headers: serde_json::Map<String, Value> = response
        .headers()
        .iter()
        .filter_map(|(name, value)| {
            value.to_str().ok().map(|v| {
                (
                    name.as_str().to_string(),
                    Value::String(tokenize_for_egress(v, "http.request")),
                )
            })
        })
        .collect();

    // Bounded as it streams: buffering first and slicing after would let a
    // hostile endpoint exhaust memory before the cap was ever consulted.
    let raw = crate::tools::web::read_body_limited(response, MAX_BODY_SIZE).await?;
    let text = tokenize_for_egress(&String::from_utf8_lossy(&raw), "http.request");

    let scan = wrap_external_content(&text, "http.request");
    if scan.suspicious {
        tracing::warn!(
            url = %url,
            score = scan.score,
            hostile = scan.hostile,
            matches = ?scan.matches.iter().map(|m| m.description).collect::<Vec<_>>(),
            "Suspicious patterns detected in HTTP response"
        );
    }

    // `max_chars` binds the JSON path too. Parsing first and returning the whole
    // value would let a large JSON response past the only cap the model's
    // context has — the byte ceiling above is about memory, not context.
    let oversized = text.chars().count() > max_chars;
    let (body, truncated) = match serde_json::from_str::<Value>(&text) {
        Ok(parsed) if is_json && !oversized => (parsed, false),
        _ if oversized => (
            Value::String(text.chars().take(max_chars).collect::<String>()),
            true,
        ),
        _ => (Value::String(text), false),
    };

    Ok(json!({
        "status": status.as_u16(),
        "ok": status.is_success(),
        "url": tokenize_for_egress(&url, "http.request"),
        "headers": headers,
        "body": body,
        "truncated": truncated,
        "_security": {
            "source": "external",
            "suspicious": scan.suspicious,
            "hostile": scan.hostile,
            "score": scan.score,
        }
    }))
}

/// One audit record per request. Scheme and host only — a full URL can carry a
/// token in its query string — and never headers or body.
fn audit(
    method: Method,
    endpoint: Option<&str>,
    url: &url::Url,
    status: Option<reqwest::StatusCode>,
) {
    let actor = CURRENT_USER_ID
        .try_with(|u| u.clone())
        .ok()
        .filter(|u| !u.is_empty());
    let agent = CURRENT_AGENT_ID
        .try_with(|a| a.clone())
        .ok()
        .filter(|a| !a.is_empty());
    tracing::info!(
        target: "sentinel.audit",
        tool = "http.request",
        method = %method.as_str(),
        endpoint = endpoint.unwrap_or("-"),
        host = %url.host_str().unwrap_or("-"),
        scheme = %url.scheme(),
        status = ?status.map(|s| s.as_u16()),
        actor = ?actor,
        agent = ?agent,
        ok = status.is_some_and(|s| s.is_success()),
        "outbound http request"
    );
}

/// Build the `http.*` tool family.
pub fn http_tools(store: Arc<dyn SecretStore>) -> Vec<Arc<dyn Tool>> {
    let runtime = HttpRuntime::new(store);
    vec![
        Arc::new(tools::HttpRequestTool::new(Arc::clone(&runtime))),
        Arc::new(tools::HttpEndpointsTool::new(runtime)),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;
    use wiremock::matchers::{body_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[derive(Default)]
    struct StubStore(Mutex<HashMap<String, String>>);

    impl StubStore {
        fn with(entries: &[(&str, &str)]) -> Arc<dyn SecretStore> {
            let store = StubStore::default();
            for (k, v) in entries {
                store
                    .0
                    .lock()
                    .unwrap()
                    .insert((*k).to_string(), (*v).to_string());
            }
            Arc::new(store)
        }
    }

    impl SecretStore for StubStore {
        fn get(&self, path: &str) -> Option<String> {
            self.0.lock().unwrap().get(path).cloned()
        }
        fn set(&self, _: &str, _: &str) -> eyre::Result<()> {
            unreachable!("tests do not write")
        }
        fn delete(&self, _: &str) -> eyre::Result<bool> {
            unreachable!("tests do not delete")
        }
        fn list(&self, prefix: Option<&str>) -> Vec<String> {
            self.0
                .lock()
                .unwrap()
                .keys()
                .filter(|k| prefix.is_none_or(|p| k.starts_with(p)))
                .cloned()
                .collect()
        }
    }

    /// A runtime whose endpoint `svc` points at `server`, with the vault
    /// entries in `extra`. `dispatch` is used rather than `send` because
    /// wiremock binds to loopback, which the SSRF policy correctly refuses —
    /// that policy is covered by `send_rejects_a_loopback_target` and by
    /// `security::ssrf`'s own tests.
    fn runtime_for(server: &MockServer, extra: &[(&str, &str)]) -> Arc<HttpRuntime> {
        let base = server.uri();
        let mut entries: Vec<(&str, &str)> = vec![("http/endpoints/svc/base_url", &base)];
        entries.extend_from_slice(extra);
        HttpRuntime::new(StubStore::with(&entries))
    }

    async fn call(runtime: &HttpRuntime, input: Value) -> Value {
        let prepared = request::prepare(&input, &runtime.store, |name| runtime.load_endpoint(name))
            .expect("prepare");
        runtime.dispatch(prepared).await.expect("dispatch")
    }

    #[tokio::test]
    async fn post_sends_method_path_content_type_and_json_body() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/login/get_access_tokens"))
            .and(header("content-type", "application/json"))
            .and(body_json(serde_json::json!({"email": "a@b.com"})))
            .respond_with(
                ResponseTemplate::new(201).set_body_json(serde_json::json!({"token": "t"})),
            )
            .mount(&server)
            .await;

        let runtime = runtime_for(&server, &[]);
        let out = call(
            &runtime,
            json!({
                "method": "POST", "endpoint": "svc", "path": "/login/get_access_tokens",
                "body": {"email": "a@b.com"}
            }),
        )
        .await;

        assert_eq!(out["status"], 201);
        assert_eq!(out["ok"], true);
        assert_eq!(out["body"]["token"], "t");
        assert_eq!(out["truncated"], false);
    }

    #[tokio::test]
    async fn bearer_credentials_reach_the_server() {
        let server = MockServer::start().await;
        Mock::given(header("authorization", "Bearer configured-token"))
            .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
            .mount(&server)
            .await;

        let runtime = runtime_for(&server, &[("http/endpoints/svc/token", "configured-token")]);
        let out = call(
            &runtime,
            json!({"method": "GET", "endpoint": "svc", "path": "/x"}),
        )
        .await;
        assert_eq!(out["status"], 200);
    }

    #[tokio::test]
    async fn basic_credentials_reach_the_server() {
        let server = MockServer::start().await;
        Mock::given(header("authorization", "Basic dTpw"))
            .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
            .mount(&server)
            .await;

        let runtime = runtime_for(
            &server,
            &[
                ("http/endpoints/svc/username", "u"),
                ("http/endpoints/svc/password", "p"),
            ],
        );
        let out = call(
            &runtime,
            json!({"method": "GET", "endpoint": "svc", "path": "/x"}),
        )
        .await;
        assert_eq!(out["status"], 200);
    }

    #[tokio::test]
    async fn custom_header_credentials_reach_the_server() {
        let server = MockServer::start().await;
        Mock::given(header("x-api-key", "k"))
            .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
            .mount(&server)
            .await;

        let runtime = runtime_for(
            &server,
            &[
                ("http/endpoints/svc/token", "k"),
                ("http/endpoints/svc/auth", "header"),
            ],
        );
        let out = call(
            &runtime,
            json!({"method": "GET", "endpoint": "svc", "path": "/x"}),
        )
        .await;
        assert_eq!(out["status"], 200);
    }

    #[tokio::test]
    async fn a_body_credential_resolves_from_the_vault_and_reaches_the_server() {
        let server = MockServer::start().await;
        Mock::given(body_json(
            serde_json::json!({"email": "a@b.com", "password": "hunter2"}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
        .mount(&server)
        .await;

        let runtime = runtime_for(
            &server,
            &[
                ("http/endpoints/svc/creds/email", "a@b.com"),
                ("http/endpoints/svc/creds/password", "hunter2"),
            ],
        );
        let out = call(
            &runtime,
            json!({
                "method": "POST", "endpoint": "svc", "path": "/login",
                "body": {"email": "[[vault:email]]", "password": "[[vault:password]]"}
            }),
        )
        .await;
        assert_eq!(out["status"], 200);
    }

    #[tokio::test]
    async fn an_authenticated_request_does_not_follow_a_redirect() {
        let server = MockServer::start().await;
        Mock::given(path("/start"))
            .respond_with(ResponseTemplate::new(302).insert_header("location", "/elsewhere"))
            .mount(&server)
            .await;
        Mock::given(path("/elsewhere"))
            .respond_with(ResponseTemplate::new(200).set_body_string("followed"))
            .mount(&server)
            .await;

        let runtime = runtime_for(&server, &[("http/endpoints/svc/token", "tok")]);
        let out = call(
            &runtime,
            json!({"method": "GET", "endpoint": "svc", "path": "/start"}),
        )
        .await;

        assert_eq!(out["status"], 302, "credentialled request must not follow");
        assert!(!out["body"]
            .as_str()
            .unwrap_or_default()
            .contains("followed"));
    }

    /// The complement of the test above: an *un*authenticated request does try
    /// to follow, and the redirect policy re-runs the SSRF check on the hop —
    /// so the attempt is refused rather than silently reaching loopback. Both
    /// halves of the redirect contract are covered by the pair.
    #[tokio::test]
    async fn an_unauthenticated_redirect_hop_is_ssrf_checked() {
        let server = MockServer::start().await;
        Mock::given(path("/start"))
            .respond_with(ResponseTemplate::new(302).insert_header("location", "/elsewhere"))
            .mount(&server)
            .await;

        let runtime = runtime_for(&server, &[]);
        let prepared = request::prepare(
            &json!({"method": "GET", "endpoint": "svc", "path": "/start"}),
            &runtime.store,
            |name| runtime.load_endpoint(name),
        )
        .expect("prepare");
        assert!(prepared.follow_redirects);

        let err = runtime.dispatch(prepared).await.expect_err("hop blocked");
        assert!(err.to_string().contains("redirect"), "{err}");
    }

    #[tokio::test]
    async fn a_non_2xx_is_a_successful_call_carrying_the_status_and_body() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(422).set_body_json(serde_json::json!({"error": "bad"})),
            )
            .mount(&server)
            .await;

        let runtime = runtime_for(&server, &[]);
        let out = call(
            &runtime,
            json!({"method": "GET", "endpoint": "svc", "path": "/x"}),
        )
        .await;
        assert_eq!(out["status"], 422);
        assert_eq!(out["ok"], false);
        assert_eq!(out["body"]["error"], "bad");
    }

    #[tokio::test]
    async fn a_non_json_body_is_truncated_at_max_chars() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string("x".repeat(500)))
            .mount(&server)
            .await;

        let runtime = runtime_for(&server, &[]);
        let out = call(
            &runtime,
            json!({"method": "GET", "endpoint": "svc", "path": "/x", "max_chars": 10}),
        )
        .await;
        assert_eq!(out["body"], "xxxxxxxxxx");
        assert_eq!(out["truncated"], true);
    }

    #[tokio::test]
    async fn a_json_content_type_is_parsed_rather_than_returned_as_text() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"n": [1, 2]})),
            )
            .mount(&server)
            .await;

        let runtime = runtime_for(&server, &[]);
        let out = call(
            &runtime,
            json!({"method": "GET", "endpoint": "svc", "path": "/x"}),
        )
        .await;
        assert_eq!(out["body"]["n"][1], 2);
    }

    #[tokio::test]
    async fn response_headers_are_returned() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("x-request-id", "abc")
                    .set_body_string("ok"),
            )
            .mount(&server)
            .await;

        let runtime = runtime_for(&server, &[]);
        let out = call(
            &runtime,
            json!({"method": "GET", "endpoint": "svc", "path": "/x"}),
        )
        .await;
        assert_eq!(out["headers"]["x-request-id"], "abc");
    }

    #[tokio::test]
    async fn send_rejects_a_loopback_target() {
        let runtime = HttpRuntime::new(StubStore::with(&[]));
        let prepared = request::prepare(
            &json!({"method": "GET", "url": "http://127.0.0.1:9/x"}),
            &runtime.store,
            |name| runtime.load_endpoint(name),
        )
        .expect("prepare");
        let err = runtime.send(prepared).await.expect_err("blocked");
        assert!(err.to_string().contains("private/internal"), "{err}");
    }

    #[tokio::test]
    async fn an_unknown_endpoint_names_the_operator_command() {
        let runtime = HttpRuntime::new(StubStore::with(&[]));
        let err = request::prepare(
            &json!({"method": "GET", "endpoint": "nope", "path": "/x"}),
            &runtime.store,
            |name| runtime.load_endpoint(name),
        )
        .expect_err("unknown endpoint");
        let msg = err.to_string();
        assert!(msg.contains("workforce vault set"), "{msg}");
        assert!(msg.contains("Do not ask the user"), "{msg}");
        // Both credential routes, so an agent that hits this never concludes
        // the call is impossible when the user could just set a secret.
        assert!(msg.contains("cmd: secret <name> <value>"), "{msg}");
        // The chat route, and which half of it a non-admin can run — an agent
        // that only knew the host command would send the user to a shell they
        // may not have.
        assert!(msg.contains("cmd: endpoint add"), "{msg}");
        assert!(msg.contains("cmd: endpoint cred"), "{msg}");
        assert!(msg.contains("ADMIN ONLY"), "{msg}");
    }

    /// End to end over the wire: the remote service must receive the real
    /// credential, while the input the runner records for the transcript keeps
    /// the marker. `http.request` is deliberately absent from the secret-sink
    /// registry — it resolves markers itself — so the runner's substitution
    /// pass is a no-op over it and cannot double-substitute.
    ///
    /// Not a `#[tokio::test]`: the global-cache lock is a `std::sync::Mutex`
    /// shared with the synchronous resolver tests, so the runtime is driven
    /// from inside the guard rather than the guard held across an await.
    #[test]
    fn a_session_secret_reaches_the_server_but_never_the_transcript() {
        let _guard = crate::security::test_support::SECRET_CACHE_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        workforce_common::secret_marker::clear_global_cache();
        workforce_common::secret_marker::set_global_cache(
            crate::security::test_support::stub_named_secret_cache(&[
                ("sess-1", "email", "a@b.com"),
                ("sess-1", "password", "hunter2"),
            ]),
        );
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(session_secret_over_the_wire());
        workforce_common::secret_marker::clear_global_cache();
    }

    async fn session_secret_over_the_wire() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/login"))
            .and(body_json(json!({
                "client_id": "user-service",
                "email": "a@b.com",
                "password": "hunter2",
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"token": "t"})))
            .mount(&server)
            .await;

        let input = json!({
            "method": "POST", "endpoint": "svc", "path": "/login",
            "body": {
                "client_id": "user-service",
                "email": "[[secret:email]]",
                "password": "[[secret:password]]",
            }
        });

        let runtime = runtime_for(&server, &[]);
        let out = crate::runner::CURRENT_SESSION_KEY
            .scope("sess-1".to_string(), call(&runtime, input.clone()))
            .await;
        // wiremock only answers a request matching the cleartext body.
        assert_eq!(out["status"], 200);

        let recorded = crate::security::secret_sinks::substitute_secret_markers(
            "http.request",
            input.clone(),
            "sess-1",
        );
        assert_eq!(
            recorded.input, input,
            "the runner must not rewrite the call"
        );
        assert!(recorded.original.is_none(), "nothing was substituted");
        let recorded = recorded.input.to_string();
        assert!(recorded.contains("[[secret:password]]"), "{recorded}");
        assert!(!recorded.contains("hunter2"), "{recorded}");
    }

    #[test]
    fn tool_names_match_the_registered_family() {
        let names: Vec<String> = http_tools(StubStore::with(&[]))
            .iter()
            .map(|t| t.name().to_string())
            .collect();
        assert_eq!(names, TOOL_NAMES);
        assert!(names.iter().all(|n| n.starts_with("http.")));
    }
}
