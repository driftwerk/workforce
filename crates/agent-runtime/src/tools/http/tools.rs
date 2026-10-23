//! The agent-callable `http.*` tools.

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use eyre::Result;
use serde_json::{json, Value};

use crate::tools::{Tool, ToolResult};

use super::config::{MAX_TIMEOUT_MS, MIN_TIMEOUT_MS};
use super::request::{prepare, MAX_MAX_CHARS};
use super::{setup_hint, HttpRuntime};

/// Above [`MAX_TIMEOUT_MS`] so the request's own timeout always fires first,
/// and below `runner::CHECK_IN_INTERVAL` as [`Tool::soft_deadline`] requires.
const SOFT_DEADLINE: Duration = Duration::from_secs(135);

pub struct HttpRequestTool {
    runtime: Arc<HttpRuntime>,
}

impl HttpRequestTool {
    pub fn new(runtime: Arc<HttpRuntime>) -> Self {
        Self { runtime }
    }
}

#[async_trait]
impl Tool for HttpRequestTool {
    fn name(&self) -> &str {
        "http.request"
    }

    fn description(&self) -> &str {
        "Make an HTTP request. Either name a configured `endpoint` and a `path` — the base \
         URL and credential come from operator configuration — or give an absolute `url`. \
         Call `http.endpoints` to see what is configured. Credentials never appear in your \
         arguments as cleartext: put a marker where the value goes and it is swapped in \
         server-side, so you can send a credential you have never seen. `[[secret:<name>]]` \
         (a value the user set with `cmd: secret`, or a credential redacted out of a message \
         they pasted) works with either `endpoint` or `url`. `[[vault:<key>]]` reads the \
         named endpoint's stored credentials and so requires `endpoint`, never a `url`. \
         `[[vault:<scope>:<key>]]` is a workflow secret variable and works in either mode \
         inside the workflow run that declares it. All resolve in `headers`, `query`, \
         `body`, and `body_text` — at any depth, so a JSON body can mix markers with \
         ordinary values."
    }

    fn soft_deadline(&self) -> Option<Duration> {
        Some(SOFT_DEADLINE)
    }

    fn approval_summary(&self, input: &Value) -> Option<String> {
        let method = input["method"].as_str()?.to_ascii_uppercase();
        match (input["endpoint"].as_str(), input["url"].as_str()) {
            (Some(endpoint), _) => Some(format!(
                "{method} {} on endpoint `{endpoint}`",
                input["path"].as_str().unwrap_or("/")
            )),
            (None, Some(url)) => Some(format!("{method} {url} (no credentials)")),
            (None, None) => None,
        }
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "method": {
                    "type": "string",
                    "enum": ["GET", "HEAD", "POST", "PUT", "PATCH", "DELETE"],
                    "description": "HTTP method"
                },
                "endpoint": {
                    "type": "string",
                    "description": "Name of a configured endpoint (see http.endpoints). Use with `path`; mutually exclusive with `url`."
                },
                "path": {
                    "type": "string",
                    "description": "Path starting with '/', appended to the endpoint's base URL"
                },
                "url": {
                    "type": "string",
                    "description": "Absolute http(s) URL for an unauthenticated ad-hoc request. Mutually exclusive with `endpoint`."
                },
                "headers": {
                    "type": "object",
                    "description": "Request headers as string key-value pairs. A value may be or contain a `[[secret:<name>]]` or `[[vault:<key>]]` marker."
                },
                "query": {
                    "type": "object",
                    "description": "Query parameters as string key-value pairs. A value may be or contain a `[[secret:<name>]]` or `[[vault:<key>]]` marker."
                },
                "body": {
                    "description": "JSON request body, sent as application/json. Any string leaf, at any depth, may be or contain a `[[secret:<name>]]` or `[[vault:<key>]]` marker; other leaves are sent as given, so a body can mix credentials with ordinary values. Mutually exclusive with `body_text`."
                },
                "body_text": {
                    "type": "string",
                    "description": "Raw request body. May contain `[[secret:<name>]]` or `[[vault:<key>]]` markers. Mutually exclusive with `body`."
                },
                "timeout_ms": {
                    "type": "integer",
                    "description": format!("Request timeout in milliseconds ({MIN_TIMEOUT_MS}–{MAX_TIMEOUT_MS})")
                },
                "max_chars": {
                    "type": "integer",
                    "description": format!("Cap on characters of a non-JSON response body (max {MAX_MAX_CHARS})")
                }
            },
            "required": ["method"]
        })
    }

    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let started = Instant::now();
        let outcome = match prepare(&input, &self.runtime.store, |name| {
            self.runtime.load_endpoint(name)
        }) {
            Ok(prepared) => self.runtime.send(prepared).await,
            Err(rejected) => Err(rejected),
        };
        let duration_ms = started.elapsed().as_millis() as u64;
        Ok(match outcome {
            Ok(output) => ToolResult::success(output, duration_ms),
            Err(e) => ToolResult::failure(e.to_string(), duration_ms),
        })
    }
}

pub struct HttpEndpointsTool {
    runtime: Arc<HttpRuntime>,
}

impl HttpEndpointsTool {
    pub fn new(runtime: Arc<HttpRuntime>) -> Self {
        Self { runtime }
    }
}

#[async_trait]
impl Tool for HttpEndpointsTool {
    fn name(&self) -> &str {
        "http.endpoints"
    }

    fn description(&self) -> &str {
        "List the HTTP endpoints configured for http.request — name, base URL, which kind of \
         authentication each uses, which `creds/` keys are set (so you know what \
         `[[vault:<key>]]` will resolve), and whether anything is still missing. Never \
         returns a credential value."
    }

    fn input_schema(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }

    async fn execute(&self, _input: Value) -> Result<ToolResult> {
        let started = Instant::now();
        let endpoints = self.runtime.endpoints();
        let store = self.runtime.store();
        let output = json!({
            "endpoints": endpoints
                .iter()
                .map(|c| c.summary_with_gaps(store.as_ref()))
                .collect::<Vec<_>>(),
            "hint": setup_hint(&endpoints),
        });
        Ok(ToolResult::success(
            output,
            started.elapsed().as_millis() as u64,
        ))
    }
}
