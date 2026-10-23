//! Data-driven Google tool dispatcher and shared helpers for service modules.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use eyre::Result;
use workforce_agent::tools::{Tool, ToolResult};

use super::api::{GoogleApi, GoogleApiContext};

/// Boxed future type for tool handlers.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

pub type HandlerFn = Arc<
    dyn for<'a> Fn(serde_json::Value, &'a GoogleApi) -> BoxFuture<'a, Result<serde_json::Value>>
        + Send
        + Sync,
>;

pub struct GoogleTool {
    name: String,
    description: String,
    schema: serde_json::Value,
    ctx: Arc<GoogleApiContext>,
    handler: HandlerFn,
}

impl GoogleTool {
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        schema: serde_json::Value,
        ctx: Arc<GoogleApiContext>,
        handler: HandlerFn,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            schema,
            ctx,
            handler,
        }
    }

    pub fn arc(self) -> Arc<dyn Tool> {
        Arc::new(self)
    }
}

#[async_trait]
impl Tool for GoogleTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn input_schema(&self) -> serde_json::Value {
        self.schema.clone()
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();
        let account = input
            .get("account")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let api = self.ctx.resolve(account.as_deref());
        let result = (self.handler)(input, api).await;
        let duration = start.elapsed().as_millis() as u64;
        match result {
            Ok(out) => Ok(ToolResult::success(out, duration)),
            Err(e) => Ok(ToolResult::from_report(&e, duration)),
        }
    }
}

// ─── Schema helpers ──────────────────────────────────────────────────────────

pub fn account_property() -> serde_json::Value {
    serde_json::json!({
        "type": "string",
        "description": "Account/workspace name for multi-account setups. Omit to use the default."
    })
}

pub fn object_schema(
    mut properties: serde_json::Map<String, serde_json::Value>,
    required: &[&str],
) -> serde_json::Value {
    properties.insert("account".to_string(), account_property());
    serde_json::json!({
        "type": "object",
        "properties": properties,
        "required": required,
    })
}

/// Build a properties map from `(&str, Value)` pairs — shared across all
/// service modules to avoid per-file `props()` duplication.
pub fn props(pairs: &[(&str, serde_json::Value)]) -> serde_json::Map<String, serde_json::Value> {
    let mut m = serde_json::Map::new();
    for (k, v) in pairs {
        m.insert((*k).to_string(), v.clone());
    }
    m
}

/// Shorthand: build a `GoogleTool` and wrap it in `Arc<dyn Tool>`.
pub fn tool(
    name: &str,
    desc: &str,
    schema: serde_json::Value,
    ctx: &Arc<GoogleApiContext>,
    h: HandlerFn,
) -> Arc<dyn Tool> {
    GoogleTool::new(name, desc, schema, ctx.clone(), h).arc()
}

// ─── Input helpers ───────────────────────────────────────────────────────────

pub fn required_str<'a>(input: &'a serde_json::Value, field: &str) -> Result<&'a str> {
    input
        .get(field)
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| eyre::eyre!("missing required field `{field}`"))
}

pub fn optional_str<'a>(input: &'a serde_json::Value, field: &str) -> Option<&'a str> {
    input.get(field).and_then(|v| v.as_str())
}

pub fn optional_u64(input: &serde_json::Value, field: &str) -> Option<u64> {
    match input.get(field)? {
        serde_json::Value::Number(n) => n.as_u64(),
        serde_json::Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

/// Accept either a JSON array or a JSON-encoded array string. Some LLMs
/// (notably smaller / non-frontier models) emit nested array tool args as
/// stringified JSON; this lets the adapter recover instead of forwarding
/// the malformed shape to Google's API.
fn coerce_array(v: &serde_json::Value) -> Option<serde_json::Value> {
    match v {
        serde_json::Value::Array(_) => Some(v.clone()),
        serde_json::Value::String(s) => match serde_json::from_str::<serde_json::Value>(s) {
            Ok(parsed @ serde_json::Value::Array(_)) => Some(parsed),
            _ => None,
        },
        _ => None,
    }
}

pub fn required_array(input: &serde_json::Value, field: &str) -> Result<serde_json::Value> {
    let raw = input
        .get(field)
        .ok_or_else(|| eyre::eyre!("missing required field `{field}`"))?;
    coerce_array(raw).ok_or_else(|| {
        eyre::eyre!("field `{field}` must be a JSON array (or a JSON-encoded array string)")
    })
}

pub fn optional_array(input: &serde_json::Value, field: &str) -> Option<serde_json::Value> {
    input.get(field).and_then(coerce_array)
}

pub fn urlencode(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

pub fn handler<F>(f: F) -> HandlerFn
where
    F: for<'a> Fn(serde_json::Value, &'a GoogleApi) -> BoxFuture<'a, Result<serde_json::Value>>
        + Send
        + Sync
        + 'static,
{
    Arc::new(f)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn required_array_accepts_json_array() {
        let input = json!({ "values": [["a", "b"], ["c", "d"]] });
        let got = required_array(&input, "values").unwrap();
        assert_eq!(got, json!([["a", "b"], ["c", "d"]]));
    }

    #[test]
    fn required_array_accepts_stringified_array() {
        let input = json!({ "values": "[[\"a\",\"b\"]]" });
        let got = required_array(&input, "values").unwrap();
        assert_eq!(got, json!([["a", "b"]]));
    }

    #[test]
    fn required_array_rejects_string_that_parses_to_non_array() {
        let input = json!({ "values": "{\"not\":\"array\"}" });
        assert!(required_array(&input, "values").is_err());
    }

    #[test]
    fn required_array_rejects_bare_string() {
        let input = json!({ "values": "hello" });
        assert!(required_array(&input, "values").is_err());
    }

    #[test]
    fn required_array_missing_field_errors() {
        let input = json!({});
        let err = required_array(&input, "values").unwrap_err();
        assert!(err.to_string().contains("missing required field"));
    }

    #[test]
    fn optional_array_returns_none_for_absent_field() {
        let input = json!({});
        assert!(optional_array(&input, "x").is_none());
    }

    #[test]
    fn optional_array_returns_none_for_invalid_shape() {
        let input = json!({ "x": 42 });
        assert!(optional_array(&input, "x").is_none());
    }
}
