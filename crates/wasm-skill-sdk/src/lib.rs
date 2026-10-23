//! # workforce-skill-sdk
//!
//! Write custom workforce skill tools in Rust that compile to WASM.
//!
//! This crate wraps the raw WASM host ABI so you write normal Rust code
//! instead of pointer manipulation. Compile with `--target wasm32-unknown-unknown`
//! and distribute via a GitHub repo.
//!
//! ## Quick Start
//!
//! ```rust,ignore
//! use workforce_skill_sdk::prelude::*;
//!
//! workforce_skill_sdk::init!();
//!
//! tool!(get_weather, "Get current weather", {
//!     "type": "object",
//!     "properties": {
//!         "city": { "type": "string", "description": "City name" }
//!     },
//!     "required": ["city"]
//! }, |input: ToolInput| {
//!     let city = input.get_str("city").unwrap_or("unknown");
//!     let resp = http::get(
//!         &format!("https://wttr.in/{}?format=j1", city),
//!         &[],
//!     );
//!     match resp {
//!         Some(r) if r.is_success() => ToolOutput::success(json!({"weather": r.body})),
//!         _ => ToolOutput::error("Failed to fetch weather"),
//!     }
//! });
//! ```
//!
//! ## Architecture
//!
//! Skill authors write tool handlers that receive a `ToolInput` (the agent's
//! input parameters) and return a `ToolOutput` (success/failure with data).
//! The SDK handles serialisation, memory management, and host function calls.

use serde_json::Value;
use std::collections::HashMap;

// ─── Raw FFI ─────────────────────────────────────────────────────────────────
// Host functions provided by the workforce WASM runtime.
// Users never call these directly — use the `vault`, `http`, `log`, and
// `config` modules instead.

#[cfg(target_arch = "wasm32")]
#[allow(dead_code)]
extern "C" {
    fn wf_log(level: i32, msg_ptr: i32, msg_len: i32);
    fn wf_set_response(ptr: i32, len: i32);
    fn wf_get_host_response_len() -> i32;
    fn wf_get_host_response(buf_ptr: i32, buf_len: i32) -> i32;
    fn wf_generate_uuid() -> i32;
    fn wf_current_time() -> i32;
    fn wf_vault_get(key_ptr: i32, key_len: i32) -> i32;
    fn wf_vault_set(key_ptr: i32, key_len: i32, val_ptr: i32, val_len: i32) -> i32;
    fn wf_config_get(key_ptr: i32, key_len: i32) -> i32;
    fn wf_http_fetch(
        method_ptr: i32,
        method_len: i32,
        url_ptr: i32,
        url_len: i32,
        headers_ptr: i32,
        headers_len: i32,
        body_ptr: i32,
        body_len: i32,
    ) -> i32;
    fn wf_read_artifact(id_ptr: i32, id_len: i32) -> i32;
    fn wf_stage_artifact(payload_ptr: i32, payload_len: i32) -> i32;
    fn wf_render_chart(payload_ptr: i32, payload_len: i32) -> i32;
    fn wf_tool_invoke(name_ptr: i32, name_len: i32, args_ptr: i32, args_len: i32) -> i32;
    fn wf_tool_invoke_many(payload_ptr: i32, payload_len: i32) -> i32;
    fn wf_store_call(req_ptr: i32, req_len: i32) -> i32;
}

// ─── Common host response reader ────────────────────────────────────────────

#[cfg(target_arch = "wasm32")]
fn read_host_response_string() -> Option<String> {
    let len = unsafe { wf_get_host_response_len() };
    if len <= 0 {
        return None;
    }
    let mut buf = vec![0u8; len as usize];
    let read = unsafe { wf_get_host_response(buf.as_mut_ptr() as i32, len) };
    if read <= 0 {
        return None;
    }
    buf.truncate(read as usize);
    String::from_utf8(buf).ok()
}

// ─── ToolInput ──────────────────────────────────────────────────────────────

/// Input parameters received by a tool handler.
///
/// Wraps the JSON input from the agent with convenience accessors.
#[derive(Debug, Clone)]
pub struct ToolInput {
    /// Raw JSON input from the agent.
    pub data: Value,
    /// The tool name being invoked.
    pub tool_name: String,
    /// The agent ID making the request.
    pub agent_id: String,
    /// The user ID making the request, if known. `None` for system-
    /// initiated work that didn't opt into a synthetic identity. Skills
    /// that read per-user credentials via `vault::get` don't need this —
    /// the host applies user scoping automatically — but it's exposed
    /// here for skills that want to surface "I am acting on behalf of X"
    /// in their tool output.
    pub user_id: Option<String>,
}

impl ToolInput {
    /// Parse a `ToolInput` from the request JSON the runtime provides.
    pub fn from_json(json: &str) -> Option<Self> {
        let v: Value = serde_json::from_str(json).ok()?;
        Some(Self {
            data: v["input"].clone(),
            tool_name: v["tool_name"].as_str().unwrap_or("").to_string(),
            agent_id: v["agent_id"].as_str().unwrap_or("").to_string(),
            user_id: v["user_id"].as_str().map(|s| s.to_string()),
        })
    }

    /// Get a string parameter by key.
    pub fn get_str(&self, key: &str) -> Option<&str> {
        self.data.get(key).and_then(|v| v.as_str())
    }

    /// Get an integer parameter by key.
    pub fn get_i64(&self, key: &str) -> Option<i64> {
        self.data.get(key).and_then(|v| v.as_i64())
    }

    /// Get a float parameter by key.
    pub fn get_f64(&self, key: &str) -> Option<f64> {
        self.data.get(key).and_then(|v| v.as_f64())
    }

    /// Get a boolean parameter by key.
    pub fn get_bool(&self, key: &str) -> Option<bool> {
        self.data.get(key).and_then(|v| v.as_bool())
    }

    /// Get a nested JSON value by key.
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.data.get(key)
    }

    /// Get the raw input as a reference.
    pub fn raw(&self) -> &Value {
        &self.data
    }
}

// ─── ToolOutput ─────────────────────────────────────────────────────────────

/// Output from a tool handler.
///
/// Serialised to JSON and returned to the workforce runtime, which passes
/// it back to the agent as a `ToolResult`.
#[derive(Debug, Clone)]
pub struct ToolOutput {
    success: bool,
    result: Value,
    error: Option<String>,
}

impl ToolOutput {
    /// Create a successful output with the given result data.
    pub fn success(result: Value) -> Self {
        Self {
            success: true,
            result,
            error: None,
        }
    }

    /// Create a failure output with an error message.
    pub fn error(message: &str) -> Self {
        Self {
            success: false,
            result: Value::Null,
            error: Some(message.to_string()),
        }
    }

    /// Serialise to the JSON format the runtime expects.
    pub fn into_json(self) -> String {
        let obj = serde_json::json!({
            "success": self.success,
            "result": self.result,
            "error": self.error,
        });
        serde_json::to_string(&obj)
            .unwrap_or_else(|_| r#"{"success":false,"error":"serialisation failed"}"#.to_string())
    }
}

// ─── vault module ───────────────────────────────────────────────────────────

/// Read and write secrets in the workforce vault.
///
/// All paths are automatically scoped to `wasm-skills/{skill_name}/` — you
/// only need to provide the key name relative to your skill.
///
/// Requires the `vault` capability in your `Skill.toml`.
pub mod vault {
    #[allow(unused_imports)]
    use super::*;

    /// Get a secret by key. Returns `None` if the key doesn't exist.
    ///
    /// ```rust,ignore
    /// let api_key = vault::get("api_key");
    /// ```
    pub fn get(key: &str) -> Option<String> {
        #[cfg(target_arch = "wasm32")]
        {
            let bytes = key.as_bytes();
            let result = unsafe { wf_vault_get(bytes.as_ptr() as i32, bytes.len() as i32) };
            if result < 0 {
                return None;
            }
            read_host_response_string()
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            let _ = key;
            None
        }
    }

