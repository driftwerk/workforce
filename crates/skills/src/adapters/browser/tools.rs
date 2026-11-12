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
        let navigating = workforce_core::computer::navigates(self.spec.name);
        let target = input["url"].as_str().unwrap_or_default();
        use super::client::{call_key, expect, Decided};
        let key = call_key(self.spec.name, input);
        if navigating {
            if let super::address::Kind::File(path) = super::address::kind(target) {
                let real = workforce_agent::tools::paths::resolve_real(&path);
                expect(
                    super::client::Backend::Desk,
                    &key,
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
            // The wait for the person's sign-in acts on no page: where it
            // ends up (an SSO hop included) is read after they answer.
            _ if self.spec.name == workforce_core::computer::SIGNED_IN => None,
            (_, false) => Some(Decided::Origin(origin.clone())),
            (super::client::Backend::Chrome, true) => Some(Decided::Visit(origin.clone())),
            (super::client::Backend::Desk, true) => None,
        };
        expect(backend, &key, decision);
        Effect::Connector {
            connector: site_key(backend, navigating, origin.as_deref()),
            write: !READS.contains(&self.spec.name),
        }
    }

    /// What an approval card shows of a fill: the field and what goes in
    /// it, as the model wrote it (a credential stays its `[[secret:…]]` or
    /// `[[vault:…]]` marker; anything that reads as one is redacted),
    /// capped, so two fills can be told apart.
    fn approval_summary(&self, input: &Value) -> Option<String> {
        (self.spec.name == "browser.fill").then(|| fill_summary(input))
    }

    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();
        if let Err(e) = super::client::pick(input["browser"].as_str(), false) {
            return Ok(ToolResult::failure(e.to_string(), elapsed(start)));
        }
        // While the person signs in, the browser waits for them.
        if held_by_sign_in(self.spec.name) {
            return Ok(ToolResult::failure(
                super::client::SIGNING_IN_NOW.to_string(),
                elapsed(start),
            ));
        }
        // This call takes what was decided for it, and nothing decided for
        // another call made alongside it.
        let call = super::client::Call {
            key: super::client::call_key(self.spec.name, &input),
            write: !READS.contains(&self.spec.name)
                && self.spec.name != workforce_core::computer::SIGNED_IN,
        };
        let ran = super::client::CALL_KEY
            .scope(call, (self.spec.handler)(self.client.clone(), input))
            .await;
        match ran {
            Ok(output) => Ok(ToolResult::success(output, elapsed(start))),
            Err(e) => {
                warn!(target: "browser", tool = self.spec.name, error = %e, "tool call failed");
                Ok(ToolResult::failure(e.to_string(), elapsed(start)))
            }
        }
    }
}

/// Whether `tool` waits because the person in scope is signing in: every
/// browser call does but the wait for them.
fn held_by_sign_in(tool: &str) -> bool {
    tool != workforce_core::computer::SIGNED_IN && super::client::signing_now()
}

