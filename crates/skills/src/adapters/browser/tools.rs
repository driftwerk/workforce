use std::sync::Arc;

use async_trait::async_trait;
use eyre::{eyre, Result};
use futures::future::BoxFuture;
use futures::FutureExt;
use serde_json::{json, Value};
use tracing::warn;

use workforce_agent::tools::{Tool, ToolResult};

use super::client::BrowserClient;

use crate::adapters::utils::coerce_to_u64;
use crate::definition::{SkillPolicy, SkillTool};

type Handler =
    Box<dyn Fn(Arc<BrowserClient>, Value) -> BoxFuture<'static, Result<Value>> + Send + Sync>;

struct ToolSpec {
    name: &'static str,
    description: &'static str,
    schema: Value,
    handler: Handler,
}

struct BrowserTool {
    client: Arc<BrowserClient>,
    spec: ToolSpec,
}

#[async_trait]
impl Tool for BrowserTool {
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
                warn!(target: "browser", tool = self.spec.name, error = %e, "tool call failed");
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
    F: Fn(Arc<BrowserClient>, Value) -> BoxFuture<'static, Result<Value>> + Send + Sync + 'static,
{
    Box::new(f)
}

fn specs() -> Vec<ToolSpec> {
    vec![
        ToolSpec {
            name: "browser.navigate",
            description:
                "Navigate the browser to a URL. Returns after the page finishes loading.",
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
            name: "browser.snapshot",
            description: "Return the current page URL, title, and visible body text.",
            schema: json!({ "type": "object", "properties": {} }),
            handler: handler(|c, _input| async move { c.snapshot().await }.boxed()),
        },
        ToolSpec {
            name: "browser.screenshot",
            description:
                "Capture a PNG screenshot of the current page. Reflects all current DOM mutations \
                 and fills. The system stages the image and delivers it to the originating \
                 conversation automatically — you do not see the attachment and you do not need \
                 to call any upload tool. Just briefly tell the user what's on screen.",
            schema: json!({ "type": "object", "properties": {} }),
            handler: handler(|c, _input| async move { c.screenshot().await }.boxed()),
        },
        ToolSpec {
            name: "browser.pdf",
            description:
                "Render the current page as a PDF. Reflects all current DOM mutations and fills. \
                 The system stages the file and delivers it to the originating conversation \
                 automatically (e.g. a Slack file upload) — you do not see the attachment and you \
                 do not need to call any upload tool. Just briefly tell the user what the PDF \
                 contains. To turn an HTML file into a PDF, browser.navigate to it first \
                 (a file:// or data:text/html URL), then call this.",
            schema: json!({
                "type": "object",
                "properties": {
                    "filename": {
                        "type": "string",
                        "description": "Name for the delivered file, e.g. 'report.pdf'. Defaults to 'page.pdf'."
                    }
                }
            }),
            handler: handler(|c, input| {
                async move {
                    let filename = input["filename"]
                        .as_str()
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .unwrap_or("page.pdf");
                    c.pdf(filename).await
                }
                .boxed()
            }),
        },
        ToolSpec {
            name: "browser.click",
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
            name: "browser.fill",
            description:
                "Set the value of an <input> or <textarea>, firing input and change events. Use \
                 browser.type for contenteditable. The `value` field accepts \
                 `[[secret:<name>]]` markers (set via `cmd: secret`, interactive flows) and, in a \
                 workflow run, the `[[vault:<scope>:<key>]]` marker of a declared secret variable \
                 (scope: agent_user/global_user/agent/global). Outside a workflow, a legacy \
                 `[[vault:<key>]]` resolves `browser_creds/<key>` from the per-agent vault. Markers \
                 are dereferenced server-side; neither the marker nor the cleartext reaches the \
                 model context.",
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
            name: "browser.type",
            description:
                "Focus the element and type text key-by-key (simulates real typing, fires \
                 keydown/keyup). The `text` field accepts `[[secret:<name>]]` and \
                 `[[vault:<scope>:<key>]]` (declared secret variable) markers, dereferenced \
                 server-side — same semantics as browser.fill.",
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
            name: "browser.press_key",
            description:
                "Press a single key (e.g. 'Enter', 'Tab', 'ArrowDown'). Targets a selector if given, \
                 otherwise the currently focused element.",
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
            name: "browser.select_option",
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
            name: "browser.upload_file",
            description:
                "Upload a staged artifact (a generated image, an export_xlsx, a viewed/downloaded \
                 file — referenced by its artifact_id) to a file <input> on the current page. \
                 Provide the CSS `selector` of the file input and the `artifact_id`. The bytes are \
                 read from the artifact store automatically — you never handle them. After this, \
                 submit the form as usual (e.g. browser.click the submit/upload button).",
            schema: json!({
                "type": "object",
                "properties": {
                    "selector": { "type": "string", "description": "CSS selector of the <input type=\"file\"> element." },
                    "artifact_id": { "type": "string", "description": "Artifact id (art_…) of the staged file to upload." },
                    "filename": { "type": "string", "description": "Optional filename the site should see; derived from the artifact if omitted." }
                },
                "required": ["selector", "artifact_id"]
            }),
            handler: handler(|c, input| {
                async move {
                    let selector = require_str(&input, "selector")?;
                    let artifact_id = require_str(&input, "artifact_id")?;
                    let filename = input["filename"].as_str().filter(|s| !s.is_empty());
                    c.upload_file(selector, artifact_id, filename).await
                }
                .boxed()
            }),
        },
        ToolSpec {
            name: "browser.evaluate",
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
            name: "browser.wait",
            description: "Poll for a CSS selector to appear; errors on timeout.",
            schema: json!({
                "type": "object",
                "properties": {
                    "selector": { "type": "string", "description": "CSS selector" },
                    "timeout_ms": { "type": "integer", "description": "Override the default call timeout; capped at 120000 (120 s) per call" }
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
            name: "browser.get_content",
            description:
                "Return page content. format='text' returns body.innerText; format='html' (default) returns documentElement.outerHTML.",
            schema: json!({
                "type": "object",
                "properties": {
                    "format": { "type": "string", "enum": ["text", "html"] }
                }
            }),
            handler: handler(|c, input| {
                async move {
                    let format = input["format"].as_str().unwrap_or("html");
                    c.get_content(format).await
                }
                .boxed()
            }),
        },
        ToolSpec {
            name: "browser.network_requests",
            description:
                "Return the buffered list of network requests observed since the page was opened (bounded ring buffer).",
            schema: json!({ "type": "object", "properties": {} }),
            handler: handler(|c, _input| async move { c.network_requests().await }.boxed()),
        },
        ToolSpec {
            name: "browser.console_messages",
            description:
                "Return the buffered list of console / log entries observed since the page was opened (bounded ring buffer).",
            schema: json!({ "type": "object", "properties": {} }),
            handler: handler(|c, _input| async move { c.console_messages().await }.boxed()),
        },
    ]
}

pub(super) fn build_tools(client: Arc<BrowserClient>) -> Vec<Arc<dyn Tool>> {
    specs()
        .into_iter()
        .map(|spec| {
            Arc::new(BrowserTool {
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
    fn tool_names_are_unique_and_browser_prefixed() {
        let names = tool_names();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), names.len(), "duplicate tool name");
        for n in &names {
            assert!(n.starts_with("browser."), "tool name must be dotted: {n}");
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
}
