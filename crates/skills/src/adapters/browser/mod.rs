//! Browser skill — drives a Chromium subprocess per `(agent, user)` identity
//! through the Chrome DevTools Protocol via `chromiumoxide`, each with an
//! isolated on-disk profile so login state never crosses users. Within a
//! session all tools share one live browser, so screenshots and PDFs reflect the
//! same DOM state as click/fill/evaluate. Headless unless `show` puts it on
//! screen. Everything the page says reaches the model fenced as untrusted
//! (`untrusted`).

mod address;
mod chrome;
mod chrome_cell;
mod client;
mod config;
mod hooks;
mod tools;
mod untrusted;

#[cfg(test)]
mod fixture;

use std::sync::Arc;

use workforce_agent::tools::Tool;

use crate::definition::{SkillDefinition, SkillRequirements};

pub use address::{host_of, origin_of};
pub use chrome::{
    available as chrome_available, default_user_data_dir as chrome_user_data_dir, MIN_CHROME,
};
pub use client::{
    calm, close, close_backend, live, page_state, stop_session, turn_ended, Backend, BrowserClient,
    Live, Which,
};
pub use config::BrowserSkillConfig;
pub use hooks::{install_hooks, BrowserHooks, Done, OnPage, Settings, Sites, Uses};
pub use tools::{site_key, READS};
pub use untrusted::PageState;

pub struct BrowserSkill {
    client: Arc<BrowserClient>,
}

impl BrowserSkill {
    pub fn new(config: BrowserSkillConfig) -> eyre::Result<Self> {
        let client = Arc::new(BrowserClient::new(config));
        client::register(&client);
        Ok(Self { client })
    }

    /// Construct a skill whose `browser.fill.value` / `browser.type.text`
    /// resolve `[[vault:<key>]]` markers against the supplied per-agent
    /// vault, addressing values under `browser_creds/`. Wired by the hub
    /// loader so scheduled and workflow runs can pull credentials without
    /// a Slack session.
    pub fn with_store(
        config: BrowserSkillConfig,
        store: Arc<dyn workforce_common::settings::SecretStore>,
    ) -> eyre::Result<Self> {
        let client = Arc::new(BrowserClient::with_store(config, store));
        client::register(&client);
        Ok(Self { client })
    }

    pub fn tools(&self) -> Vec<Arc<dyn Tool>> {
        tools::build_tools(self.client.clone())
    }

