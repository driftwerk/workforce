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

    /// A browser call, as Desk's gate keeps it: under the origin it acts on
    /// in the person's Chrome (`chrome:https://shop.example`), the one the
    /// page is on now or, for opening a site, the one it goes to
    /// (`chrome-visit:…`); and that origin is what the call then acts on,
    /// or not at all.
    fn effect(&self, input: &Value) -> workforce_core::computer::Effect {
        use super::client::{call_key, expect, origin_now, Decided};
        use workforce_core::computer::{Effect, SIGNED_IN};
        let navigating = workforce_core::computer::navigates(self.spec.name);
        let key = call_key(self.spec.name, input);
        let origin = match navigating {
            true => super::address::origin_of(input["url"].as_str().unwrap_or_default()),
            false => origin_now(),
        };
        // The wait for the person's sign-in acts on no page: where it ends
        // up (an SSO hop included) is read after they answer.
        let decision = match navigating {
            _ if self.spec.name == SIGNED_IN => None,
            true => Some(Decided::Visit(origin.clone())),
            false => Some(Decided::Origin(origin.clone())),
        };
        expect(&key, decision);
        Effect::Connector {
            connector: site_key(navigating, origin.as_deref()),
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
    super::extension::OPEN_FOR_ME,
    "browser.sign_in",
    "browser.snapshot",
    "browser.screenshot",
    "browser.get_content",
    "browser.wait",
];

/// What Desk's gate keeps a browser call under, in the person's Chrome:
/// `chrome:<origin>`, and for opening a site `chrome-visit:<origin>`. A page
/// with no origin of its own (blank) is kept under `chrome` alone, which no
/// site's allowance is shared with.
pub fn site_key(navigating: bool, origin: Option<&str>) -> String {
    let kind = if navigating { "chrome-visit" } else { "chrome" };
    match origin.filter(|o| !o.is_empty()) {
        Some(o) => format!("{kind}:{o}"),
        None => "chrome".into(),
    }
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
                "Browse for yourself: open a web page in your own tab of the person's Chrome (signed in as them), to read it or act on it with the other browser tools. Your tab is in Desk's window, which may be out of the person's sight, and it closes when your turn ends. To show the person a page (\"open X for me\", \"show me X\", \"open my Chrome to X\"), use browser.open_for_me instead. Returns once it has loaded.",
            schema: json!({
                "type": "object",
                "properties": {
                    "url": { "type": "string", "description": "The page (https://…), or about:blank" }
                },
                "required": ["url"]
            }),
            handler: handler(|c, input| {
                async move { c.navigate(require_str(&input, "url")?).await }.boxed()
            }),
        },
        ToolSpec {
            name: super::extension::OPEN_FOR_ME,
            description:
                "Open a page for the person to see: use it when they ask you to open, show or pull up a page for them (\"open X for me\", \"show me X\", \"open my Chrome to X\"). It opens as the front tab of their current Chrome window, signed in as them, and stays open after your turn. It is theirs from then on: you can't read or act on it, so open it last, once you've done any browsing of your own with browser.navigate. Succeeds only once Chrome confirms it is in front of them; if it fails, tell them plainly it wasn't shown.",
            schema: json!({
                "type": "object",
                "properties": {
                    "url": { "type": "string", "description": "The page to show them (https://…)" }
                },
                "required": ["url"]
            }),
            handler: handler(|c, input| {
                async move { c.open_for_person(require_str(&input, "url")?).await }.boxed()
            }),
        },
        ToolSpec {
            name: "browser.sign_in",
            description:
                "Sign in for me: when a site needs the person to sign in (a password, a code, a passkey), open its sign-in page and put it in front of them in their Chrome, for them to sign in themselves. Then call browser.signed_in, which waits for them. Never ask for, read or type their password.",
            schema: json!({
                "type": "object",
                "properties": {
                    "url": { "type": "string", "description": "The site's sign-in page (https)" }
                },
                "required": ["url"]
            }),
            handler: handler(|c, input| {
                async move { c.sign_in(require_str(&input, "url")?).await }.boxed()
            }),
        },
        ToolSpec {
            name: "browser.signed_in",
            description:
                "After browser.sign_in: shows the person a card and waits while they sign in. Returns where the page is once they say they're done; if they cancel or don't answer, tell them it wasn't done.",
            schema: json!({ "type": "object", "properties": {} }),
            handler: handler(|c, _input| async move { c.signed_in().await }.boxed()),
        },
        ToolSpec {
            name: "browser.snapshot",
            description: "Return the current page's address, title and visible text.",
            schema: json!({ "type": "object", "properties": {} }),
            handler: handler(|c, _input| async move { c.snapshot().await }.boxed()),
        },
        ToolSpec {
            name: "browser.screenshot",
            description:
                "Capture a PNG screenshot of the current page. The system stages the image and delivers it to the originating \
                 conversation automatically — you do not see the attachment and you do not need \
                 to call any upload tool. Just briefly tell the user what's on screen. No picture is taken of a page showing a secret.",
            schema: json!({ "type": "object", "properties": {} }),
            handler: handler(|c, _input| async move { c.screenshot().await }.boxed()),
        },
        ToolSpec {
            name: "browser.click",
            description: "Click the element a CSS selector names, where it shows on the page.",
            schema: json!({
                "type": "object",
                "properties": {
                    "selector": { "type": "string", "description": "CSS selector" }
                },
                "required": ["selector"]
            }),
            handler: handler(|c, input| {
                async move { c.click(require_str(&input, "selector")?).await }.boxed()
            }),
        },
        ToolSpec {
            name: "browser.fill",
            description:
                "Set an input field's value, as typing it would leave it. For a credential, put a [[secret:…]] or [[vault:…]] marker in value: it is put in at the page, never shown to you.",
            schema: json!({
                "type": "object",
                "properties": {
                    "selector": { "type": "string", "description": "CSS selector of the field" },
                    "value": { "type": "string", "description": "What goes in it" }
                },
                "required": ["selector", "value"]
            }),
            handler: handler(|c, input| {
                async move {
                    let selector = require_str(&input, "selector")?;
                    let value = input["value"].as_str().unwrap_or_default();
                    c.fill(selector, value).await
                }
                .boxed()
            }),
        },
        ToolSpec {
            name: "browser.type",
            description:
                "Type text into the field a selector names (it must take the focus), a character at a time, as a person does.",
            schema: json!({
                "type": "object",
                "properties": {
                    "selector": { "type": "string", "description": "CSS selector of the field" },
                    "text": { "type": "string", "description": "What to type" }
                },
                "required": ["selector", "text"]
            }),
            handler: handler(|c, input| {
                async move {
                    let selector = require_str(&input, "selector")?;
                    let text = input["text"].as_str().unwrap_or_default();
                    c.type_text(selector, text).await
                }
                .boxed()
            }),
        },
        ToolSpec {
            name: "browser.press_key",
            description:
                "Press a key (Enter, Tab, Escape, an arrow, or a character) on the element a selector names, or the focused one.",
            schema: json!({
                "type": "object",
                "properties": {
                    "key": { "type": "string", "description": "The key: Enter, Tab, Escape, ArrowDown, or a character" },
                    "selector": { "type": "string", "description": "CSS selector to focus first" }
                },
                "required": ["key"]
            }),
            handler: handler(|c, input| {
                async move {
                    let key = require_str(&input, "key")?;
                    let selector = input["selector"].as_str().filter(|s| !s.is_empty());
                    c.press_key(key, selector).await
                }
                .boxed()
            }),
        },
        ToolSpec {
            name: "browser.select_option",
            description: "Choose the option of a <select> whose value or text is the one given.",
            schema: json!({
                "type": "object",
                "properties": {
                    "selector": { "type": "string", "description": "CSS selector of the <select>" },
                    "value": { "type": "string", "description": "The option's value or text" }
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
                "Give a file input on the page a file staged in this conversation (an artifact), as choosing it would. The bytes never pass through you.",
            schema: json!({
                "type": "object",
                "properties": {
                    "selector": { "type": "string", "description": "CSS selector of the <input type=\"file\">" },
                    "artifact_id": { "type": "string", "description": "The staged file's artifact id" },
                    "filename": { "type": "string", "description": "The name the site sees (defaults to the artifact's)" }
                },
                "required": ["selector", "artifact_id"]
            }),
            handler: handler(|c, input| {
                async move {
                    let selector = require_str(&input, "selector")?;
                    let artifact_id = require_str(&input, "artifact_id")?;
                    let filename = input["filename"].as_str();
                    c.upload_file(selector, artifact_id, filename).await
                }
                .boxed()
            }),
        },
        ToolSpec {
            name: "browser.wait",
            description: "Wait for a CSS selector to appear; errors on timeout.",
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
                "Return page content. format='text' returns the page's visible text; format='html' (default) its HTML.",
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
            name: super::keys::CAPTURE_KEY,
            description:
                "While setting up a connection the person asked for: on the service's own key page, right after it shows a new API key, keep that key for the connection. You never see the key: pass the CSS selector of the element showing it, and the connector. Never screenshot, read out or type the key.",
            schema: json!({
                "type": "object",
                "properties": {
                    "selector": { "type": "string", "description": "CSS selector of the element showing the new key" },
                    "connector": { "type": "string", "description": "The connection being set up: linear, github, …" }
                },
                "required": ["selector", "connector"]
            }),
            handler: handler(|c, input| {
                async move {
                    let selector = require_str(&input, "selector")?;
                    let connector = require_str(&input, "connector")?;
                    c.capture_key(selector, connector).await
                }
                .boxed()
            }),
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
    fn sign_in_opens_as_a_read_and_waits_as_a_call_the_person_answers() {
        let names = tool_names();
        assert!(names.contains(&"browser.sign_in") && names.contains(&"browser.signed_in"));
        assert!(READS.contains(&"browser.sign_in"));
        assert!(!READS.contains(&"browser.signed_in"));
        assert!(super::super::client::SIGN_IN_NEXT.contains("browser.signed_in"));
    }

    #[test]
    fn nothing_that_runs_a_script_or_needs_the_debugger_is_left() {
        let names = tool_names();
        for gone in [
            "browser.evaluate",
            "browser.pdf",
            "browser.console_messages",
            "browser.network_requests",
        ] {
            assert!(!names.contains(&gone), "{gone}");
        }
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
        for (t, p) in tools.iter().zip(policies.iter()) {
            assert_eq!(t.name, p.tool);
            assert_eq!(p.action, "allow");
        }
        for spec in specs() {
            assert_eq!(spec.schema["type"], "object", "{}", spec.name);
            assert!(
                spec.schema["properties"].get("browser").is_none(),
                "one browser only"
            );
        }
    }

    #[test]
    fn reads_are_every_tool_that_changes_no_page() {
        let names = tool_names();
        for r in READS {
            assert!(names.contains(r), "{r}");
        }
        // Click, fill, type, press, select, upload, the sign-in card's wait,
        // and reading a new key.
        assert_eq!(READS.len() + 8, names.len());
    }

    #[test]
    fn a_site_is_kept_by_its_origin_and_a_page_with_none_shares_nothing() {
        assert_eq!(
            site_key(false, Some("https://shop.example")),
            "chrome:https://shop.example"
        );
        assert_eq!(
            site_key(true, Some("https://shop.example")),
            "chrome-visit:https://shop.example"
        );
        assert_eq!(site_key(false, None), "chrome");
        assert_eq!(site_key(true, Some("")), "chrome");
    }

    #[test]
    fn a_call_is_kept_under_the_origin_it_opens_or_acts_on() {
        use workforce_core::computer::Effect;
        let skill =
            super::super::BrowserSkill::new(super::super::BrowserSkillConfig::default()).unwrap();
        let tools = skill.tools();
        let find = |n: &str| tools.iter().find(|t| t.name() == n).unwrap().clone();
        assert_eq!(
            find("browser.navigate").effect(&json!({ "url": "https://shop.example/cart" })),
            Effect::Connector {
                connector: "chrome-visit:https://shop.example".into(),
                write: false
            }
        );
        assert_eq!(
            find("browser.open_for_me").effect(&json!({ "url": "https://staging.example.com/" })),
            Effect::Connector {
                connector: "chrome-visit:https://staging.example.com".into(),
                write: false
            }
        );
        assert_eq!(
            find("browser.sign_in").effect(&json!({ "url": "https://login.example/" })),
            Effect::Connector {
                connector: "chrome-visit:https://login.example".into(),
                write: false
            }
        );
        assert!(matches!(
            find("browser.click").effect(&json!({ "selector": "#save" })),
            Effect::Connector { write: true, .. }
        ));
    }

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
        super::super::client::start_signing(&super::super::client::SessionId::derive(
            "agent",
            "signing-person",
        ));
        for name in [
            "browser.screenshot",
            "browser.snapshot",
            "browser.click",
            "browser.sign_in",
        ] {
            let out = run(name).await;
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
    fn a_fills_card_shows_the_field_and_the_value_as_written() {
        let summary = fill_summary(&json!({ "selector": "#name", "value": "Allan" }));
        assert_eq!(summary, "Fill #name with: Allan");
        let marker =
            fill_summary(&json!({ "selector": "#pw", "value": "[[vault:shop/password]]" }));
        assert!(marker.contains("[[vault:shop/password]]"));
        let long = fill_summary(&json!({ "selector": "#x", "value": "a".repeat(500) }));
        assert!(long.ends_with('…'));
    }
}
