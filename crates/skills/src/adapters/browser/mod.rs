//! Headless-browser skill — drives a Chromium subprocess per `(agent, user)`
//! identity through the Chrome DevTools Protocol via `chromiumoxide`, each with
//! an isolated on-disk profile so login state never crosses users. Within a
//! session all tools share one live browser, so screenshots and PDFs reflect the
//! same DOM state as click/fill/evaluate.

mod client;
mod config;
mod tools;

use std::sync::Arc;

use workforce_agent::tools::Tool;

use crate::definition::{SkillDefinition, SkillRequirements};

pub use client::BrowserClient;
pub use config::BrowserSkillConfig;

pub struct BrowserSkill {
    client: Arc<BrowserClient>,
}

impl BrowserSkill {
    pub fn new(config: BrowserSkillConfig) -> eyre::Result<Self> {
        Ok(Self {
            client: Arc::new(BrowserClient::new(config)),
        })
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
        Ok(Self {
            client: Arc::new(BrowserClient::with_store(config, store)),
        })
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
        let html_path = dir.path().join("fixture.html");
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
        assert_eq!(snap.output["title"], "fix-title");

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
        assert_eq!(content.output["result"], "got:alice");

        let shot = find("browser.screenshot")
            .execute(serde_json::json!({}))
            .await
            .unwrap();
        assert!(shot.success);
        assert_eq!(shot.output["format"], "png");
        assert!(shot.output["size_bytes"].as_u64().unwrap_or(0) > 100);
    }
}