    /// Set a secret. Returns `true` on success.
    ///
    /// ```rust,ignore
    /// vault::set("api_key", "sk-abc123");
    /// ```
    pub fn set(key: &str, value: &str) -> bool {
        #[cfg(target_arch = "wasm32")]
        {
            let key_bytes = key.as_bytes();
            let val_bytes = value.as_bytes();
            let result = unsafe {
                wf_vault_set(
                    key_bytes.as_ptr() as i32,
                    key_bytes.len() as i32,
                    val_bytes.as_ptr() as i32,
                    val_bytes.len() as i32,
                )
            };
            result == 0
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            let _ = (key, value);
            true
        }
    }
}

// ─── store module ───────────────────────────────────────────────────────────

/// The hub's document store: scoped JSON persistence with versions and
/// claims. Needs `[capabilities] store = true` in Skill.toml.
///
/// Documents live in a scope — `agent` (every run of the calling agent; the
/// default), `user` (private to the calling user) or `global` (read by
/// everyone, written by admins) — under `collection/doc_id`. Every call
/// takes the same JSON the `store.*` platform tools take.
///
/// ```rust,ignore
/// store::set("pipeline", "ENG-107", &json!({"stage": "review"}), None)?;
/// let doc = store::get("pipeline", "ENG-107")?;           // Option<Value>
/// let page = store::query("pipeline", &json!({"where": [["stage", "eq", "review"]]}))?;
/// store::claim("pipeline", "ENG-107", "run-abc", 600)?;   // Err(claimed …) when held
/// ```
pub mod store {
    use serde_json::{json, Value};

    /// The reply to a failed store call, as the host sends it.
    #[derive(Debug, Clone, PartialEq)]
    pub struct StoreFailure {
        /// `not_found`, `version_conflict`, `claimed`, `denied`,
        /// `no_identity`, `invalid`, `backend`.
        pub code: String,
        pub error: String,
        /// `expected`/`actual` for a version conflict, `owner`/`until` for
        /// a claim — whatever the host attached.
        pub details: Value,
    }

    impl std::fmt::Display for StoreFailure {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "{} ({})", self.error, self.code)
        }
    }

    /// One raw call: `op` is `get`, `set`, `update`, `delete`, `list`,
    /// `query`, `batch`, `claim` or `release`; `args` are that op's fields
    /// (`scope`, `collection`, `doc_id`, `body`, `if_version`, `ttl_secs`,
    /// `where`, `order_by`, `direction`, `limit`, `cursor`, `writes`,
    /// `owner`). `Ok` is the op's output; `Err` a store failure or a
    /// runtime refusal (missing capability, policy).
    pub fn call(op: &str, args: &Value) -> Result<Value, StoreFailure> {
        #[cfg(target_arch = "wasm32")]
        {
            let mut request = args.as_object().cloned().unwrap_or_default();
            request.insert("op".to_string(), Value::String(op.to_string()));
            let payload = Value::Object(request).to_string();
            let bytes = payload.as_bytes();
            let code = unsafe { super::wf_store_call(bytes.as_ptr() as i32, bytes.len() as i32) };
            let runtime = |code: &str, error: &str| StoreFailure {
                code: code.to_string(),
                error: error.to_string(),
                details: Value::Null,
            };
            match code {
                0 => {
                    let reply: Value = super::read_host_response_string()
                        .and_then(|s| serde_json::from_str(&s).ok())
                        .ok_or_else(|| runtime("backend", "store reply unavailable"))?;
                    if reply.get("ok") == Some(&Value::Bool(false)) {
                        let mut details = reply.clone();
                        if let Some(m) = details.as_object_mut() {
                            m.remove("ok");
                            m.remove("code");
                            m.remove("error");
                        }
                        return Err(StoreFailure {
                            code: reply["code"].as_str().unwrap_or("backend").to_string(),
                            error: reply["error"]
                                .as_str()
                                .unwrap_or("store call failed")
                                .to_string(),
                            details,
                        });
                    }
                    Ok(reply)
                }
                -2 => Err(runtime(
                    "capability",
                    "the `store` capability is not declared in Skill.toml",
                )),
                -4 => Err(runtime(
                    "denied",
                    "denied by platform policy — the runtime attaches the remedy",
                )),
                _ => Err(runtime("backend", "store call failed")),
            }
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            let _ = (op, args);
            Err(StoreFailure {
                code: "backend".into(),
                error: "not running in the WASM runtime".into(),
                details: Value::Null,
            })
        }
    }

    fn with_scope(mut args: Value, scope: Option<&str>) -> Value {
        if let (Some(s), Some(m)) = (scope, args.as_object_mut()) {
            m.insert("scope".into(), Value::String(s.into()));
        }
        args
    }

    /// Read one document (`None` when absent). `scope` defaults to `agent`.
    pub fn get_in(
        scope: Option<&str>,
        collection: &str,
        doc_id: &str,
    ) -> Result<Option<Value>, StoreFailure> {
        let reply = call(
            "get",
            &with_scope(json!({"collection": collection, "doc_id": doc_id}), scope),
        )?;
        Ok(if reply["found"] == Value::Bool(true) {
            Some(reply["document"].clone())
        } else {
            None
        })
    }

    pub fn get(collection: &str, doc_id: &str) -> Result<Option<Value>, StoreFailure> {
        get_in(None, collection, doc_id)
    }

    /// Create or replace. `if_version`: `Some(0)` = create only; `Some(n)` =
    /// only if still at n. Returns the stored document.
    pub fn set_in(
        scope: Option<&str>,
        collection: &str,
        doc_id: &str,
        body: &Value,
        if_version: Option<i64>,
    ) -> Result<Value, StoreFailure> {
        let mut args = json!({"collection": collection, "doc_id": doc_id, "body": body});
        if let Some(v) = if_version {
            args["if_version"] = json!(v);
        }
        Ok(call("set", &with_scope(args, scope))?["document"].clone())
    }

    pub fn set(
        collection: &str,
        doc_id: &str,
        body: &Value,
        if_version: Option<i64>,
    ) -> Result<Value, StoreFailure> {
        set_in(None, collection, doc_id, body, if_version)
    }

    /// Merge top-level fields into an existing document (`null` removes).
    pub fn update_in(
        scope: Option<&str>,
        collection: &str,
        doc_id: &str,
        patch: &Value,
        if_version: Option<i64>,
    ) -> Result<Value, StoreFailure> {
        let mut args = json!({"collection": collection, "doc_id": doc_id, "body": patch});
        if let Some(v) = if_version {
            args["if_version"] = json!(v);
        }
        Ok(call("update", &with_scope(args, scope))?["document"].clone())
    }

    pub fn update(
        collection: &str,
        doc_id: &str,
        patch: &Value,
        if_version: Option<i64>,
    ) -> Result<Value, StoreFailure> {
        update_in(None, collection, doc_id, patch, if_version)
    }

    /// Delete; `Ok(false)` when there was nothing to delete.
    pub fn delete_in(
        scope: Option<&str>,
        collection: &str,
        doc_id: &str,
    ) -> Result<bool, StoreFailure> {
        Ok(call(
            "delete",
            &with_scope(json!({"collection": collection, "doc_id": doc_id}), scope),
        )?["existed"]
            == Value::Bool(true))
    }

    pub fn delete(collection: &str, doc_id: &str) -> Result<bool, StoreFailure> {
        delete_in(None, collection, doc_id)
    }

    /// Query a collection. `query` is `{"where": [[field, op, value], …],
    /// "order_by", "direction", "limit", "cursor"}`; the reply carries
    /// `documents` and `next_cursor`.
    pub fn query_in(
        scope: Option<&str>,
        collection: &str,
        query: &Value,
    ) -> Result<Value, StoreFailure> {
        let mut args = query.clone();
        if let Some(m) = args.as_object_mut() {
            m.insert("collection".into(), Value::String(collection.into()));
        }
        call("query", &with_scope(args, scope))
    }

    pub fn query(collection: &str, query: &Value) -> Result<Value, StoreFailure> {
        query_in(None, collection, query)
    }

    /// Apply up to 50 writes atomically: `[{"op": "set"|"update"|"delete",
    /// "collection", "doc_id", "body"?, "if_version"?}, …]`.
    pub fn batch_in(scope: Option<&str>, writes: &[Value]) -> Result<Value, StoreFailure> {
        call("batch", &with_scope(json!({"writes": writes}), scope))
    }

    pub fn batch(writes: &[Value]) -> Result<Value, StoreFailure> {
        batch_in(None, writes)
    }

    /// Take a document for `ttl_secs` as `owner`; `Err(claimed …)` names
    /// the holder when someone else has it. Claiming again renews.
    pub fn claim_in(
        scope: Option<&str>,
        collection: &str,
        doc_id: &str,
        owner: &str,
        ttl_secs: i64,
    ) -> Result<Value, StoreFailure> {
        call(
            "claim",
            &with_scope(
                json!({"collection": collection, "doc_id": doc_id, "owner": owner, "ttl_secs": ttl_secs}),
                scope,
            ),
        )
    }

    pub fn claim(
        collection: &str,
        doc_id: &str,
        owner: &str,
        ttl_secs: i64,
    ) -> Result<Value, StoreFailure> {
        claim_in(None, collection, doc_id, owner, ttl_secs)
    }

    /// Give a claim back; `Ok(false)` when `owner` did not hold it.
    pub fn release_in(
        scope: Option<&str>,
        collection: &str,
        doc_id: &str,
        owner: &str,
    ) -> Result<bool, StoreFailure> {
        Ok(call(
            "release",
            &with_scope(
                json!({"collection": collection, "doc_id": doc_id, "owner": owner}),
                scope,
            ),
        )?["released"]
            == Value::Bool(true))
    }

    pub fn release(collection: &str, doc_id: &str, owner: &str) -> Result<bool, StoreFailure> {
        release_in(None, collection, doc_id, owner)
    }
}

