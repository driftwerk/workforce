use std::sync::Arc;

use async_trait::async_trait;
use eyre::{eyre, Result};
use futures::future::BoxFuture;
use futures::FutureExt;
use serde_json::{json, Value};
use tracing::warn;

use workforce_agent::tools::{Tool, ToolResult};

use super::client::ObscuraClient;

use crate::adapters::utils::coerce_to_u64;
use crate::definition::{SkillPolicy, SkillTool};

type Handler =
    Box<dyn Fn(Arc<ObscuraClient>, Value) -> BoxFuture<'static, Result<Value>> + Send + Sync>;

struct ToolSpec {
    name: &'static str,
    description: &'static str,
    schema: Value,
    handler: Handler,
}

struct ObscuraTool {
    client: Arc<ObscuraClient>,
    spec: ToolSpec,
}

#[async_trait]
impl Tool for ObscuraTool {
    fn name(&self) -> &str {
        self.spec.name
    }

    fn description(&self) -> &str {
        self.spec.description
    }

    fn input_schema(&self) -> Value {
        self.spec.schema.clone()
    }

    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();
        match (self.spec.handler)(self.client.clone(), input).await {
            Ok(output) => Ok(ToolResult::success(output, elapsed(start))),
            Err(e) => {
                warn!(target: "obscura", tool = self.spec.name, error = %e, "tool call failed");
                Ok(ToolResult::failure(e.to_string(), elapsed(start)))
            }
        }
    }
}

fn elapsed(start: std::time::Instant) -> u64 {
    start.elapsed().as_millis() as u64
}

fn require_str<'a>(input: &'a Value, key: &str) -> Result<&'a str> {
    input[key]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| eyre!("'{key}' is required"))
}

fn handler<F>(f: F) -> Handler
where
    F: Fn(Arc<ObscuraClient>, Value) -> BoxFuture<'static, Result<Value>> + Send + Sync + 'static,
{
    Box::new(f)
}

fn specs() -> Vec<ToolSpec> {
    vec![
        ToolSpec {
            name: "obscura.navigate",
            description:
                "Navigate the headless browser to a URL. Lower-memory alternative to browser.navigate; \
                 no screenshot capability — use browser.* if you need a PNG.",
            schema: json!({
                "type": "object",
                "properties": {
                    "url": { "type": "string", "description": "URL to navigate to" }
                },
                "required": ["url"]
            }),
            handler: handler(|c, input| {
                async move {
                    let url = require_str(&input, "url")?;
                    c.navigate(url).await
                }
                .boxed()
            }),
        },
        ToolSpec {
            name: "obscura.snapshot",
            description: "Return the current page's URL, title, and visible body text.",
            schema: json!({ "type": "object", "properties": {} }),
            handler: handler(|c, _input| async move { c.snapshot().await }.boxed()),
        },
        ToolSpec {
            name: "obscura.click",
            description: "Click the first element matching the CSS selector.",
            schema: json!({
                "type": "object",
                "properties": {
                    "selector": { "type": "string", "description": "CSS selector" }
                },
                "required": ["selector"]
            }),
            handler: handler(|c, input| {
                async move {
                    let selector = require_str(&input, "selector")?;
                    c.click(selector).await
                }
                .boxed()
            }),
        },
        ToolSpec {
            name: "obscura.fill",
            description:
                "Set the value of an <input> or <textarea>, firing input and change events. The \
                 `value` field accepts `[[secret:<name>]]` (set via `cmd: secret`) and, outside a \
                 workflow run, `[[vault:<key>]]` markers (resolved against `obscura_creds/<key>`). \
                 In a workflow run obscura cannot enforce egress, so vault markers are refused — \
                 use the browser skill with a declared secret variable. Markers are dereferenced \
                 server-side; neither marker nor cleartext enters the model context.",
            schema: json!({
                "type": "object",
                "properties": {
                    "selector": { "type": "string", "description": "CSS selector" },
                    "value": { "type": "string", "description": "Value to set" }
                },
                "required": ["selector", "value"]
            }),
            handler: handler(|c, input| {
                async move {
                    let selector = require_str(&input, "selector")?;
                    let value = input["value"].as_str().unwrap_or("");
                    c.fill(selector, value).await
                }
                .boxed()
            }),
        },
        ToolSpec {
            name: "obscura.type",
            description:
                "Focus the element and type text key-by-key (simulates real typing). The `text` \
                 field accepts `[[secret:<name>]]` and `[[vault:<key>]]` markers, dereferenced \
                 server-side — same semantics as obscura.fill.",
            schema: json!({
                "type": "object",
                "properties": {
                    "selector": { "type": "string", "description": "CSS selector" },
                    "text": { "type": "string", "description": "Text to type" }
                },
                "required": ["selector", "text"]
            }),
            handler: handler(|c, input| {
                async move {
                    let selector = require_str(&input, "selector")?;
                    let text = input["text"].as_str().unwrap_or("");
                    c.type_text(selector, text).await
                }
                .boxed()
            }),
        },
        ToolSpec {
            name: "obscura.press_key",
            description:
                "Press a single key (e.g. 'Enter', 'Tab'). Targets a selector if given, otherwise \
                 the currently focused element.",
            schema: json!({
                "type": "object",
                "properties": {
                    "key": { "type": "string", "description": "Key name (e.g. 'Enter')" },
                    "selector": { "type": "string", "description": "Optional CSS selector to focus first" }
                },
                "required": ["key"]
            }),
            handler: handler(|c, input| {
                async move {
                    let key = require_str(&input, "key")?;
                    let selector = input["selector"].as_str();
                    c.press_key(key, selector).await
                }
                .boxed()
            }),
        },
        ToolSpec {
            name: "obscura.select_option",
            description: "Set the value of a <select> by option value or visible text.",
            schema: json!({
                "type": "object",
                "properties": {
                    "selector": { "type": "string", "description": "CSS selector of the <select>" },
                    "value": { "type": "string", "description": "Option value or visible text" }
                },
                "required": ["selector", "value"]
            }),
            handler: handler(|c, input| {
                async move {
                    let selector = require_str(&input, "selector")?;
                    let value = require_str(&input, "value")?;
                    c.select_option(selector, value).await
                }
                .boxed()
            }),
        },
        ToolSpec {
            name: "obscura.evaluate",
            description: "Run a JavaScript expression in the page and return its result.",
            schema: json!({
                "type": "object",
                "properties": {
                    "expression": { "type": "string", "description": "JavaScript expression" }
                },
                "required": ["expression"]
            }),
            handler: handler(|c, input| {
                async move {
                    let expression = require_str(&input, "expression")?;
                    c.evaluate(expression).await
                }
                .boxed()
            }),
        },
        ToolSpec {
            name: "obscura.wait",
            description: "Wait for a CSS selector to appear; errors on timeout.",
            schema: json!({
                "type": "object",
                "properties": {
                    "selector": { "type": "string", "description": "CSS selector" },
                    "timeout_ms": { "type": "integer", "description": "Override default call timeout" }
                },
                "required": ["selector"]
            }),
            handler: handler(|c, input| {
                async move {
                    let selector = require_str(&input, "selector")?;
                    let timeout_ms = coerce_to_u64(&input["timeout_ms"]);
                    c.wait_for_selector(selector, timeout_ms).await
                }
                .boxed()
            }),
        },
        ToolSpec {
            name: "obscura.network_requests",
            description: "Return the list of network requests observed since the page was opened.",
            schema: json!({ "type": "object", "properties": {} }),
            handler: handler(|c, _input| async move { c.network_requests().await }.boxed()),
        },
        ToolSpec {
            name: "obscura.console_messages",
            description: "Return the list of console / log messages observed since the page was opened.",
            schema: json!({ "type": "object", "properties": {} }),
            handler: handler(|c, _input| async move { c.console_messages().await }.boxed()),
        },
    ]
}

