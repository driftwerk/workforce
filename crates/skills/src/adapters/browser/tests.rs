//! The skill against a stand-in for Desk's extension: pages by address, and
//! what each tab did. The extension itself is tested in Desk's repo, in
//! Chrome for Testing, over the same protocol (`extension/protocol/v1.json`).

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};

use async_trait::async_trait;
use eyre::{eyre, Result};
use serde_json::{json, Value};

use workforce_agent::runner::{CURRENT_AGENT_ID, CURRENT_SESSION_KEY, CURRENT_USER_ID};

use super::*;

/// A page the stand-in serves.
#[derive(Clone, Default)]
struct Page {
    title: &'static str,
    text: String,
    /// What `shown` reads of an element, by selector.
    shown: HashMap<&'static str, &'static str>,
    /// A key a `capture_key` read finds, by selector: `Ok(value)` or the
    /// extension's refusal.
    keys: HashMap<&'static str, std::result::Result<&'static str, &'static str>>,
    /// Where a click takes the tab, by selector.
    goes: HashMap<&'static str, &'static str>,
    /// Where opening it lands instead: a redirect.
    redirect: Option<&'static str>,
}

#[derive(Default)]
struct World {
    pages: HashMap<String, Page>,
    /// Each tab's address, by session.
    tabs: HashMap<String, String>,
    /// Every call, `cmd session`.
    calls: Vec<String>,
    /// What fills and types put in.
    typed: Vec<String>,
    connected: bool,
}

static WORLD: LazyLock<Mutex<World>> = LazyLock::new(|| {
    Mutex::new(World {
        connected: true,
        ..World::default()
    })
});

struct Stand;

#[async_trait]
impl ExtensionLink for Stand {
    async fn call(&self, cmd: &str, args: Value) -> Result<Value> {
        let mut w = WORLD.lock().unwrap();
        let session = args["session"].as_str().unwrap_or_default().to_string();
        w.calls.push(format!("{cmd} {session}"));
        let url = w
            .tabs
            .get(&session)
            .cloned()
            .unwrap_or_else(|| "about:blank".into());
        let page = w.pages.get(&url).cloned().unwrap_or_default();
        Ok(match cmd {
            "navigate" => {
                let mut to = args["url"].as_str().unwrap_or_default().to_string();
                if let Some(r) = w.pages.get(&to).and_then(|p| p.redirect) {
                    to = r.to_string();
                }
                let title = w.pages.get(&to).map(|p| p.title).unwrap_or("");
                w.tabs.insert(session, to.clone());
                json!({ "url": to, "title": title })
            }
            "where" | "front" => json!({ "url": url, "title": page.title }),
            "read" => {
                json!({ "url": url, "title": page.title, "text": page.text, "html": format!("<p>{}</p>", page.text) })
            }
            "shown" => json!(page
                .shown
                .get(args["selector"].as_str().unwrap_or_default())
                .copied()
                .unwrap_or("")),
            "click" => {
                let sel = args["selector"].as_str().unwrap_or_default();
                if let Some(to) = page.goes.get(sel) {
                    w.tabs.insert(session, to.to_string());
                }
                json!(true)
            }
            "fill" | "type" => {
                // As the extension does: on the origin it was resolved for only.
                if args["origin"].as_str() != origin_of(&url).as_deref() {
                    return Err(eyre!("The page moved, so nothing was typed."));
                }
                let v = args["value"]
                    .as_str()
                    .or(args["text"].as_str())
                    .unwrap_or_default();
                w.typed.push(v.to_string());
                json!(true)
            }
            "press" | "select" | "wait" | "upload" => json!(true),
            "capture" => {
                // What it pictured, read in the same moment.
                json!({ "png_base64": "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==", "url": url, "text": page.text })
            }
            "capture_key" => match page.keys.get(args["selector"].as_str().unwrap_or_default()) {
                Some(Ok(v)) => {
                    json!({ "value": v, "origin": "https://keys.example", "path": "/settings/api" })
                }
                Some(Err(why)) => return Err(eyre!("Desk didn't read a key there: {why}.")),
                None => return Err(eyre!("Desk didn't read a key there: missing.")),
            },
            "close" => {
                w.tabs.remove(&session);
                json!({ "closed": true })
            }
            other => return Err(eyre!("unknown command {other}")),
        })
    }