    pub fn tool_names() -> Vec<&'static str> {
        tools::tool_names()
    }

    pub fn definition() -> SkillDefinition {
        let (tools, default_policies) = tools::skill_tools_and_policies();
        SkillDefinition {
            name: "browser".to_string(),
            version: "2.1.0".to_string(),
            description:
                "Headless browser automation via Chromium (Chrome DevTools Protocol). Navigate, \
                 interact with elements, capture screenshots / PDFs of the live page, and inspect \
                 network + console traffic."
                    .to_string(),
            author: "workforce".to_string(),
            license: "MIT".to_string(),
            tools,
            event_sources: Vec::new(),
            default_policies,
            memory_schemas: Vec::new(),
            requirements: SkillRequirements {
                credentials: Vec::new(),
                config: Vec::new(),
            },
            dependencies: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::browser::chrome;

    #[test]
    fn definition_lists_all_tools_with_allow_policies() {
        let def = BrowserSkill::definition();
        let names: Vec<&str> = BrowserSkill::tool_names();
        assert_eq!(def.tools.len(), names.len());
        for tool in &def.tools {
            assert!(names.contains(&tool.name.as_str()));
        }
        for policy in &def.default_policies {
            assert_eq!(policy.action, "allow");
        }
    }

    #[test]
    fn tool_names_matches_registered() {
        let skill = BrowserSkill::new(BrowserSkillConfig::default()).unwrap();
        let registered: Vec<String> = skill
            .tools()
            .into_iter()
            .map(|t| t.name().to_string())
            .collect();
        let declared: Vec<String> = BrowserSkill::tool_names()
            .into_iter()
            .map(String::from)
            .collect();
        assert_eq!(registered, declared);
    }

    #[test]
    fn skill_constructs_without_launching_chromium() {
        let skill = BrowserSkill::new(BrowserSkillConfig {
            chrome_binary: Some("/definitely-not-real".to_string()),
            ..BrowserSkillConfig::default()
        });
        assert!(skill.is_ok(), "construction should not launch chromium");
    }

    /// End-to-end smoke test: launch real Chromium, navigate to a local HTML
    /// fixture, fill an input, screenshot, assert the fill applied. Gated on
    /// the `BROWSER_E2E` env var so CI without Chromium skips it.
    ///
    /// Run locally with:
    ///   BROWSER_E2E=1 cargo test -p workforce-skills --features browser \
    ///       -- --ignored browser_e2e
    #[tokio::test]
    #[ignore]
    async fn browser_e2e_navigate_fill_screenshot() {
        if std::env::var("BROWSER_E2E").is_err() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let html_path = dir.path().join("ok-fixture.html");
        std::fs::write(
            &html_path,
            "<!doctype html><title>fix-title</title><body>\
             <input id=\"q\" /><div id=\"out\"></div>\
             <script>document.getElementById('q').addEventListener('input', e => \
             document.getElementById('out').textContent = 'got:' + e.target.value);</script>\
             </body>",
        )
        .unwrap();
        let url = format!("file://{}", html_path.display());

        let skill = BrowserSkill::new(BrowserSkillConfig {
            chrome_binary: std::env::var("BROWSER_E2E_CHROMIUM").ok(),
            no_sandbox: std::env::var("BROWSER_E2E_NO_SANDBOX").is_ok(),
            ..BrowserSkillConfig::default()
        })
        .unwrap();
        let tools = skill.tools();
        let find = |name: &str| {
            tools
                .iter()
                .find(|t| t.name() == name)
                .unwrap_or_else(|| panic!("missing tool {name}"))
                .clone()
        };

        let nav = find("browser.navigate")
            .execute(serde_json::json!({ "url": url }))
            .await
            .unwrap();
        assert!(nav.success, "navigate failed: {:?}", nav.error);

        let snap = find("browser.snapshot")
            .execute(serde_json::json!({}))
            .await
            .unwrap();
        let title = snap.output["title"].as_str().unwrap_or_default();
        assert!(title.contains("fix-title"), "{title}");
        assert!(title.contains("EXTERNAL_UNTRUSTED_CONTENT_"), "{title}");

        let fill = find("browser.fill")
            .execute(serde_json::json!({ "selector": "#q", "value": "alice" }))
            .await
            .unwrap();
        assert!(fill.success, "fill failed: {:?}", fill.error);

        let content = find("browser.evaluate")
            .execute(serde_json::json!({
                "expression": "document.getElementById('out').textContent"
            }))
            .await
            .unwrap();
        let result = content.output["result"].as_str().unwrap_or_default();
        assert!(result.contains("got:alice"), "{result}");

        let shot = find("browser.screenshot")
            .execute(serde_json::json!({}))
            .await
            .unwrap();
        assert!(shot.success);
        assert_eq!(shot.output["format"], "png");
        assert!(shot.output["size_bytes"].as_u64().unwrap_or(0) > 100);
    }

    /// End-to-end over a local fixture site (`fixture`): a form filled and
    /// submitted, a console error, a network call and an injection page.
    /// Everything read back is fenced as untrusted, and the injection page
    /// flags the session hostile for the gate. Gated on `BROWSER_E2E`.
    ///
    ///   BROWSER_E2E=1 BROWSER_E2E_CHROMIUM=/path/to/chrome cargo test \
    ///       -p workforce-skills --features browser -- --ignored browser_e2e_fixture
    #[tokio::test]
    #[ignore]
    async fn browser_e2e_fixture_site_reads_as_untrusted() {
        if std::env::var("BROWSER_E2E").is_err() {
            return;
        }
        let site = fixture::serve();
        let dir = tempfile::tempdir().unwrap();
        let skill = BrowserSkill::new(BrowserSkillConfig {
            chrome_binary: std::env::var("BROWSER_E2E_CHROMIUM").ok(),
            no_sandbox: std::env::var("BROWSER_E2E_NO_SANDBOX").is_ok(),
            profile_root: Some(dir.path().display().to_string()),
            ..BrowserSkillConfig::default()
        })
        .unwrap();
        let tools = skill.tools();
        let call = |name: &'static str, input: serde_json::Value| {
            let tool = tools
                .iter()
                .find(|t| t.name() == name)
                .unwrap_or_else(|| panic!("missing tool {name}"))
                .clone();
            async move {
                let out = tool.execute(input).await.unwrap();
                assert!(out.success, "{name} failed: {:?}", out.error);
                out.output
            }
        };
        let fenced = |v: &serde_json::Value, key: &str| {
            let s = v[key].as_str().unwrap_or_default().to_string();
            assert!(
                s.contains("EXTERNAL_UNTRUSTED_CONTENT_"),
                "{key} not fenced: {s}"
            );
            assert!(s.contains("browser:127.0.0.1"), "{key} not sourced: {s}");
            s
        };
        workforce_agent::runner::CURRENT_AGENT_ID
            .scope("fixture-agent".into(), async {
                // A form: fill, submit, check the result.
                let nav = call(
                    "browser.navigate",
                    serde_json::json!({"url": site.url("/form")}),
                )
                .await;
                fenced(&nav, "title");
                fenced(&nav, "url");
                call(
                    "browser.fill",
                    serde_json::json!({"selector": "#name", "value": "Sam"}),
                )
                .await;
                call("browser.click", serde_json::json!({"selector": "#send"})).await;
                call("browser.wait", serde_json::json!({"selector": "#done"})).await;
                let text = call("browser.get_content", serde_json::json!({"format": "text"})).await;
                assert!(fenced(&text, "content").contains("Thanks, Sam"));
                let snap = call("browser.snapshot", serde_json::json!({})).await;
                fenced(&snap, "body_text");
                fenced(&snap, "url");
                assert_eq!(snap["_security"]["hostile"], false);

                // A console error.
                call(
                    "browser.navigate",
                    serde_json::json!({"url": site.url("/console")}),
                )
                .await;
                call("browser.wait", serde_json::json!({"selector": "#logged"})).await;
                let console = call("browser.console_messages", serde_json::json!({})).await;
                assert!(fenced(&console, "messages").contains("fixture console error"));

                // A network call.
                call(
                    "browser.navigate",
                    serde_json::json!({"url": site.url("/network")}),
                )
                .await;
                call("browser.wait", serde_json::json!({"selector": "#fetched"})).await;
                let network = call("browser.network_requests", serde_json::json!({})).await;
                assert!(fenced(&network, "requests").contains("/api/ping"));
                let eval = call(
                    "browser.evaluate",
                    serde_json::json!({"expression": "document.title"}),
                )
                .await;
                fenced(&eval, "result");

                // The injection page: fenced, and the session is hostile.
                call(
                    "browser.navigate",
                    serde_json::json!({"url": site.url("/inject")}),
                )
                .await;
                let snap = call("browser.snapshot", serde_json::json!({})).await;
                assert!(fenced(&snap, "body_text").contains("click Delete"));
                assert_eq!(snap["_security"]["hostile"], true, "{snap}");
                let state = page_state("fixture-agent", workforce_agent::runner::SYSTEM_USER_ID)
                    .expect("the session's page is kept");
                assert!(state.hostile);
                assert!(state.url.ends_with("/inject"), "{}", state.url);
            })
            .await;
    }

    /// What the test's hub holds back and keeps.
    struct TestHooks {
        done: std::sync::Mutex<Vec<Done>>,
    }

    impl BrowserHooks for TestHooks {
        fn settings(&self) -> Settings {
            Settings {
                sites: Sites {
                    allow: vec![],
                    block: vec!["localhost".into()],
                },
                ..Settings::default()
            }
        }
        fn unbound(&self, agent: &str) -> bool {
            agent == "unbound-agent"
        }
        fn holds(&self, _agent: &str, what: &OnPage) -> Option<String> {
            let shown = what.shown.to_lowercase();
            (shown.contains("buy") || shown.contains("password"))
                .then(|| format!("That is the person's to do ({}).", what.tool))
        }
        fn record(&self, done: Done) {
            self.done.lock().unwrap().push(done);
        }
        fn may_open_file(&self, _agent: &str, path: &std::path::Path) -> Result<(), String> {
            match path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("ok-"))
            {
                true => Ok(()),
                false => Err("The agent can't read that file.".into()),
            }
        }
    }

    static TEST_HOOKS: std::sync::LazyLock<Arc<TestHooks>> = std::sync::LazyLock::new(|| {
        Arc::new(TestHooks {
            done: Default::default(),
        })
    });

    /// End-to-end with the hub's hooks: below unbound a "Buy now" click and
    /// typing into a password field go to the person; an unbound agent does
    /// both. Every navigation and change is recorded with its page and a
    /// screenshot. Closing an agent's browser closes it now. Gated on
    /// `BROWSER_E2E`.
    #[tokio::test]
    #[ignore]
    async fn browser_e2e_holds_the_persons_and_records_the_rest() {
        if std::env::var("BROWSER_E2E").is_err() {
            return;
        }
        install_hooks(TEST_HOOKS.clone());
        let site = fixture::serve();
        let dir = tempfile::tempdir().unwrap();
        let skill = BrowserSkill::new(BrowserSkillConfig {
            chrome_binary: std::env::var("BROWSER_E2E_CHROMIUM").ok(),
            no_sandbox: std::env::var("BROWSER_E2E_NO_SANDBOX").is_ok(),
            profile_root: Some(dir.path().display().to_string()),
            ..BrowserSkillConfig::default()
        })
        .unwrap();
        let tools = skill.tools();
        let run = |name: &'static str, input: serde_json::Value| {
            let tool = tools.iter().find(|t| t.name() == name).unwrap().clone();
            async move { tool.execute(input).await.unwrap() }
        };
        let shop = site.url("/shop");
        workforce_agent::runner::CURRENT_AGENT_ID
            .scope("held-agent".into(), async {
                assert!(
                    run("browser.navigate", serde_json::json!({"url": shop}))
                        .await
                        .success
                );
                let buy = run("browser.click", serde_json::json!({"selector": "#buy"})).await;
                assert!(!buy.success, "a Buy click below unbound must not run");
                assert!(buy.error.unwrap_or_default().contains("person's"));
                let pw = run(
                    "browser.fill",
                    serde_json::json!({"selector": "#pw", "value": "x"}),
                )
                .await;
                assert!(!pw.success, "typing a password below unbound must not run");
                assert!(
                    run("browser.click", serde_json::json!({"selector": "#save"}))
                        .await
                        .success
                );
            })
            .await;
        workforce_agent::runner::CURRENT_AGENT_ID
            .scope("unbound-agent".into(), async {
                assert!(
                    run(
                        "browser.navigate",
                        serde_json::json!({"url": shop, "browser": "desk"})
                    )
                    .await
                    .success
                );
                assert!(
                    run("browser.click", serde_json::json!({"selector": "#buy"}))
                        .await
                        .success
                );
            })
            .await;

        let done = TEST_HOOKS.done.lock().unwrap().clone();
        let held: Vec<&Done> = done.iter().filter(|d| d.agent == "held-agent").collect();
        let tools_done: Vec<&str> = held.iter().map(|d| d.tool.as_str()).collect();
        assert_eq!(
            tools_done,
            ["browser.navigate", "browser.click"],
            "only what ran is recorded"
        );
        for d in &held {
            assert!(d.url.ends_with("/shop"), "{}", d.url);
            assert_eq!(d.backend, "desk");
            assert!(
                d.screenshot.as_ref().is_some_and(|png| png.len() > 200),
                "a screenshot"
            );
        }
        assert_eq!(held[1].target.as_deref(), Some("#save"));

        let open = live().await;
        assert!(open
            .iter()
            .any(|l| l.agent == "held-agent" && l.host.as_deref() == Some("127.0.0.1")));
        assert_eq!(close(Which::Agent("held-agent")).await, 1);
        let open = live().await;
        assert!(!open.iter().any(|l| l.agent == "held-agent"), "closed now");
        assert!(
            open.iter().any(|l| l.agent == "unbound-agent"),
            "another agent's stays"
        );
        assert!(page_state("held-agent", workforce_agent::runner::SYSTEM_USER_ID).is_none());
        close(Which::All).await;
    }

    fn e2e_skill(dir: &std::path::Path) -> BrowserSkill {
        BrowserSkill::new(BrowserSkillConfig {
            chrome_binary: std::env::var("BROWSER_E2E_CHROMIUM").ok(),
            no_sandbox: std::env::var("BROWSER_E2E_NO_SANDBOX").is_ok(),
            profile_root: Some(dir.display().to_string()),
            ..BrowserSkillConfig::default()
        })
        .unwrap()
    }

    async fn run_tool(
        tools: &[Arc<dyn Tool>],
        name: &str,
        input: serde_json::Value,
    ) -> workforce_agent::tools::ToolResult {
        let tool = tools.iter().find(|t| t.name() == name).unwrap().clone();
        tool.execute(input).await.unwrap()
    }

    /// H1–H3 end to end: a file opens by any spelling of its address, a
    /// file page that moves to a file the agent may not read is blanked,
    /// and nothing but the web, about:blank, inline pages and files opens.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore]
    async fn browser_e2e_files_and_other_addresses_are_kept_to_what_may_open() {
        if std::env::var("BROWSER_E2E").is_err() {
            return;
        }
        install_hooks(TEST_HOOKS.clone());
        let dir = tempfile::tempdir().unwrap();
        let ok = dir.path().join("ok-a.html");
        std::fs::write(
            &ok,
            "<!doctype html><title>A</title><a id=\"next\" href=\"no-b.html\">next</a>",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("no-b.html"),
            "<!doctype html><title>B</title>secret",
        )
        .unwrap();
        let skill = e2e_skill(&dir.path().join("profiles"));
        let tools = skill.tools();
        workforce_agent::runner::CURRENT_AGENT_ID
            .scope("file-agent".into(), async {
                let upper = format!("FILE://{}", ok.display());
                let nav = run_tool(
                    &tools,
                    "browser.navigate",
                    serde_json::json!({ "url": upper }),
                )
                .await;
                assert!(nav.success, "{:?}", nav.error);
                let moved = run_tool(
                    &tools,
                    "browser.click",
                    serde_json::json!({ "selector": "#next" }),
                )
                .await;
                assert!(
                    !moved.success,
                    "a file page moved to a file the agent may not read"
                );
                let why = moved.error.unwrap_or_default();
                assert!(why.contains("can't read that file"), "{why}");
                let snap = run_tool(&tools, "browser.snapshot", serde_json::json!({})).await;
                assert!(snap.success);
                assert!(
                    snap.output["url"]
                        .as_str()
                        .unwrap_or_default()
                        .contains("about:blank"),
                    "{snap:?}"
                );
                for no in [
                    "javascript:alert(1)",
                    "chrome://version",
                    "about:version",
                    "view-source:https://a.test/",
                    "file://otherhost/etc/hosts",
                ] {
                    let r = run_tool(&tools, "browser.navigate", serde_json::json!({ "url": no }))
                        .await;
                    assert!(!r.success, "{no} opened");
                }
            })
            .await;
        close(Which::All).await;
    }

    /// H5, M5 and dialogs end to end: a call decided for one origin doesn't
    /// act on another the page moved to on its own; a click Desk can't land
    /// on what it checked (something laid over it) isn't made; an alert is
    /// dismissed and reported, never left waiting.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore]
    async fn browser_e2e_a_moved_page_a_covered_button_and_an_alert() {
        if std::env::var("BROWSER_E2E").is_err() {
            return;
        }
        install_hooks(TEST_HOOKS.clone());
        let site = fixture::serve();
        let dir = tempfile::tempdir().unwrap();
        let skill = e2e_skill(dir.path());
        let tools = skill.tools();
        workforce_agent::runner::CURRENT_AGENT_ID
            .scope("moving-agent".into(), async {
                let elsewhere = fixture::serve();
                let moving = format!("/moving?to={}", elsewhere.url("/shop"));
                assert!(run_tool(&tools, "browser.navigate", serde_json::json!({ "url": site.url(&moving) })).await.success);
                // The gate decides the click for the page's origin now...
                let click = tools.iter().find(|t| t.name() == "browser.click").unwrap().clone();
                let effect = click.effect(&serde_json::json!({ "selector": "#save" }));
                let decided_for = format!("browser:{}", site.url("").trim_end_matches('/'));
                assert!(matches!(effect, workforce_core::computer::Effect::Connector { ref connector, .. } if *connector == decided_for), "{effect:?}");
                // ...and the page moves itself before the click is made.
                tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
                let r = click.execute(serde_json::json!({ "selector": "#save" })).await.unwrap();
                assert!(!r.success, "acted on a page it wasn't decided for");
                assert!(r.error.unwrap_or_default().contains("page moved"));

                assert!(run_tool(&tools, "browser.navigate", serde_json::json!({ "url": site.url("/overlay") })).await.success);
                let covered = run_tool(&tools, "browser.click", serde_json::json!({ "selector": "#buy" })).await;
                assert!(!covered.success);
                assert!(covered.error.unwrap_or_default().contains("covers"));

                let alert = run_tool(&tools, "browser.navigate", serde_json::json!({ "url": site.url("/alert") })).await;
                assert!(alert.success, "an alert never holds the browser: {:?}", alert.error);
                let console = run_tool(&tools, "browser.console_messages", serde_json::json!({})).await;
                let text = console.output["messages"].as_str().unwrap_or_default();
                assert!(text.contains("dialog") && text.contains("fixture alert"), "{text}");
                assert!(text.contains("EXTERNAL_UNTRUSTED_CONTENT_"));
            })
            .await;
        close(Which::All).await;
    }

    /// H6 end to end, with a real session id: Stop for a session closes the
    /// browser its calls came from and names the conversation whose
    /// allowances go; another session's stays. M6: nothing typed is pictured.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore]
    async fn browser_e2e_stop_by_session_id_closes_that_sessions_browser() {
        if std::env::var("BROWSER_E2E").is_err() {
            return;
        }
        install_hooks(TEST_HOOKS.clone());
        let site = fixture::serve();
        let dir = tempfile::tempdir().unwrap();
        let skill = e2e_skill(dir.path());
        let tools = skill.tools();
        let (mine, theirs) = (
            uuid::Uuid::new_v4().to_string(),
            uuid::Uuid::new_v4().to_string(),
        );
        use workforce_agent::runner::{CURRENT_AGENT_ID, CURRENT_SESSION_ID, CURRENT_SESSION_KEY};
        for (agent, id) in [("stop-agent-a", &mine), ("stop-agent-b", &theirs)] {
            CURRENT_AGENT_ID
                .scope(
                    agent.into(),
                    CURRENT_SESSION_ID.scope(
                        id.clone(),
                        CURRENT_SESSION_KEY.scope(format!("desk:{agent}"), async {
                            assert!(
                                run_tool(
                                    &tools,
                                    "browser.navigate",
                                    serde_json::json!({ "url": site.url("/form") })
                                )
                                .await
                                .success
                            );
                            assert!(
                                run_tool(
                                    &tools,
                                    "browser.fill",
                                    serde_json::json!({ "selector": "#name", "value": "Sam" })
                                )
                                .await
                                .success
                            );
                        }),
                    ),
                )
                .await;
        }
        let fills: Vec<Done> = TEST_HOOKS
            .done
            .lock()
            .unwrap()
            .iter()
            .filter(|d| d.tool == "browser.fill" && d.agent.starts_with("stop-agent"))
            .cloned()
            .collect();
        assert!(
            !fills.is_empty() && fills.iter().all(|d| d.screenshot.is_none()),
            "no picture of what was typed"
        );
        assert!(
            fills
                .iter()
                .any(|d| d.session.as_deref() == Some(mine.as_str())),
            "the session id is recorded"
        );
        let open = live().await;
        assert!(open
            .iter()
            .any(|l| l.session.as_deref() == Some(mine.as_str())));
        let kept = stop_session(&mine).await;
        assert_eq!(
            kept,
            vec![("stop-agent-a".to_string(), "desk:stop-agent-a".to_string())]
        );
        let open = live().await;
        assert!(
            !open.iter().any(|l| l.agent == "stop-agent-a"),
            "closed by its session id"
        );
        assert!(
            open.iter().any(|l| l.agent == "stop-agent-b"),
            "another session's stays"
        );
        close(Which::All).await;
    }

    /// `GET path` on 127.0.0.1:`port`, the body.
    fn http_get(port: u16, path: &str) -> String {
        use std::io::{Read, Write};
        let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        write!(
            stream,
            "GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n"
        )
        .unwrap();
        // Chrome may keep the socket open: read the body by its length.
        let mut got = Vec::new();
        let mut buf = [0u8; 8192];
        loop {
            let text = String::from_utf8_lossy(&got).to_string();
            if let Some((head, body)) = text.split_once("\r\n\r\n") {
                let len = head.lines().find_map(|l| {
                    l.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                });
                if len.is_some_and(|n| body.len() >= n) {
                    return body.to_string();
                }
            }
            match stream.read(&mut buf) {
                Ok(0) | Err(_) => {
                    return text
                        .split_once("\r\n\r\n")
                        .map(|(_, b)| b.to_string())
                        .unwrap_or_default()
                }
                Ok(n) => got.extend_from_slice(&buf[..n]),
            }
        }
    }

    /// The page targets the throwaway Chrome has open, by URL.
    fn open_tabs(port: u16) -> Vec<String> {
        let body = http_get(port, "/json/list");
        let start = body.find('[').unwrap_or(0);
        let list: serde_json::Value = serde_json::from_str(&body[start..]).unwrap_or_default();
        list.as_array()
            .into_iter()
            .flatten()
            .filter(|t| t["type"] == "page")
            .filter_map(|t| t["url"].as_str().map(str::to_string))
            .collect()
    }

    /// My Chrome end to end, against a throwaway Chrome launched with its
    /// own user data folder in a test folder (never the person's): Desk
    /// attaches by its `DevToolsActivePort`, works in a window of its own,
    /// reads fenced, holds the person's steps, records what it did, keeps
    /// to the site list, and on Stop closes its own tab only; the tab the
    /// "person" had open is still there, on the same page. Gated on
    /// `BROWSER_E2E`.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore]
    async fn browser_e2e_my_chrome_works_in_its_own_tab_and_leaves_the_rest() {
        if std::env::var("BROWSER_E2E").is_err() {
            return;
        }
        install_hooks(TEST_HOOKS.clone());
        let site = fixture::serve();
        let dir = tempfile::tempdir().unwrap();
        let profile = dir.path().join("their-chrome");
        let binary = std::env::var("BROWSER_E2E_CHROMIUM").unwrap_or_else(|_| "chromium".into());
        let theirs = site.url("/form");
        let mut chrome = std::process::Command::new(binary)
            .args([
                "--headless=new",
                "--no-sandbox",
                "--remote-debugging-port=0",
                "--no-first-run",
                &format!("--user-data-dir={}", profile.display()),
                &theirs,
            ])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("the throwaway Chrome starts");
        let port_file = profile.join("DevToolsActivePort");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while !chrome::available(&profile) {
            assert!(
                std::time::Instant::now() < deadline,
                "no DevToolsActivePort"
            );
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        let port: u16 = std::fs::read_to_string(&port_file)
            .unwrap()
            .lines()
            .next()
            .unwrap()
            .parse()
            .unwrap();
        // The person's tab has loaded.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while !open_tabs(port).contains(&theirs) {
            assert!(
                std::time::Instant::now() < deadline,
                "the person's tab never loaded: {:?} / {}",
                open_tabs(port),
                http_get(port, "/json/list")
            );
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }

        let skill = BrowserSkill::new(BrowserSkillConfig {
            chrome_user_data_dir: Some(profile.display().to_string()),
            profile_root: Some(dir.path().join("desk").display().to_string()),
            ..BrowserSkillConfig::default()
        })
        .unwrap();
        let tools = skill.tools();
        let run = |name: &'static str, input: serde_json::Value| {
            let tool = tools.iter().find(|t| t.name() == name).unwrap().clone();
            async move { tool.execute(input).await.unwrap() }
        };
        let shop = site.url("/shop");
        let session_id = uuid::Uuid::new_v4().to_string();
        let popup = site.url("/network");
        workforce_agent::runner::CURRENT_AGENT_ID
            .scope(
                "chrome-agent".into(),
                workforce_agent::runner::CURRENT_SESSION_ID.scope(session_id.clone(), async {
                    let nav = run(
                        "browser.navigate",
                        serde_json::json!({"url": shop, "browser": "chrome"}),
                    )
                    .await;
                    assert!(nav.success, "{:?}", nav.error);
                    assert_eq!(nav.output["browser"], "chrome");
                    // The call after carries on in the person's Chrome.
                    let snap = run("browser.snapshot", serde_json::json!({})).await;
                    assert_eq!(snap.output["browser"], "chrome");
                    let body = snap.output["body_text"].as_str().unwrap_or_default();
                    assert!(
                        body.contains("EXTERNAL_UNTRUSTED_CONTENT_") && body.contains("Buy now"),
                        "{body}"
                    );
                    let buy = run("browser.click", serde_json::json!({"selector": "#buy"})).await;
                    assert!(
                        !buy.success,
                        "a Buy click in the person's Chrome below unbound"
                    );
                    assert!(
                        run("browser.click", serde_json::json!({"selector": "#save"}))
                            .await
                            .success
                    );
                    let shot = run("browser.screenshot", serde_json::json!({})).await;
                    assert!(shot.success && shot.output["size_bytes"].as_u64().unwrap_or(0) > 200);
                    let blocked = site.url("/form").replace("127.0.0.1", "localhost");
                    let away = run("browser.navigate", serde_json::json!({"url": blocked})).await;
                    assert!(!away.success);
                    assert!(away.error.unwrap_or_default().contains("keeps away"));

                    // M9: keys go only where the field has focus.
                    assert!(
                        run(
                            "browser.navigate",
                            serde_json::json!({"url": site.url("/form")})
                        )
                        .await
                        .success
                    );
                    let typed = run(
                        "browser.type",
                        serde_json::json!({"selector": "#name", "text": "Sam"}),
                    )
                    .await;
                    assert!(typed.success, "{:?}", typed.error);
                    // M9: a click lands only on what was checked.
                    assert!(
                        run(
                            "browser.navigate",
                            serde_json::json!({"url": site.url("/overlay")})
                        )
                        .await
                        .success
                    );
                    let covered =
                        run("browser.click", serde_json::json!({"selector": "#buy"})).await;
                    assert!(!covered.success);
                    assert!(covered.error.unwrap_or_default().contains("covers"));
                    // L: a tab Desk's opens is Desk's, closed with it.
                    assert!(
                        run(
                            "browser.navigate",
                            serde_json::json!({"url": site.url("/popup")})
                        )
                        .await
                        .success
                    );
                    assert!(
                        run("browser.click", serde_json::json!({"selector": "#open"}))
                            .await
                            .success
                    );
                    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
                    while !open_tabs(port).contains(&popup) {
                        assert!(
                            std::time::Instant::now() < deadline,
                            "the popup never opened"
                        );
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    }
                    // H8: a page that takes itself to a site the list keeps Desk
                    // from is blanked, without a call.
                    let blocked_shop = shop.replace("127.0.0.1", "localhost");
                    let moving = format!("/moving?to={blocked_shop}");
                    assert!(
                        run(
                            "browser.navigate",
                            serde_json::json!({"url": site.url(&moving)})
                        )
                        .await
                        .success
                    );
                    tokio::time::sleep(std::time::Duration::from_millis(2000)).await;
                    let snap = run("browser.snapshot", serde_json::json!({})).await;
                    let url = snap.output["url"].as_str().unwrap_or_default().to_string();
                    assert!(url.contains("about:blank"), "{url} / {:?}", snap.error);
                    // An alert in the person's Chrome is dismissed and noted.
                    let alert = run(
                        "browser.navigate",
                        serde_json::json!({"url": site.url("/alert")}),
                    )
                    .await;
                    assert!(alert.success, "{:?}", alert.error);
                    let console = run("browser.console_messages", serde_json::json!({})).await;
                    let text = console.output["messages"].as_str().unwrap_or_default();
                    assert!(
                        text.contains("dialog") && text.contains("fixture alert"),
                        "{text}"
                    );
                    // Desk's own tab left the person's tab where it was.
                    let shop_again =
                        run("browser.navigate", serde_json::json!({"url": shop})).await;
                    assert!(shop_again.success);
                }),
            )
            .await;

        let tabs = open_tabs(port);
        assert!(tabs.contains(&theirs), "the person's tab: {tabs:?}");
        assert!(tabs.contains(&shop), "Desk's own tab: {tabs:?}");
        let done = TEST_HOOKS.done.lock().unwrap().clone();
        let mine: Vec<&Done> = done.iter().filter(|d| d.agent == "chrome-agent").collect();
        assert_eq!(
            mine.iter()
                .take(2)
                .map(|d| d.tool.as_str())
                .collect::<Vec<_>>(),
            ["browser.navigate", "browser.click"]
        );
        assert!(mine.iter().all(|d| d.backend == "chrome"));
        assert!(
            mine.iter()
                .all(|d| (d.tool == "browser.type") == d.screenshot.is_none()),
            "a picture of all but typing"
        );
        assert!(mine
            .iter()
            .all(|d| d.session.as_deref() == Some(session_id.as_str())));
        assert!(live()
            .await
            .iter()
            .any(|l| l.agent == "chrome-agent" && l.backend == "chrome"));

        // H6(c): the turn's end, by its session id, lets the Chrome go.
        assert_eq!(turn_ended(&session_id).await, 1);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let tabs = open_tabs(port);
            if !tabs.contains(&shop) && !tabs.contains(&popup) {
                assert!(
                    tabs.contains(&theirs),
                    "Stop left the person's tab: {tabs:?}"
                );
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "Desk's tab never closed: {tabs:?}"
            );
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        assert!(!live().await.iter().any(|l| l.agent == "chrome-agent"));
        let _ = chrome.kill();
        let _ = chrome.wait();
    }

    /// With remote debugging off (no `DevToolsActivePort`), My Chrome says
    /// how to turn it on and nothing else happens.
    #[tokio::test]
    async fn my_chrome_with_remote_debugging_off_says_how_to_turn_it_on() {
        let dir = tempfile::tempdir().unwrap();
        let skill = BrowserSkill::new(BrowserSkillConfig {
            chrome_user_data_dir: Some(dir.path().display().to_string()),
            profile_root: Some(dir.path().join("desk").display().to_string()),
            ..BrowserSkillConfig::default()
        })
        .unwrap();
        let nav = skill
            .tools()
            .into_iter()
            .find(|t| t.name() == "browser.navigate")
            .unwrap();
        let out = workforce_agent::runner::CURRENT_AGENT_ID
            .scope(
                "chrome-off-agent".into(),
                nav.execute(serde_json::json!({"url": "about:blank", "browser": "chrome"})),
            )
            .await
            .unwrap();
        // With no hooks installed only Desk's own browser is on; with the
        // test's hooks, both are and the toggle is what's missing.
        let error = out.error.unwrap_or_default();
        assert!(!out.success);
        assert!(
            error.contains("chrome://inspect/#remote-debugging")
                || error.contains("Desk doesn't use the person's Chrome"),
            "{error}"
        );
    }
}