pub(super) fn build_tools(client: Arc<ObscuraClient>) -> Vec<Arc<dyn Tool>> {
    specs()
        .into_iter()
        .map(|spec| {
            Arc::new(ObscuraTool {
                client: client.clone(),
                spec,
            }) as Arc<dyn Tool>
        })
        .collect()
}

pub(super) fn tool_names() -> Vec<&'static str> {
    specs().into_iter().map(|s| s.name).collect()
}

pub(super) fn skill_tools_and_policies() -> (Vec<SkillTool>, Vec<SkillPolicy>) {
    specs()
        .into_iter()
        .map(|spec| {
            (
                SkillTool {
                    name: spec.name.to_string(),
                    description: spec.description.to_string(),
                    input_schema: spec.schema,
                },
                SkillPolicy {
                    tool: spec.name.to_string(),
                    action: "allow".to_string(),
                },
            )
        })
        .unzip()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_names_are_unique_and_obscura_prefixed() {
        let names = tool_names();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), names.len(), "duplicate tool name");
        for n in &names {
            assert!(n.starts_with("obscura."), "tool name must be dotted: {n}");
        }
    }

    #[test]
    fn obscura_tools_do_not_overlap_with_browser_prefix() {
        for n in tool_names() {
            assert!(
                !n.starts_with("browser."),
                "should not collide with browser: {n}"
            );
        }
    }

    #[test]
    fn skill_tools_match_tool_names_one_to_one() {
        let (tools, policies) = skill_tools_and_policies();
        let names = tool_names();
        assert_eq!(tools.len(), names.len());
        assert_eq!(policies.len(), names.len());
        for (t, p) in tools.iter().zip(policies.iter()) {
            assert_eq!(t.name, p.tool);
            assert_eq!(p.action, "allow");
        }
    }

    #[test]
    fn every_spec_schema_is_an_object() {
        for spec in specs() {
            assert_eq!(
                spec.schema["type"], "object",
                "schema not object: {}",
                spec.name
            );
        }
    }

    #[test]
    fn no_screenshot_or_pdf_tools() {
        let names = tool_names();
        for forbidden in ["obscura.screenshot", "obscura.pdf"] {
            assert!(
                !names.contains(&forbidden),
                "should not expose: {forbidden}"
            );
        }
    }
}