    fn connected(&self) -> bool {
        WORLD.lock().unwrap().connected
    }
}

/// What the stand-in hub does: holds paying below unbound, records, keeps
/// keys.
struct TestHooks {
    done: Mutex<Vec<Done>>,
    kept: Mutex<Vec<(String, String, String, String)>>,
}

impl BrowserHooks for TestHooks {
    fn unbound(&self, agent: &str) -> bool {
        agent == "unbound"
    }
    fn holds(&self, _agent: &str, what: &OnPage) -> Option<String> {
        what.shown
            .contains("Pay")
            .then(|| "Paying is the person's to do.".to_string())
    }
    fn record(&self, done: Done) {
        self.done.lock().unwrap().push(done);
    }
    fn may_visit(&self, _agent: &str, origin: &str) -> Result<(), String> {
        match origin {
            "https://elsewhere.example" => Err("not allowed this session".into()),
            _ => Ok(()),
        }
    }
    fn keep_key(
        &self,
        user: &str,
        conversation: &str,
        connector: &str,
        key: &str,
    ) -> Result<String, String> {
        self.kept.lock().unwrap().push((
            user.into(),
            conversation.into(),
            connector.into(),
            key.into(),
        ));
        Ok("passed".into())
    }
}

static HOOKS: LazyLock<Arc<TestHooks>> = LazyLock::new(|| {
    Arc::new(TestHooks {
        done: Mutex::new(Vec::new()),
        kept: Mutex::new(Vec::new()),
    })
});

/// One test at a time: the link, the hooks and the stand-in's world are
/// the process's.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn setup() -> tokio::sync::MutexGuard<'static, ()> {
    let held = SERIAL.lock().await;
    let _ = install_link(Arc::new(Stand));
    let _ = install_hooks(HOOKS.clone());
    WORLD.lock().unwrap().connected = true;
    held
}

fn page(url: &str, p: Page) {
    WORLD.lock().unwrap().pages.insert(url.to_string(), p);
}

async fn run(
    agent: &str,
    user: &str,
    tool: &str,
    input: Value,
) -> workforce_agent::tools::ToolResult {
    // One skill, as a hub keeps one: its tabs outlive each call.
    static SKILL: LazyLock<BrowserSkill> =
        LazyLock::new(|| BrowserSkill::new(BrowserSkillConfig::default()).unwrap());
    let t = SKILL
        .tools()
        .into_iter()
        .find(|t| t.name() == tool)
        .unwrap();
    CURRENT_AGENT_ID
        .scope(
            agent.to_string(),
            CURRENT_USER_ID.scope(
                user.to_string(),
                CURRENT_SESSION_KEY.scope("conv-1".into(), async move {
                    // As the gate does before a call: what it acts on.
                    let _ = t.effect(&input);
                    t.execute(input).await.unwrap()
                }),
            ),
        )
        .await
}

#[test]
fn definition_lists_all_tools_with_allow_policies() {
    let def = BrowserSkill::definition();
    let names = BrowserSkill::tool_names();
    assert_eq!(def.tools.len(), names.len());
    for p in &def.default_policies {
        assert_eq!(p.action, "allow");
    }
}

#[tokio::test]
async fn a_page_reads_fenced_in_the_agents_own_tab() {
    let _serial = setup().await;
    page(
        "https://shop.example/cart",
        Page {
            title: "Cart",
            text: "Two items".into(),
            ..Page::default()
        },
    );
    let opened = run(
        "allan",
        "sarah",
        "browser.navigate",
        json!({ "url": "https://shop.example/cart" }),
    )
    .await;
    assert!(opened.success, "{:?}", opened.error);
    let read = run("allan", "sarah", "browser.snapshot", json!({})).await;
    assert!(read.success, "{:?}", read.error);
    let text = read.output["body_text"].as_str().unwrap();
    assert!(
        text.contains("Two items") && text.contains("EXTERNAL_UNTRUSTED_CONTENT"),
        "{text}"
    );
    assert_eq!(read.output["_security"]["source"], "browser:shop.example");
    // Another person's tab is another tab.
    let other = run("allan", "tom", "browser.snapshot", json!({})).await;
    assert!(!other.output.to_string().contains("Two items"));
}

