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
        with_browser(self.spec.schema.clone())
    }

    /// A browser call, as Desk's gate keeps it. Opening a file reads it,
    /// as a file tool would, so the gate holds it to the agent's level and
    /// what Desk withholds. Any other call is kept under the origin it acts
    /// on (`browser:https://shop.example`), the one the page is on now or,
    /// for a navigation, the one it goes to; and that origin is what the
    /// call then acts on, or not at all ([`super::client::expect_origin`]).
    fn effect(&self, input: &Value) -> workforce_core::computer::Effect {
        use workforce_core::computer::Effect;
        let navigating = self.spec.name == "browser.navigate";
        let target = input["url"].as_str().unwrap_or_default();
        use super::client::{expect, Decided};
        if navigating {
            if let super::address::Kind::File(path) = super::address::kind(target) {
                let real = workforce_agent::tools::paths::resolve_real(&path);
                expect(
                    super::client::Backend::Desk,
                    Some(Decided::File(real.clone())),
                );
                return Effect::Read { paths: vec![real] };
            }
        }
        let backend = super::client::pick(input["browser"].as_str(), true)
            .unwrap_or(super::client::Backend::Desk);
        let origin = match navigating {
            true => super::address::origin_of(target),
            false => super::client::origin_now(backend),
        };
        let decision = match (backend, navigating) {
            (_, false) => Some(Decided::Origin(origin.clone())),
            (super::client::Backend::Chrome, true) => Some(Decided::Visit(origin.clone())),
            (super::client::Backend::Desk, true) => None,
        };
        expect(backend, decision);
        Effect::Connector {
            connector: site_key(backend, navigating, origin.as_deref()),
            write: !READS.contains(&self.spec.name),
        }
    }

    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();
        if let Err(e) = super::client::pick(input["browser"].as_str(), false) {
            return Ok(ToolResult::failure(e.to_string(), elapsed(start)));
        }
        match (self.spec.handler)(self.client.clone(), input).await {
            Ok(output) => Ok(ToolResult::success(output, elapsed(start))),
            Err(e) => {
                warn!(target: "browser", tool = self.spec.name, error = %e, "tool call failed");
                Ok(ToolResult::failure(e.to_string(), elapsed(start)))
            }
        }
    }
}

/// The tools that only read a page; the rest change one.
pub const READS: &[&str] = &[
    "browser.navigate",
    "browser.snapshot",
    "browser.screenshot",
    "browser.pdf",
    "browser.get_content",
    "browser.console_messages",
    "browser.network_requests",
    "browser.wait",
];

/// What Desk's gate keeps a browser call under: `browser:<origin>` in
/// Desk's own browser; in the person's Chrome `chrome:<origin>`, and for
/// opening a site there `chrome-visit:<origin>`. A page with no origin of
/// its own (blank, inline, a file) is kept under the browser alone, which
/// no allowance is shared for.
pub fn site_key(backend: super::client::Backend, navigating: bool, origin: Option<&str>) -> String {
    use super::client::Backend;
    let kind = match (backend, navigating) {
        (Backend::Desk, _) => "browser",
        (Backend::Chrome, true) => "chrome-visit",
        (Backend::Chrome, false) => "chrome",
    };
    match origin.filter(|o| !o.is_empty()) {
        Some(o) => format!("{kind}:{o}"),
        None if backend == Backend::Chrome => "chrome".into(),
        None => kind.into(),
    }
}

/// Every tool's `browser`: which browser it works in.
fn with_browser(mut schema: Value) -> Value {
    if let Some(props) = schema["properties"].as_object_mut() {
        props.insert(
            "browser".into(),
            json!({
                "type": "string",
                "enum": ["desk", "chrome"],
                "description": "Which browser: `desk` (Desk's own, which the person signs in to by hand) or `chrome` (the person's own Chrome, signed in as them, where Desk works in a window of its own). Leave it out to carry on in the one used last."
            }),
        );
    }
    schema
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
                    let kind = super::address::kind(url);
                    if !super::address::openable(&kind, super::address::In::Desk) {
                        let what = match kind {
                            super::address::Kind::Other(what) => what,
                            _ => "that".into(),
                        };
                        return Err(eyre!(
                            "The browser opens web pages, about:blank, an inline page or a file on this computer, not {what}."
                        ));
                    }
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
                    input_schema: with_browser(spec.schema),
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

    #[test]
    fn opening_a_file_is_a_read_of_it_and_any_other_call_is_kept_by_origin() {
        let skill =
            super::super::BrowserSkill::new(super::super::BrowserSkillConfig::default()).unwrap();
        let nav = skill
            .tools()
            .into_iter()
            .find(|t| t.name() == "browser.navigate")
            .unwrap();
        for url in [
            "file:///etc/hosts",
            "FILE:///etc/hosts",
            "file:/etc/hosts",
            "file://localhost/etc/hosts",
        ] {
            let effect = nav.effect(&json!({ "url": url }));
            assert!(
                matches!(effect, workforce_core::computer::Effect::Read { ref paths } if paths.len() == 1),
                "{url}: {effect:?}"
            );
        }
        assert_eq!(
            nav.effect(&json!({ "url": "https://A.test:8443/x" })),
            workforce_core::computer::Effect::Connector {
                connector: "browser:https://a.test:8443".into(),
                write: false
            }
        );
        assert_eq!(
            nav.effect(&json!({ "url": "about:blank" })),
            workforce_core::computer::Effect::Connector {
                connector: "browser".into(),
                write: false
            }
        );
    }

    #[test]
    fn a_site_is_kept_by_its_browser_and_origin_and_a_page_with_none_shares_nothing() {
        use super::super::client::Backend;
        assert_eq!(
            site_key(Backend::Desk, false, Some("https://a.test")),
            "browser:https://a.test"
        );
        assert_eq!(
            site_key(Backend::Desk, true, Some("https://a.test")),
            "browser:https://a.test"
        );
        assert_eq!(site_key(Backend::Desk, false, None), "browser");
        assert_eq!(
            site_key(Backend::Chrome, false, Some("https://a.test")),
            "chrome:https://a.test"
        );
        assert_eq!(
            site_key(Backend::Chrome, true, Some("https://a.test")),
            "chrome-visit:https://a.test"
        );
        assert_eq!(site_key(Backend::Chrome, true, None), "chrome");
    }

    #[test]
    fn every_tool_names_its_browser() {
        for spec in specs() {
            let schema = with_browser(spec.schema);
            assert_eq!(
                schema["properties"]["browser"]["enum"],
                json!(["desk", "chrome"]),
                "{}",
                spec.name
            );
        }
    }

    #[test]
    fn reads_are_every_tool_that_changes_no_page() {
        let names = tool_names();
        for r in READS {
            assert!(names.contains(r), "{r}");
        }
        assert_eq!(READS.len() + 7, names.len());
    }
}