// ─── config module ──────────────────────────────────────────────────────────

/// Read skill-scoped configuration values.
///
/// Config values are set when the skill is installed/configured. They are
/// non-secret settings declared in the `[[config]]` section of `Skill.toml`.
///
/// Requires the `config` capability in your `Skill.toml`.
pub mod config {
    #[allow(unused_imports)]
    use super::*;

    /// Get a config value by key. Returns `None` if the key doesn't exist.
    ///
    /// ```rust,ignore
    /// let units = config::get("units").unwrap_or_else(|| "metric".to_string());
    /// ```
    pub fn get(key: &str) -> Option<String> {
        #[cfg(target_arch = "wasm32")]
        {
            let bytes = key.as_bytes();
            let result = unsafe { wf_config_get(bytes.as_ptr() as i32, bytes.len() as i32) };
            if result < 0 {
                return None;
            }
            read_host_response_string()
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            let _ = key;
            None
        }
    }
}

// ─── log module ─────────────────────────────────────────────────────────────

/// Structured logging from inside your WASM skill.
///
/// Messages appear in the workforce hub's log output prefixed with `[wasm-skill]`.
pub mod log {
    #[allow(unused_imports)]
    use super::*;

    /// Log an error message (level 0).
    pub fn error(msg: &str) {
        write(0, msg);
    }

    /// Log a warning message (level 1).
    pub fn warn(msg: &str) {
        write(1, msg);
    }

    /// Log an info message (level 2).
    pub fn info(msg: &str) {
        write(2, msg);
    }

    /// Log a debug message (level 3).
    pub fn debug(msg: &str) {
        write(3, msg);
    }

    fn write(level: i32, msg: &str) {
        #[cfg(target_arch = "wasm32")]
        {
            let bytes = msg.as_bytes();
            unsafe {
                super::wf_log(level, bytes.as_ptr() as i32, bytes.len() as i32);
            }
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            let _ = (level, msg);
        }
    }
}

// ─── tools module ───────────────────────────────────────────────────────────

/// Invoke platform tools declared in this skill's `[[permissions]]`.
///
/// Requires the `tools` capability in your `Skill.toml`.
pub mod tools {
    #[allow(unused_imports)]
    use super::*;

    /// A platform tool call with the hub's envelope already unwrapped: the
    /// tool's `output`, or `"<tool> failed: <error>"`. [`call`] on the hub;
    /// a closure in a skill's own tests, so they see every call it makes.
    pub type Call<'a> = &'a dyn Fn(&str, &Value) -> Result<Value, String>;

    /// [`invoke`] with the envelope unwrapped: `Ok(output)` when the tool
    /// succeeded, `Err("<tool> failed: <error>")` when it did not, and
    /// [`invoke`]'s own `Err` when the call never ran. Use [`invoke`] when
    /// the envelope's extras (`rate_limited`) matter.
    ///
    /// ```rust,ignore
    /// let issue = tools::call("jira.get_issue", &json!({"key": "ENG-1"}))?;
    /// ```
    pub fn call(name: &str, args: &Value) -> Result<Value, String> {
        unwrap_envelope(name, invoke(name, args)?)
    }

    fn unwrap_envelope(name: &str, mut envelope: Value) -> Result<Value, String> {
        if envelope["success"].as_bool() == Some(true) {
            Ok(envelope["output"].take())
        } else {
            Err(format!(
                "{name} failed: {}",
                envelope["error"].as_str().unwrap_or("unknown error")
            ))
        }
    }

    /// Invoke a platform tool declared in this skill's `[[permissions]]`.
    ///
    /// The call runs through the platform's governed dispatch — the user's
    /// tools grant (`cmd: skill grant <skill> tools`), the capability gate
    /// under the calling user, and the policy engine — so it can be refused
    /// even when declared. On refusal the runtime attaches the full remedy to
    /// this tool call's result for the agent; the `Err` here is a short
    /// category for the guest's own control flow. Rate-limited calls (429)
    /// are retried host-side with backoff before the envelope comes back.
    ///
    /// `Ok` is the platform envelope: `{"success": bool, "output": …,
    /// "error": …}` — check `success`, a tool-level failure is data, not a
    /// policy denial. A failure the upstream refused for rate limiting also
    /// carries `"rate_limited": {"retry_after_secs": N|null}` after host-side
    /// retries were exhausted — branch on that key, never on error prose.
    ///
    /// An `output` over the runtime's inline limit (512 KiB) is offloaded to
    /// the artifact store host-side and read back here before the envelope
    /// is returned, so size never changes the shape a skill sees; a body up
    /// to 12 MiB is supported.
    ///
    /// ```rust,ignore
    /// let result = tools::invoke("slack.post_message", &json!({
    ///     "channel": "C123", "text": "done"
    /// }))?;
    /// ```
    pub fn invoke(name: &str, args: &Value) -> Result<Value, String> {
        #[cfg(target_arch = "wasm32")]
        {
            let name_bytes = name.as_bytes();
            let args_json = args.to_string();
            let args_bytes = args_json.as_bytes();
            let code = unsafe {
                wf_tool_invoke(
                    name_bytes.as_ptr() as i32,
                    name_bytes.len() as i32,
                    args_bytes.as_ptr() as i32,
                    args_bytes.len() as i32,
                )
            };
            match code {
                0 => read_host_response_string()
                    .and_then(|s| serde_json::from_str(&s).ok())
                    .map(resolve_spilled)
                    .ok_or_else(|| "tool result unavailable".to_string()),
                -2 => Err("the `tools` capability is not declared in Skill.toml".to_string()),
                -4 => Err(
                    "denied by platform policy (declaration, grant, capability, or rule) — \
                     surface this failure; the runtime attaches the remedy for the agent"
                        .to_string(),
                ),
                -5 => Err("tool-call budget exhausted for this invocation".to_string()),
                _ => Err("tool invocation failed".to_string()),
            }
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            let _ = (name, args);
            Err("not running in the WASM runtime".to_string())
        }
    }