#[tokio::test]
async fn only_web_pages_and_blank_are_opened() {
    let _serial = setup().await;
    for bad in [
        "file:///etc/passwd",
        "chrome://settings",
        "javascript:alert(1)",
        "data:text/html,hi",
    ] {
        let out = run("allan", "sarah", "browser.navigate", json!({ "url": bad })).await;
        assert!(!out.success, "{bad}");
    }
}

#[tokio::test]
async fn the_persons_own_steps_are_held_back_below_unbound() {
    let _serial = setup().await;
    let mut shown = HashMap::new();
    shown.insert("#pay", "Pay now");
    page(
        "https://pay.example/",
        Page {
            title: "Pay",
            shown,
            ..Page::default()
        },
    );
    run(
        "held",
        "sarah",
        "browser.navigate",
        json!({ "url": "https://pay.example/" }),
    )
    .await;
    let held = run(
        "held",
        "sarah",
        "browser.click",
        json!({ "selector": "#pay" }),
    )
    .await;
    assert_eq!(held.error.as_deref(), Some("Paying is the person's to do."));
    run(
        "unbound",
        "sarah",
        "browser.navigate",
        json!({ "url": "https://pay.example/" }),
    )
    .await;
    let ran = run(
        "unbound",
        "sarah",
        "browser.click",
        json!({ "selector": "#pay" }),
    )
    .await;
    assert!(ran.success, "{:?}", ran.error);
}

#[tokio::test]
async fn what_is_done_is_recorded_with_a_picture_but_never_of_a_secret() {
    let _serial = setup().await;
    page(
        "https://ok.example/",
        Page {
            title: "Ok",
            text: "Nothing secret".into(),
            ..Page::default()
        },
    );
    run(
        "rec",
        "sarah",
        "browser.navigate",
        json!({ "url": "https://ok.example/" }),
    )
    .await;
    let secret = concat!("Your token ghp_", "abcdefghijklmnopqrstuvwxyz0123456789AB");
    page(
        "https://tok.example/",
        Page {
            title: "Token",
            text: secret.into(),
            ..Page::default()
        },
    );
    run(
        "rec",
        "sarah",
        "browser.navigate",
        json!({ "url": "https://tok.example/" }),
    )
    .await;
    {
        let done = HOOKS.done.lock().unwrap();
        let mine: Vec<&Done> = done.iter().filter(|d| d.agent == "rec").collect();
        assert!(mine[0].screenshot.is_some(), "an ordinary page is pictured");
        assert!(
            mine[1].screenshot.is_none(),
            "a page showing a secret isn't"
        );
    }
    let shot = run("rec", "sarah", "browser.screenshot", json!({})).await;
    assert_eq!(shot.error.as_deref(), Some(client::SHOWS_A_SECRET));
}

#[tokio::test]
async fn with_chrome_not_connected_every_tool_says_so_plainly() {
    let _serial = setup().await;
    WORLD.lock().unwrap().connected = false;
    let out = run("lonely", "sarah", "browser.snapshot", json!({})).await;
    WORLD.lock().unwrap().connected = true;
    assert_eq!(out.error.as_deref(), Some(NOT_CONNECTED));
}