/// The tools that only read a page; the rest change one.
pub const READS: &[&str] = &[
    "browser.navigate",
    "browser.sign_in",
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

/// The most of a fill's value an approval card shows.
const SHOWN_VALUE: usize = 200;

fn fill_summary(input: &Value) -> String {
    let selector = input["selector"].as_str().unwrap_or_default();
    let value =
        workforce_common::redact::redact_secrets(input["value"].as_str().unwrap_or_default());
    let shown = match value.char_indices().nth(SHOWN_VALUE) {
        Some((at, _)) => format!("{}…", &value[..at]),
        None => value,
    };
    format!("Fill {selector} with: {shown}")
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
            name: "browser.sign_in",
            description:
                "Sign in for me: when a site needs the person's password, a code or a passkey, open its sign-in page here on the person's screen, in Desk's own browser, for them to sign in themselves. Then call browser.signed_in, which waits for them. Never ask for, read or type their password.",
            schema: json!({
                "type": "object",
                "properties": {
                    "url": { "type": "string", "description": "The site's sign-in page (https)" }
                },
                "required": ["url"]
            }),
            handler: handler(|c, input| {
                async move {
                    let url = require_str(&input, "url")?;
                    if !matches!(super::address::kind(url), super::address::Kind::Web) {
                        return Err(eyre!("A sign-in page is a web page (https://…)."));
                    }
                    c.sign_in(url).await
                }
                .boxed()
            }),
        },
        ToolSpec {
            name: "browser.signed_in",
            description:
                "After browser.sign_in: shows the person a card and waits while they sign in in Desk's browser. Returns where the page is once they say they're done; if they cancel or don't answer, tell them it wasn't done.",
            schema: json!({ "type": "object", "properties": {} }),
            handler: handler(|c, _input| async move { c.signed_in().await }.boxed()),
        },
        ToolSpec {
            name: "browser.keep_google_client",
            description:
                "On Google Cloud Console in Desk's own browser, right after creating a Desktop app OAuth client: keeps its client ID and secret as the person's Google client, so Desk can sign them in to Google. Read the dialog with browser.snapshot: the secret shows as a [[secret:…]] marker; pass that marker as client_secret, never the secret itself, and never screenshot the dialog.",
            schema: json!({
                "type": "object",
                "properties": {
                    "client_id": { "type": "string", "description": "The client ID: …apps.googleusercontent.com" },
                    "client_secret": { "type": "string", "description": "The [[secret:…]] marker browser.snapshot showed for the client secret" }
                },
                "required": ["client_id", "client_secret"]
            }),
            handler: handler(|c, input| {
                async move {
                    let id = require_str(&input, "client_id")?;
                    let secret = require_str(&input, "client_secret")?;
                    c.keep_google_client(id, secret).await
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

    /// "Sign in for me" is two calls: opening the page only reads, and the
    /// wait for the person is a call the gate puts to them. Neither keeps a
    /// screenshot of the page the person signs in to.
    #[test]
    fn sign_in_opens_as_a_read_and_waits_as_a_call_the_person_answers() {
        let names: Vec<&str> = specs().iter().map(|s| s.name).collect();
        assert!(names.contains(&"browser.sign_in") && names.contains(&"browser.signed_in"));
        assert!(READS.contains(&"browser.sign_in"));
        assert!(!READS.contains(&"browser.signed_in"));
        for tool in [
            "browser.sign_in",
            "browser.signed_in",
            "browser.fill",
            "browser.type",
        ] {
            assert!(super::super::client::no_screenshot(tool), "{tool}");
        }
        assert!(!super::super::client::no_screenshot("browser.click"));
        assert!(super::super::client::SIGN_IN_NEXT.contains("browser.signed_in"));
    }

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

    /// While the person signs in, every other browser call on that session
    /// is refused, screenshots and snapshots included; the wait isn't.
    #[tokio::test]
    async fn while_the_person_signs_in_the_browser_takes_no_other_call() {
        use workforce_agent::runner::{CURRENT_AGENT_ID, CURRENT_USER_ID};
        let skill =
            super::super::BrowserSkill::new(super::super::BrowserSkillConfig::default()).unwrap();
        let tools = skill.tools();
        let run = |name: &'static str| {
            let tool = tools.iter().find(|t| t.name() == name).unwrap().clone();
            CURRENT_AGENT_ID.scope(
                "agent".to_string(),
                CURRENT_USER_ID.scope("signing-person".to_string(), async move {
                    tool.execute(json!({})).await.unwrap()
                }),
            )
        };
        super::super::client::start_signing_for_test("agent", "signing-person");
        for name in [
            "browser.screenshot",
            "browser.snapshot",
            "browser.click",
            "browser.sign_in",
        ] {
            let out = run(name).await;
            assert!(!out.success, "{name}");
            assert_eq!(
                out.error.as_deref(),
                Some(super::super::client::SIGNING_IN_NOW),
                "{name}"
            );
        }
        let wait_held = CURRENT_AGENT_ID.sync_scope("agent".to_string(), || {
            CURRENT_USER_ID.sync_scope("signing-person".to_string(), || {
                held_by_sign_in("browser.signed_in")
            })
        });
        assert!(!wait_held, "the wait itself is let through");
        super::super::client::sign_in_ended("agent", "signing-person");
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
        // The seven that change a page, the sign-in card's wait, and
        // keeping a Google client.
        assert_eq!(READS.len() + 9, names.len());
    }

    #[test]
    fn a_fills_card_shows_the_field_and_the_value_as_written() {
        let skill =
            super::super::BrowserSkill::new(super::super::BrowserSkillConfig::default()).unwrap();
        let fill = skill
            .tools()
            .into_iter()
            .find(|t| t.name() == "browser.fill")
            .unwrap();
        assert_eq!(
            fill.approval_summary(&json!({"selector": "#name", "value": "Sam"}))
                .as_deref(),
            Some("Fill #name with: Sam")
        );
        let marker = "[[secret:7f3a type=password preview=hu…]]";
        assert_eq!(
            fill.approval_summary(&json!({"selector": "#pw", "value": marker})),
            Some(format!("Fill #pw with: {marker}"))
        );
        let vault = "[[vault:agent:site_password]]";
        assert!(fill
            .approval_summary(&json!({"selector": "#pw", "value": vault}))
            .unwrap()
            .ends_with(vault));
        let token = format!("lin_api_{}", "a1".repeat(15));
        let shown = fill
            .approval_summary(&json!({"selector": "#k", "value": token}))
            .unwrap();
        assert!(!shown.contains(&token), "{shown}");
        let long = "x".repeat(500);
        let shown = fill
            .approval_summary(&json!({"selector": "#a", "value": long}))
            .unwrap();
        assert!(shown.ends_with('…') && shown.chars().filter(|c| *c == 'x').count() == 200);
        let click = skill
            .tools()
            .into_iter()
            .find(|t| t.name() == "browser.click")
            .unwrap();
        assert_eq!(click.approval_summary(&json!({"selector": "#a"})), None);
    }
}