    /// Invoke several declared platform tools in one host call, dispatched
    /// host-side with bounded per-service concurrency (calls to different
    /// services run concurrently; calls to the same service are limited to a
    /// few in flight so a burst can't trip its rate window) — guests have no
    /// threads, so this is the way to fan out (e.g. one query per service)
    /// instead of a serial loop.
    ///
    /// Each element passes the same governance as [`invoke`] and consumes
    /// one slot of the per-invocation tool-call budget. `Ok` preserves
    /// order: one platform envelope (`{"success", "output", "error"}`) per
    /// request — a per-element denial or failure arrives as an envelope
    /// with `success: false`, so a partial batch still returns the
    /// successes. `Err` is batch-level only (capability, grant, budget).
    ///
    /// ```rust,ignore
    /// let results = tools::invoke_many(&[
    ///     ("datadog.logs_search", json!({"query": "service:a status:error"})),
    ///     ("datadog.logs_search", json!({"query": "service:b status:error"})),
    /// ])?;
    /// ```
    pub fn invoke_many(calls: &[(&str, Value)]) -> Result<Vec<Value>, String> {
        #[cfg(target_arch = "wasm32")]
        {
            let payload = Value::Array(
                calls
                    .iter()
                    .map(|(name, args)| serde_json::json!({"tool": name, "args": args}))
                    .collect(),
            )
            .to_string();
            let bytes = payload.as_bytes();
            let code = unsafe { wf_tool_invoke_many(bytes.as_ptr() as i32, bytes.len() as i32) };
            match code {
                0 => read_host_response_string()
                    .and_then(|s| serde_json::from_str::<Vec<Value>>(&s).ok())
                    .map(|envelopes| envelopes.into_iter().map(resolve_spilled).collect())
                    .ok_or_else(|| "tool results unavailable".to_string()),
                -2 => Err("the `tools` capability is not declared in Skill.toml".to_string()),
                -4 => Err(
                    "denied by platform policy (grant, marker ban, or rule) — surface \
                     this failure; the runtime attaches the remedy for the agent"
                        .to_string(),
                ),
                -5 => Err("tool-call budget exhausted for this invocation".to_string()),
                _ => Err("tool invocation failed".to_string()),
            }
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            let _ = calls;
            Err("not running in the WASM runtime".to_string())
        }
    }

    /// `kind` the runtime stamps on an `output` it offloaded to the artifact
    /// store because it was over the inline limit.
    #[cfg(any(target_arch = "wasm32", test))]
    const SPILLED_RESULT_KIND: &str = "skill-tool-result";

    /// The envelope with an offloaded `output` read back in place, so a skill
    /// sees the same shape whatever the size. An artifact that cannot be read
    /// (expired, out of scope, not JSON) becomes a tool-level failure that
    /// names it.
    #[cfg(any(target_arch = "wasm32", test))]
    fn resolve_spilled_with(mut envelope: Value, read: impl Fn(&str) -> Option<Vec<u8>>) -> Value {
        let Some(id) = spilled_artifact_id(&envelope) else {
            return envelope;
        };
        match read(&id)
            .ok_or("not readable")
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).map_err(|_| "not JSON"))
        {
            Ok(output) => {
                envelope["output"] = output;
                envelope
            }
            Err(cause) => serde_json::json!({
                "success": false,
                "output": Value::Null,
                "error": format!("spilled result {id} could not be read: {cause}"),
            }),
        }
    }

    #[cfg(any(target_arch = "wasm32", test))]
    fn spilled_artifact_id(envelope: &Value) -> Option<String> {
        let marker = envelope.get("output")?;
        if marker.get("kind")?.as_str()? != SPILLED_RESULT_KIND {
            return None;
        }
        marker.get("artifact_id")?.as_str().map(str::to_string)
    }

    #[cfg(target_arch = "wasm32")]
    fn resolve_spilled(envelope: Value) -> Value {
        resolve_spilled_with(envelope, |id| artifact::read(id).map(|a| a.bytes))
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn marker(id: &str) -> Value {
            serde_json::json!({
                "success": true, "error": null,
                "output": { "kind": SPILLED_RESULT_KIND, "artifact_id": id, "bytes": 9, "mime": "application/json" }
            })
        }

        #[test]
        fn a_call_is_the_output_or_the_named_failure() {
            let ok = serde_json::json!({ "success": true, "output": { "id": 7 }, "error": null });
            assert_eq!(
                unwrap_envelope("jira.get_issue", ok),
                Ok(serde_json::json!({ "id": 7 }))
            );
            let failed =
                serde_json::json!({ "success": false, "output": null, "error": "no such issue" });
            assert_eq!(
                unwrap_envelope("jira.get_issue", failed),
                Err("jira.get_issue failed: no such issue".to_string())
            );
            assert_eq!(
                unwrap_envelope("jira.get_issue", serde_json::json!({})),
                Err("jira.get_issue failed: unknown error".to_string())
            );
        }

        #[test]
        fn an_ordinary_envelope_passes_through() {
            let plain = serde_json::json!({ "success": true, "output": { "kind": "other", "artifact_id": "x" }, "error": null });
            assert_eq!(
                resolve_spilled_with(plain.clone(), |_| panic!("no read")),
                plain
            );
            let failure = serde_json::json!({ "success": false, "output": null, "error": "e" });
            assert_eq!(
                resolve_spilled_with(failure.clone(), |_| panic!("no read")),
                failure
            );
        }

        #[test]
        fn a_spilled_output_is_read_back_in_place() {
            let resolved = resolve_spilled_with(marker("art_1"), |id| {
                assert_eq!(id, "art_1");
                Some(br#"{"issues": [1, 2, 3]}"#.to_vec())
            });
            assert_eq!(resolved["success"], true);
            assert_eq!(resolved["output"]["issues"], serde_json::json!([1, 2, 3]));
        }

        #[test]
        fn an_unreadable_or_malformed_body_is_a_named_failure() {
            let missing = resolve_spilled_with(marker("art_2"), |_| None);
            assert_eq!(missing["success"], false);
            assert_eq!(
                missing["error"],
                "spilled result art_2 could not be read: not readable"
            );
            let garbage = resolve_spilled_with(marker("art_3"), |_| Some(b"not json".to_vec()));
            assert_eq!(
                garbage["error"],
                "spilled result art_3 could not be read: not JSON"
            );
            assert!(garbage["output"].is_null());
        }
    }
}

// ─── pipeline module ────────────────────────────────────────────────────────

/// A pipeline a skill ships and records against. The definition JSON lives
/// in the skill's repository: it is registered on the hub the first time a
/// tool runs and again whenever the file's `revision` is newer than the
/// stored one, so a hand edit on the hub is never undone by an unchanged
/// skill and a skill upgrade carries its changes. Every move an item makes
/// is one `pipeline.record`. Needs `pipeline.get`, `pipeline.define` and
/// `pipeline.record` in the skill's `[[permissions]]`.
///
/// Every call takes the tool call to use — `&tools::call` on the hub, a
/// closure in tests — so a skill's own tests see what it records.
///
/// ```rust,ignore
/// const PIPELINE: pipeline::Shipped =
///     pipeline::Shipped::new("fieldops", include_str!("../../../pipelines/fieldops.json"))
///         .with_guide(include_str!("../../../pipelines/fieldops.guide.md"));
///
/// PIPELINE.ensure(&tools::call)?;                  // Ensured::UpToDate | Ensured::Defined
/// let tracked = PIPELINE.record(&tools::call, "ENG-107", "implement", "claimed", json!({"url": url}), run);
/// // "recorded → implementing", "recorded, unplaced: no rule for …" or "skipped: …"
/// ```
pub mod pipeline {
    use super::log;
    use super::tools::Call;
    use serde_json::{json, Value};