#[tokio::test]
async fn a_new_key_goes_to_the_secret_path_and_never_to_the_model() {
    let _serial = setup().await;
    keys::register(keys::fixture_page());
    let key = concat!("fx_", "abcdefghijklmnopqrstuvwx");
    let mut k = HashMap::new();
    k.insert("#key", Ok(key));
    k.insert("#field", Err("an editable field"));
    page(
        "https://keys.example/settings/api",
        Page {
            title: "API keys",
            text: format!("Your new key {key} Copy it now."),
            keys: k,
            ..Page::default()
        },
    );
    run(
        "setup",
        "sarah",
        "browser.navigate",
        json!({ "url": "https://keys.example/settings/api" }),
    )
    .await;
    // Reading the page never shows the key.
    let read = run("setup", "sarah", "browser.snapshot", json!({})).await;
    assert!(
        !read.output.to_string().contains("abcdefghijklmnop"),
        "{}",
        read.output
    );
    assert!(read.output.to_string().contains(keys::HIDDEN));
    // No picture of a key page.
    let shot = run("setup", "sarah", "browser.screenshot", json!({})).await;
    assert!(!shot.success);
    // The key goes to the hub, and the model learns only that it was kept.
    let kept = run(
        "setup",
        "sarah",
        keys::CAPTURE_KEY,
        json!({ "selector": "#key", "connector": "fixture" }),
    )
    .await;
    assert!(kept.success, "{:?}", kept.error);
    assert!(
        !kept.output.to_string().contains("abcdefghijklmnop"),
        "{}",
        kept.output
    );
    assert_eq!(kept.output["length"], key.chars().count());
    assert!(kept.output.get("ends").is_none());
    assert_eq!(kept.output["test"], "passed");
    assert!(HOOKS.kept.lock().unwrap().contains(&(
        "sarah".into(),
        "conv-1".into(),
        "fixture".into(),
        key.into()
    )));
    // A selector that tests what a field holds would read the key a
    // character at a time: refused on a key page, by every call.
    for (tool, input) in [
        (
            "browser.click",
            json!({ "selector": "input[value^=\"fx_a\"]" }),
        ),
        ("browser.wait", json!({ "selector": "#key[value^=fx_b]" })),
        (
            keys::CAPTURE_KEY,
            json!({ "selector": "input[v\\61lue^=fx]", "connector": "fixture" }),
        ),
    ] {
        let probe = run("setup", "sarah", tool, input).await;
        assert!(
            probe
                .error
                .as_deref()
                .is_some_and(|e| e.contains("tests a field's value")),
            "{tool}: {:?}",
            probe.error
        );
    }
    // An editable field, an unknown connector, or another page: nothing.
    let field = run(
        "setup",
        "sarah",
        keys::CAPTURE_KEY,
        json!({ "selector": "#field", "connector": "fixture" }),
    )
    .await;
    assert!(field.error.unwrap().contains("an editable field"));
    let other = run(
        "setup",
        "sarah",
        keys::CAPTURE_KEY,
        json!({ "selector": "#key", "connector": "nope" }),
    )
    .await;
    assert!(!other.success);
    page("https://shop.example/", Page::default());
    run(
        "setup",
        "sarah",
        "browser.navigate",
        json!({ "url": "https://shop.example/" }),
    )
    .await;
    let away = run(
        "setup",
        "sarah",
        keys::CAPTURE_KEY,
        json!({ "selector": "#key", "connector": "fixture" }),
    )
    .await;
    assert!(away.error.unwrap().contains("isn't on fixture's key page"));
    // The activity row keeps no picture of it.
    let done = HOOKS.done.lock().unwrap();
    let row = done.iter().find(|d| d.tool == keys::CAPTURE_KEY).unwrap();
    assert!(row.screenshot.is_none());
}

#[tokio::test]
async fn sign_in_puts_the_page_in_front_and_keeps_only_its_origin_and_path() {
    let _serial = setup().await;
    page(
        "https://login.example/start",
        Page {
            title: "Sign in",
            ..Page::default()
        },
    );
    let opened = run(
        "signer",
        "sarah",
        "browser.sign_in",
        json!({ "url": "https://login.example/start?next=%2Fhome" }),
    )
    .await;
    assert!(opened.success, "{:?}", opened.error);
    assert!(WORLD
        .lock()
        .unwrap()
        .calls
        .iter()
        .any(|c| c.starts_with("front ")));
    let busy = run("signer", "sarah", "browser.snapshot", json!({})).await;
    assert_eq!(busy.error.as_deref(), Some(client::SIGNING_IN_NOW));
    let done = run("signer", "sarah", "browser.signed_in", json!({})).await;
    assert!(done.success, "{:?}", done.error);
    assert!(!done.output.to_string().contains("next="));
    let again = run("signer", "sarah", "browser.signed_in", json!({})).await;
    assert!(again.error.unwrap().contains("call browser.sign_in first"));
    let rows = HOOKS.done.lock().unwrap();
    for d in rows.iter().filter(|d| d.agent == "signer") {
        assert!(d.screenshot.is_none(), "{}", d.tool);
        assert!(!d.url.contains('?'), "{}", d.url);
    }
}

#[tokio::test]
async fn a_click_takes_the_tab_where_it_leads() {
    let _serial = setup().await;
    let mut goes = HashMap::new();
    goes.insert("#next", "https://next.example/");
    page(
        "https://first.example/",
        Page {
            title: "First",
            goes,
            ..Page::default()
        },
    );
    page(
        "https://next.example/",
        Page {
            title: "Next",
            ..Page::default()
        },
    );
    run(
        "walker",
        "sarah",
        "browser.navigate",
        json!({ "url": "https://first.example/" }),
    )
    .await;
    let clicked = run(
        "walker",
        "sarah",
        "browser.click",
        json!({ "selector": "#next" }),
    )
    .await;
    assert!(clicked.success, "{:?}", clicked.error);
    let read = run("walker", "sarah", "browser.snapshot", json!({})).await;
    assert!(read.output["url"]
        .as_str()
        .unwrap()
        .contains("next.example"));
}

#[tokio::test]
async fn stop_closes_the_tab_and_live_lists_only_open_ones() {
    let _serial = setup().await;
    page(
        "https://live.example/",
        Page {
            title: "Live",
            ..Page::default()
        },
    );
    run(
        "stopper",
        "sarah",
        "browser.navigate",
        json!({ "url": "https://live.example/" }),
    )
    .await;
    assert!(live()
        .await
        .iter()
        .any(|l| l.agent == "stopper" && l.host.as_deref() == Some("live.example")));
    assert!(close(Which::Agent("stopper")).await >= 1);
    assert!(!live().await.iter().any(|l| l.agent == "stopper"));
}

/// Where a navigation lands passes the gate before anything of it is said:
/// a redirect to a site the session may not visit, or to the hub's own
/// port on this computer, is left at once and its address goes unsaid.
#[tokio::test]
async fn a_navigation_that_lands_somewhere_it_may_not_be_says_nothing_of_it() {
    let _serial = setup().await;
    page(
        "https://bounce.example/",
        Page {
            redirect: Some("https://elsewhere.example/secret-path"),
            ..Page::default()
        },
    );
    page(
        "https://elsewhere.example/secret-path",
        Page {
            title: "Private title",
            ..Page::default()
        },
    );
    let went = run(
        "lander",
        "sarah",
        "browser.navigate",
        json!({ "url": "https://bounce.example/" }),
    )
    .await;
    let error = went.error.unwrap();
    assert!(error.contains("hasn't been allowed"), "{error}");
    assert!(!error.contains("secret-path") && !error.contains("Private title"));
    assert!(went.output.is_null() || !went.output.to_string().contains("elsewhere"));
    let at = run("lander", "sarah", "browser.snapshot", json!({})).await;
    let shown = at.output["url"].as_str().unwrap_or_default();
    assert!(shown.contains("about:blank"), "{}", at.output);
    // The hub's own port here is never a page Desk opens.
    keep_off_ports(vec![50148]);
    let own = run(
        "lander",
        "sarah",
        "browser.navigate",
        json!({ "url": "http://127.0.0.1:50148/api/desk/browser" }),
    )
    .await;
    assert!(!own.success);
    assert!(!own.error.unwrap().contains("50148"));
    keep_off_ports(Vec::new());
}

/// A fill is typed only on the origin it was resolved for: the extension is
/// told it, and refuses a page that moved.
#[tokio::test]
async fn a_fill_names_the_origin_it_was_resolved_for() {
    let _serial = setup().await;
    page("https://form.example/", Page::default());
    run(
        "filler",
        "sarah",
        "browser.navigate",
        json!({ "url": "https://form.example/" }),
    )
    .await;
    let filled = run(
        "filler",
        "sarah",
        "browser.fill",
        json!({ "selector": "#q", "value": "hello" }),
    )
    .await;
    assert!(filled.success, "{:?}", filled.error);
    assert!(WORLD.lock().unwrap().typed.contains(&"hello".to_string()));
}