    /// The definition a skill ships, as text, with the pipeline's id and
    /// the guide shown on its board.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct Shipped {
        pub id: &'static str,
        pub definition: &'static str,
        pub guide: Option<&'static str>,
    }

    /// What [`Shipped::ensure`] found on the hub.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum Ensured {
        /// The stored definition is at the shipped revision or newer.
        UpToDate { version: i64 },
        /// The shipped definition was registered, replacing an older one
        /// or none.
        Defined { revision: u64, warnings: usize },
    }

    impl std::fmt::Display for Ensured {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                Self::UpToDate { version } => write!(f, "up to date (v{version})"),
                Self::Defined { revision, warnings } => {
                    write!(f, "defined at revision {revision} ({warnings} warnings)")
                }
            }
        }
    }

    impl Shipped {
        pub const fn new(id: &'static str, definition: &'static str) -> Self {
            Self {
                id,
                definition,
                guide: None,
            }
        }

        /// The board's "How it works" text, in markdown, registered as the
        /// definition's `guide`.
        pub const fn with_guide(self, guide: &'static str) -> Self {
            Self {
                guide: Some(guide),
                ..self
            }
        }

        /// The shipped file's `revision`; 0 when it has none or is not JSON.
        pub fn revision(&self) -> u64 {
            serde_json::from_str::<Value>(self.definition)
                .ok()
                .and_then(|d| d["revision"].as_u64())
                .unwrap_or(0)
        }

        fn document(&self) -> Result<Value, String> {
            let mut definition: Value = serde_json::from_str(self.definition)
                .map_err(|e| format!("pipeline {} definition is not JSON: {e}", self.id))?;
            if let Some(guide) = self.guide {
                definition["guide"] = json!(guide);
            }
            Ok(definition)
        }

        /// Make sure the hub holds the definition at least at the shipped
        /// revision: one `pipeline.get`, and a `pipeline.define` only when
        /// nothing is stored or the stored revision is older.
        pub fn ensure(&self, call: Call) -> Result<Ensured, String> {
            let (stored, version) = match call("pipeline.get", &json!({ "pipeline": self.id })) {
                Ok(out) => (
                    Some(out["definition"].clone()),
                    out["version"].as_i64().unwrap_or(0),
                ),
                Err(e) if e.contains("not_found") => (None, 0),
                Err(e) => return Err(e),
            };
            let revision = self.revision();
            if !should_define(stored.as_ref(), revision) {
                return Ok(Ensured::UpToDate { version });
            }
            let defined = call(
                "pipeline.define",
                &json!({ "pipeline": self.id, "definition": self.document()?, "if_version": version }),
            )?;
            let warnings = defined["warnings"].as_array().map_or(0, Vec::len);
            log::info(&format!(
                "pipeline {} defined at revision {revision} ({warnings} warnings)",
                self.id
            ));
            Ok(Ensured::Defined { revision, warnings })
        }

        /// [`ensure`](Self::ensure), then [`record`]. A define that fails —
        /// a hub without pipelines, or one lost to a parallel run that
        /// defined first — is logged, never a reason to skip the record:
        /// the pipeline exists either way, or the record says it does not.
        pub fn record(
            &self,
            call: Call,
            item: &str,
            stage: &str,
            event: &str,
            detail: Value,
            run: Option<&str>,
        ) -> String {
            if let Err(e) = self.ensure(call) {
                log::warn(&format!("pipeline {} not (re)defined: {e}", self.id));
            }
            record(call, self.id, item, stage, event, detail, run)
        }
    }

    /// Record that `item` moved, and say what happened: `recorded → <column>`,
    /// `recorded, unplaced: <warning>` when the definition has no rule for
    /// the event, or `skipped: <error>`. Best-effort by design: a pipeline
    /// is additive to the system of record, so a failure is logged and named
    /// in the tool's output, never a failed step. `run` is the workflow
    /// instance making the move (`_instance_id` in a tool step's input).
    pub fn record(
        call: Call,
        pipeline: &str,
        item: &str,
        stage: &str,
        event: &str,
        detail: Value,
        run: Option<&str>,
    ) -> String {
        let mut args = json!({
            "pipeline": pipeline, "item": item, "stage": stage, "event": event, "detail": detail,
        });
        if let Some(run) = run {
            args["run"] = json!(run);
        }
        match call("pipeline.record", &args) {
            Ok(out) => tracked(&out),
            Err(e) => {
                log::warn(&format!(
                    "{item}: not recorded on pipeline {pipeline} ({stage}/{event}): {e}"
                ));
                format!("skipped: {e}")
            }
        }
    }

    fn should_define(stored: Option<&Value>, shipped: u64) -> bool {
        stored.is_none_or(|def| def["revision"].as_u64().unwrap_or(0) < shipped)
    }

    fn tracked(out: &Value) -> String {
        match (out["column"].as_str(), out["warning"].as_str()) {
            (Some(column), _) => format!("recorded → {column}"),
            (None, Some(warning)) => format!("recorded, unplaced: {warning}"),
            (None, None) => "recorded".to_string(),
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::cell::RefCell;

        const SHIPPED: Shipped = Shipped::new(
            "onboarding",
            r#"{"name": "Onboarding", "revision": 3, "lanes": [{"id": "accounts", "label": "Accounts"}]}"#,
        )
        .with_guide("## How it works\n\nLeft to right.");

        struct Hub {
            replies: Vec<(&'static str, Result<Value, String>)>,
            calls: RefCell<Vec<(String, Value)>>,
        }

        impl Hub {
            fn new(replies: Vec<(&'static str, Result<Value, String>)>) -> Self {
                Self {
                    replies,
                    calls: RefCell::new(Vec::new()),
                }
            }

            fn call(&self, tool: &str, args: &Value) -> Result<Value, String> {
                self.calls
                    .borrow_mut()
                    .push((tool.to_string(), args.clone()));
                self.replies
                    .iter()
                    .find(|(name, _)| *name == tool)
                    .map(|(_, reply)| reply.clone())
                    .unwrap_or_else(|| Err(format!("unexpected {tool}")))
            }

            fn tools(&self) -> Vec<String> {
                self.calls.borrow().iter().map(|(t, _)| t.clone()).collect()
            }

            fn args(&self, index: usize) -> Value {
                self.calls.borrow()[index].1.clone()
            }
        }

        #[test]
        fn the_shipped_revision_is_read_from_the_file() {
            assert_eq!(SHIPPED.revision(), 3);
            assert_eq!(Shipped::new("x", "{}").revision(), 0);
            assert_eq!(Shipped::new("x", "not json").revision(), 0);
        }

        #[test]
        fn a_stored_definition_is_replaced_only_by_a_newer_revision() {
            assert!(should_define(None, 1));
            assert!(should_define(Some(&json!({ "revision": 0 })), 1));
            assert!(should_define(Some(&json!({})), 1));
            assert!(!should_define(Some(&json!({ "revision": 1 })), 1));
            assert!(!should_define(Some(&json!({ "revision": 5 })), 1));
        }

        #[test]
        fn a_missing_pipeline_is_defined_with_the_guide_and_create_only() {
            let hub = Hub::new(vec![
                ("pipeline.get", Err("pipeline.get failed: not_found".into())),
                (
                    "pipeline.define",
                    Ok(json!({ "version": 1, "warnings": ["w"] })),
                ),
            ]);
            let ensured = SHIPPED.ensure(&|t, a| hub.call(t, a)).unwrap();
            assert_eq!(
                ensured,
                Ensured::Defined {
                    revision: 3,
                    warnings: 1
                }
            );
            assert_eq!(ensured.to_string(), "defined at revision 3 (1 warnings)");
            assert_eq!(hub.tools(), ["pipeline.get", "pipeline.define"]);
            let define = hub.args(1);
            assert_eq!(define["pipeline"], "onboarding");
            assert_eq!(define["if_version"], 0);
            assert_eq!(define["definition"]["revision"], 3);
            assert_eq!(
                define["definition"]["guide"],
                "## How it works\n\nLeft to right."
            );
        }

        #[test]
        fn an_older_stored_definition_is_replaced_at_its_version() {
            let hub = Hub::new(vec![
                (
                    "pipeline.get",
                    Ok(json!({ "definition": { "revision": 2 }, "version": 7 })),
                ),
                (
                    "pipeline.define",
                    Ok(json!({ "version": 8, "warnings": [] })),
                ),
            ]);
            let ensured = SHIPPED.ensure(&|t, a| hub.call(t, a)).unwrap();
            assert_eq!(
                ensured,
                Ensured::Defined {
                    revision: 3,
                    warnings: 0
                }
            );
            assert_eq!(hub.args(1)["if_version"], 7);
            let without_guide = Shipped::new(SHIPPED.id, SHIPPED.definition);
            let hub = Hub::new(vec![
                ("pipeline.get", Err("not_found".into())),
                ("pipeline.define", Ok(json!({ "warnings": [] }))),
            ]);
            without_guide.ensure(&|t, a| hub.call(t, a)).unwrap();
            assert!(hub.args(1)["definition"].get("guide").is_none());
        }

        #[test]
        fn a_current_or_newer_stored_definition_is_left_alone() {
            for stored in [3, 9] {
                let hub = Hub::new(vec![(
                    "pipeline.get",
                    Ok(json!({ "definition": { "revision": stored }, "version": 4 })),
                )]);
                let ensured = SHIPPED.ensure(&|t, a| hub.call(t, a)).unwrap();
                assert_eq!(ensured, Ensured::UpToDate { version: 4 });
                assert_eq!(ensured.to_string(), "up to date (v4)");
                assert_eq!(hub.tools(), ["pipeline.get"]);
            }
        }

        #[test]
        fn a_hub_without_pipelines_or_a_bad_file_is_an_error() {
            let hub = Hub::new(vec![(
                "pipeline.get",
                Err("pipeline.get failed: denied".into()),
            )]);
            assert_eq!(
                SHIPPED.ensure(&|t, a| hub.call(t, a)),
                Err("pipeline.get failed: denied".to_string())
            );
            let hub = Hub::new(vec![("pipeline.get", Err("not_found".into()))]);
            assert_eq!(
                Shipped::new("x", "not json").ensure(&|t, a| hub.call(t, a)),
                Err(
                    "pipeline x definition is not JSON: expected ident at line 1 column 2"
                        .to_string()
                )
            );
            assert_eq!(hub.tools(), ["pipeline.get"]);
        }

        #[test]
        fn recording_reports_the_column_a_missing_rule_or_a_failure() {
            assert_eq!(tracked(&json!({ "column": "noted" })), "recorded → noted");
            assert_eq!(
                tracked(&json!({ "warning": "no rule for x/y" })),
                "recorded, unplaced: no rule for x/y"
            );
            assert_eq!(tracked(&json!({})), "recorded");

            let hub = Hub::new(vec![(
                "pipeline.record",
                Ok(json!({ "column": "accounts" })),
            )]);
            let reply = record(
                &|t, a| hub.call(t, a),
                "onboarding",
                "S-1",
                "accounts",
                "started",
                json!({ "name": "Ada" }),
                Some("run-1"),
            );
            assert_eq!(reply, "recorded → accounts");
            assert_eq!(
                hub.args(0),
                json!({
                    "pipeline": "onboarding", "item": "S-1", "stage": "accounts", "event": "started",
                    "detail": { "name": "Ada" }, "run": "run-1",
                })
            );

            let hub = Hub::new(vec![("pipeline.record", Ok(json!({})))]);
            record(
                &|t, a| hub.call(t, a),
                "onboarding",
                "S-1",
                "accounts",
                "started",
                json!({}),
                None,
            );
            assert!(hub.args(0).get("run").is_none());

            let broken = |_: &str, _: &Value| -> Result<Value, String> { Err("no store".into()) };
            assert_eq!(
                record(
                    &broken,
                    "onboarding",
                    "S-1",
                    "accounts",
                    "started",
                    json!({}),
                    None
                ),
                "skipped: no store"
            );
        }

        #[test]
        fn a_shipped_pipeline_records_after_ensuring_even_when_the_define_is_lost() {
            let hub = Hub::new(vec![
                ("pipeline.get", Err("not_found".into())),
                (
                    "pipeline.define",
                    Err("version conflict: expected 0, found 1".into()),
                ),
                ("pipeline.record", Ok(json!({ "column": "accounts" }))),
            ]);
            let reply = SHIPPED.record(
                &|t, a| hub.call(t, a),
                "S-1",
                "accounts",
                "started",
                json!({}),
                Some("run-1"),
            );
            assert_eq!(reply, "recorded → accounts");
            assert_eq!(
                hub.tools(),
                ["pipeline.get", "pipeline.define", "pipeline.record"]
            );
            assert_eq!(hub.args(2)["pipeline"], "onboarding");
        }
    }
}

pub mod http {
    #[allow(unused_imports)]
    use super::*;

    /// Response from an HTTP request.
    #[derive(Debug, Clone)]
    pub struct FetchResponse {
        /// HTTP status code (e.g., 200, 404, 500).
        pub status: i32,
        /// Response body as a string.
        pub body: String,
        /// Body encoding: "utf8" for text, "base64" for binary content.
        pub body_encoding: String,
        /// Response headers.
        pub headers: HashMap<String, String>,
    }

    impl FetchResponse {
        /// Parse the response body as JSON.
        pub fn json(&self) -> Option<Value> {
            serde_json::from_str(&self.body).ok()
        }

        /// Check if the response status indicates success (2xx).
        pub fn is_success(&self) -> bool {
            (200..300).contains(&self.status)
        }

        /// Check if the body is base64-encoded (binary content).
        pub fn is_base64(&self) -> bool {
            self.body_encoding == "base64"
        }
    }

    /// Make an HTTP request.
    pub fn fetch(
        method: &str,
        url: &str,
        headers: &[(&str, &str)],
        body: Option<&str>,
    ) -> Option<FetchResponse> {
        #[cfg(target_arch = "wasm32")]
        {
            let method_bytes = method.as_bytes();
            let url_bytes = url.as_bytes();
            let headers_map: HashMap<&str, &str> = headers.iter().copied().collect();
            let headers_json = serde_json::to_string(&headers_map).unwrap_or_default();
            let headers_bytes = headers_json.as_bytes();
            let (body_bytes, body_len) = match body {
                Some(b) => (b.as_bytes(), b.len()),
                None => (&[] as &[u8], 0),
            };

            let result = unsafe {
                wf_http_fetch(
                    method_bytes.as_ptr() as i32,
                    method_bytes.len() as i32,
                    url_bytes.as_ptr() as i32,
                    url_bytes.len() as i32,
                    headers_bytes.as_ptr() as i32,
                    headers_bytes.len() as i32,
                    body_bytes.as_ptr() as i32,
                    body_len as i32,
                )
            };

            if result < 0 {
                return None;
            }

            read_fetch_response()
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            let _ = (method, url, headers, body);
            None
        }
    }

    /// Make a GET request.
    pub fn get(url: &str, headers: &[(&str, &str)]) -> Option<FetchResponse> {
        fetch("GET", url, headers, None)
    }

    /// Make a POST request with a body.
    pub fn post(url: &str, headers: &[(&str, &str)], body: &str) -> Option<FetchResponse> {
        fetch("POST", url, headers, Some(body))
    }

    /// Make a PUT request with a body.
    pub fn put(url: &str, headers: &[(&str, &str)], body: &str) -> Option<FetchResponse> {
        fetch("PUT", url, headers, Some(body))
    }

    /// Make a DELETE request.
    pub fn delete(url: &str, headers: &[(&str, &str)]) -> Option<FetchResponse> {
        fetch("DELETE", url, headers, None)
    }

    /// Make a PATCH request with a body.
    pub fn patch(url: &str, headers: &[(&str, &str)], body: &str) -> Option<FetchResponse> {
        fetch("PATCH", url, headers, Some(body))
    }

    #[cfg(target_arch = "wasm32")]
    fn read_fetch_response() -> Option<FetchResponse> {
        let json_str = read_host_response_string()?;
        let v: Value = serde_json::from_str(&json_str).ok()?;
        Some(FetchResponse {
            status: v["status"].as_i64().unwrap_or(0) as i32,
            body: v["body"].as_str().unwrap_or("").to_string(),
            body_encoding: v["body_encoding"].as_str().unwrap_or("utf8").to_string(),
            headers: v["headers"]
                .as_object()
                .map(|m| {
                    m.iter()
                        .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                        .collect()
                })
                .unwrap_or_default(),
        })
    }
}

// ─── artifact module ────────────────────────────────────────────────────────

/// Read and stage workforce artifacts (uploaded files, produced outputs).
///
/// A skill takes the bytes of a staged file by `artifact_id`, transforms them
/// in-process, and stages the result for delivery — the bytes never pass
/// through the agent's LLM context.
///
/// Requires the `artifacts` capability in your `Skill.toml`.
pub mod artifact {
    #[allow(unused_imports)]
    use super::*;
    #[cfg(target_arch = "wasm32")]
    use base64::Engine;

    /// The bytes of a staged artifact plus its stored mime and size.
    #[derive(Debug, Clone)]
    pub struct Artifact {
        pub bytes: Vec<u8>,
        pub mime: String,
        pub size: usize,
    }

    /// A staged artifact ready for delivery. Pass it to [`attach`] so the
    /// runtime delivers the file to the conversation.
    #[derive(Debug, Clone)]
    pub struct StagedArtifact {
        pub artifact_id: String,
        pub media_type: String,
        pub filename: String,
        pub bytes: u64,
    }

    impl StagedArtifact {
        /// The delivery entry the runtime expects for one file.
        pub fn entry(&self) -> Value {
            serde_json::json!({
                "artifact_id": self.artifact_id,
                "media_type": self.media_type,
                "filename": self.filename,
            })
        }
    }

    /// Read a staged artifact by id. Returns `None` if it doesn't exist, isn't
    /// readable in this session, or exceeds the host read limit.
    pub fn read(id: &str) -> Option<Artifact> {
        #[cfg(target_arch = "wasm32")]
        {
            let bytes = id.as_bytes();
            let result = unsafe { wf_read_artifact(bytes.as_ptr() as i32, bytes.len() as i32) };
            if result < 0 {
                return None;
            }
            let v: Value = serde_json::from_str(&read_host_response_string()?).ok()?;
            let decoded = base64::engine::general_purpose::STANDARD
                .decode(v["bytes_base64"].as_str()?)
                .ok()?;
            Some(Artifact {
                bytes: decoded,
                mime: v["mime"]
                    .as_str()
                    .unwrap_or("application/octet-stream")
                    .to_string(),
                size: v["size"].as_u64().unwrap_or(0) as usize,
            })
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            let _ = id;
            None
        }
    }

    /// Stage `bytes` as a new artifact for delivery. `media_type` and
    /// `filename` label the delivered download. Returns `None` on failure.
    pub fn stage(bytes: &[u8], media_type: &str, filename: &str) -> Option<StagedArtifact> {
        #[cfg(target_arch = "wasm32")]
        {
            let payload = serde_json::json!({
                "bytes_base64": base64::engine::general_purpose::STANDARD.encode(bytes),
                "media_type": media_type,
                "filename": filename,
            })
            .to_string();
            let pb = payload.as_bytes();
            let result = unsafe { wf_stage_artifact(pb.as_ptr() as i32, pb.len() as i32) };
            if result < 0 {
                return None;
            }
            staged_from_host()
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            let _ = (bytes, media_type, filename);
            None
        }
    }

    /// The delivery marker the host answered a stage or render call with.
    #[cfg(target_arch = "wasm32")]
    pub(super) fn staged_from_host() -> Option<StagedArtifact> {
        let v: Value = serde_json::from_str(&read_host_response_string()?).ok()?;
        Some(StagedArtifact {
            artifact_id: v["artifact_id"].as_str()?.to_string(),
            media_type: v["media_type"].as_str()?.to_string(),
            filename: v["filename"].as_str()?.to_string(),
            bytes: v["bytes"].as_u64().unwrap_or(0),
        })
    }

    /// Key under which the runtime scans a tool result for files to deliver.
    const DELIVERY_KEY: &str = "generated_images";

    /// Attach staged files to a tool result so the runtime delivers them. Sets
    /// the workforce file-delivery key on the result object; a non-object
    /// result or an empty `files` slice is returned unchanged.
    pub fn attach(mut result: Value, files: &[StagedArtifact]) -> Value {
        if let (Value::Object(map), false) = (&mut result, files.is_empty()) {
            map.insert(
                DELIVERY_KEY.to_string(),
                Value::Array(files.iter().map(StagedArtifact::entry).collect()),
            );
        }
        result
    }
}

// ─── chart module ───────────────────────────────────────────────────────────

/// Draw a chart on the host from a `wf-chart` spec — the same JSON the chat
/// widget renders (`{kind, title?, labels?, series:[{name?, data}], …}`) —
/// and stage the image for delivery. No plotting code ships in the skill.
///
/// Requires the `artifacts` capability. Deliver the result with
/// [`artifact::attach`], or route it elsewhere by `artifact_id` through
/// `tools::invoke` (`messaging.send_file`, an email tool's
/// `attach_artifact_ids`).
pub mod chart {
    pub use super::artifact::StagedArtifact;
    #[allow(unused_imports)]
    use super::*;

    /// How a chart is drawn beyond its data: the file format, a named theme
    /// (a workspace brand theme or a built-in such as `slate`) and an explicit
    /// series palette of `#rrggbb` colours that overrides the theme's.
    #[derive(Debug, Clone, Default)]
    pub struct Style {
        pub format: Option<String>,
        pub theme: Option<String>,
        pub colors: Vec<String>,
    }

    /// A PNG of `spec` in the workspace theme, the format chat and email
    /// show inline.
    pub fn render(spec: &Value) -> Option<StagedArtifact> {
        render_with(spec, &Style::default())
    }

    /// `spec` drawn as `format`: `png` or `svg`.
    pub fn render_as(spec: &Value, format: &str) -> Option<StagedArtifact> {
        render_with(
            spec,
            &Style {
                format: Some(format.to_string()),
                ..Style::default()
            },
        )
    }

    /// `spec` drawn in `style`.
    pub fn render_with(spec: &Value, style: &Style) -> Option<StagedArtifact> {
        #[cfg(target_arch = "wasm32")]
        {
            let payload = serde_json::json!({
                "spec": spec,
                "format": style.format,
                "theme": style.theme,
                "colors": if style.colors.is_empty() { Value::Null } else { serde_json::json!(style.colors) },
            })
            .to_string();
            let pb = payload.as_bytes();
            let result = unsafe { wf_render_chart(pb.as_ptr() as i32, pb.len() as i32) };
            if result < 0 {
                return None;
            }
            super::artifact::staged_from_host()
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            let _ = (spec, style);
            None
        }
    }
}

// ─── util module ────────────────────────────────────────────────────────────

/// Utility functions for common operations in WASM skill handlers.
pub mod util {
    #[allow(unused_imports)]
    use super::*;

    /// Generate a new random UUID v4 string.
    pub fn generate_uuid() -> String {
        #[cfg(target_arch = "wasm32")]
        {
            let result = unsafe { super::wf_generate_uuid() };
            if result < 0 {
                return String::new();
            }
            read_host_response_string().unwrap_or_default()
        }

        #[cfg(not(target_arch = "wasm32"))]
        {
            // Fallback for native testing — not a real UUID
            format!("{:016x}", {
                use std::time::SystemTime;
                SystemTime::now()
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .map(|d| d.as_nanos() as u64)
                    .unwrap_or(0)
            })
        }
    }

    /// Get the current UTC time as an RFC3339 string.
    pub fn current_time() -> String {
        #[cfg(target_arch = "wasm32")]
        {
            let result = unsafe { super::wf_current_time() };
            if result < 0 {
                return String::new();
            }
            read_host_response_string().unwrap_or_default()
        }

        #[cfg(not(target_arch = "wasm32"))]
        {
            use std::time::SystemTime;
            let secs = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            format!("1970-01-01T00:00:{:02}Z", secs % 60)
        }
    }
}

// ─── Handler runtime ────────────────────────────────────────────────────────

/// Internal function used by the `tool!` macro. Do not call directly.
#[doc(hidden)]
pub fn __run_tool_handler<F>(ptr: i32, len: i32, f: F) -> i32
where
    F: FnOnce(ToolInput) -> ToolOutput,
{
    // Read the request JSON from guest memory.
    let request_json = unsafe {
        let slice = std::slice::from_raw_parts(ptr as *const u8, len as usize);
        String::from_utf8_lossy(slice).into_owned()
    };

    // Parse the input.
    let input = ToolInput::from_json(&request_json).unwrap_or_else(|| ToolInput {
        data: Value::Null,
        tool_name: String::new(),
        agent_id: String::new(),
        user_id: None,
    });

    // Call the user's handler.
    let output = f(input);
    let response_bytes = output.into_json().into_bytes();

    // Write response to guest memory: [4-byte LE length][data]
    let total = 4 + response_bytes.len();
    let layout = std::alloc::Layout::from_size_align(total, 1).expect("invalid layout");
    let out_ptr = unsafe { std::alloc::alloc(layout) };

    unsafe {
        let len_bytes = (response_bytes.len() as u32).to_le_bytes();
        std::ptr::copy_nonoverlapping(len_bytes.as_ptr(), out_ptr, 4);
        std::ptr::copy_nonoverlapping(
            response_bytes.as_ptr(),
            out_ptr.add(4),
            response_bytes.len(),
        );
    }

    out_ptr as i32
}

// ─── Macros ─────────────────────────────────────────────────────────────────

/// Initialize the workforce skill SDK runtime.
///
/// Call this once at the top of your `lib.rs`. Exports the `alloc` function
/// that the workforce runtime needs to pass data into your WASM module.
///
/// ```rust,ignore
/// workforce_skill_sdk::init!();
/// ```
#[macro_export]
macro_rules! init {
    () => {
        #[no_mangle]
        pub extern "C" fn alloc(size: i32) -> i32 {
            let layout = std::alloc::Layout::from_size_align(size as usize, 1).unwrap();
            unsafe { std::alloc::alloc(layout) as i32 }
        }
    };
}

/// Define a tool handler function.
///
/// This macro generates the `#[no_mangle] extern "C"` boilerplate so your
/// tool handler is a plain Rust closure that receives a [`ToolInput`] and
/// returns a [`ToolOutput`].
///
/// ```rust,ignore
/// use workforce_skill_sdk::prelude::*;
///
/// workforce_skill_sdk::init!();
///
/// tool!(get_weather, |input: ToolInput| {
///     let city = input.get_str("city").unwrap_or("unknown");
///     ToolOutput::success(json!({"city": city, "temp": 22}))
/// });
/// ```
#[macro_export]
macro_rules! tool {
    ($name:ident, |$input:ident : ToolInput| $body:expr) => {
        #[no_mangle]
        pub extern "C" fn $name(ptr: i32, len: i32) -> i32 {
            $crate::__run_tool_handler(ptr, len, |$input: $crate::ToolInput| $body)
        }
    };
}

// ─── Prelude ────────────────────────────────────────────────────────────────

/// Import everything you need to write skill tools.
///
/// ```rust,ignore
/// use workforce_skill_sdk::prelude::*;
/// ```
pub mod prelude {
    pub use crate::artifact;
    pub use crate::config;
    pub use crate::http;
    pub use crate::log;
    pub use crate::pipeline;
    pub use crate::store;
    pub use crate::tools;
    pub use crate::util;
    pub use crate::vault;
    pub use crate::ToolInput;
    pub use crate::ToolOutput;
    pub use crate::{init, tool};
    pub use serde_json::{json, Value};
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn tool_input_parsing() {
        let json = serde_json::to_string(&json!({
            "tool_name": "weather.get_forecast",
            "handler": "get_forecast",
            "input": {"city": "London", "units": "metric"},
            "agent_id": "agent-1",
        }))
        .unwrap();

        let input = ToolInput::from_json(&json).unwrap();
        assert_eq!(input.tool_name, "weather.get_forecast");
        assert_eq!(input.agent_id, "agent-1");
        assert_eq!(input.get_str("city"), Some("London"));
        assert_eq!(input.get_str("units"), Some("metric"));
        assert!(input.get_str("nonexistent").is_none());
    }

    #[test]
    fn tool_input_accessors() {
        let json = serde_json::to_string(&json!({
            "input": {"count": 42, "ratio": 2.78, "active": true},
        }))
        .unwrap();

        let input = ToolInput::from_json(&json).unwrap();
        assert_eq!(input.get_i64("count"), Some(42));
        assert_eq!(input.get_f64("ratio"), Some(2.78));
        assert_eq!(input.get_bool("active"), Some(true));
    }

    #[test]
    fn tool_input_missing_fields() {
        let json = r#"{"input": {}}"#;
        let input = ToolInput::from_json(json).unwrap();
        assert_eq!(input.tool_name, "");
        assert_eq!(input.agent_id, "");
        assert_eq!(input.user_id, None);
    }

    #[test]
    fn tool_input_parses_user_id() {
        let json = serde_json::to_string(&json!({
            "tool_name": "x",
            "agent_id": "a",
            "user_id": "alice",
            "input": {},
        }))
        .unwrap();
        let input = ToolInput::from_json(&json).unwrap();
        assert_eq!(input.user_id.as_deref(), Some("alice"));
    }

    #[test]
    fn tool_input_user_id_absent_when_unset() {
        let json = r#"{"tool_name": "x", "agent_id": "a", "input": {}}"#;
        let input = ToolInput::from_json(json).unwrap();
        assert_eq!(input.user_id, None);
    }

    #[test]
    fn tool_output_success() {
        let output = ToolOutput::success(json!({"data": "test"}));
        let json_str = output.into_json();
        let parsed: Value = serde_json::from_str(&json_str).unwrap();
        assert_eq!(parsed["success"], true);
        assert_eq!(parsed["result"]["data"], "test");
        assert!(parsed["error"].is_null());
    }

    #[test]
    fn tool_output_error() {
        let output = ToolOutput::error("something failed");
        let json_str = output.into_json();
        let parsed: Value = serde_json::from_str(&json_str).unwrap();
        assert_eq!(parsed["success"], false);
        assert!(parsed["result"].is_null());
        assert_eq!(parsed["error"], "something failed");
    }

    #[test]
    fn vault_get_noop_on_native() {
        assert!(vault::get("any-key").is_none());
    }

    #[test]
    fn vault_set_noop_on_native() {
        assert!(vault::set("key", "value"));
    }

    #[test]
    fn config_get_noop_on_native() {
        assert!(config::get("any-key").is_none());
    }

    #[test]
    fn http_get_noop_on_native() {
        assert!(http::get("https://example.com", &[]).is_none());
    }

    #[test]
    fn http_post_noop_on_native() {
        assert!(http::post("https://example.com", &[], "{}").is_none());
    }

    #[test]
    fn util_generate_uuid() {
        let id = util::generate_uuid();
        assert!(!id.is_empty());
    }

    #[test]
    fn util_current_time() {
        let time = util::current_time();
        assert!(!time.is_empty());
    }

    #[test]
    fn http_fetch_response_helpers() {
        let resp = http::FetchResponse {
            status: 200,
            body: r#"{"key": "value"}"#.to_string(),
            body_encoding: "utf8".to_string(),
            headers: HashMap::new(),
        };
        assert!(resp.is_success());
        assert!(!resp.is_base64());
        let json = resp.json().unwrap();
        assert_eq!(json["key"], "value");

        let err_resp = http::FetchResponse {
            status: 404,
            body: "not found".to_string(),
            body_encoding: "utf8".to_string(),
            headers: HashMap::new(),
        };
        assert!(!err_resp.is_success());

        let binary_resp = http::FetchResponse {
            status: 200,
            body: "aW1hZ2VkYXRh".to_string(),
            body_encoding: "base64".to_string(),
            headers: HashMap::new(),
        };
        assert!(binary_resp.is_base64());
    }

    #[test]
    fn artifact_read_stage_noop_on_native() {
        assert!(artifact::read("art_0").is_none());
        assert!(artifact::stage(b"x", "text/plain", "x.txt").is_none());
    }

    #[test]
    fn tools_invoke_noop_on_native() {
        assert!(tools::invoke("echo.say", &json!({})).is_err());
        assert!(tools::invoke_many(&[("echo.say", json!({}))]).is_err());
    }

    #[test]
    fn artifact_attach_sets_delivery_key() {
        let staged = artifact::StagedArtifact {
            artifact_id: "art_00000000000000000000000000000000".to_string(),
            media_type: "application/pdf".to_string(),
            filename: "out.pdf".to_string(),
            bytes: 42,
        };
        let out = artifact::attach(json!({ "rows": 3 }), std::slice::from_ref(&staged));
        assert_eq!(out["rows"], 3);
        let entries = out["generated_images"].as_array().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["artifact_id"], staged.artifact_id);
        assert_eq!(entries[0]["media_type"], "application/pdf");
        assert_eq!(entries[0]["filename"], "out.pdf");
    }

    #[test]
    fn artifact_attach_empty_is_unchanged() {
        let out = artifact::attach(json!({ "rows": 3 }), &[]);
        assert!(out.get("generated_images").is_none());
    }
}
