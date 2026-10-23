//! Obscura skill — headless-browser automation via the Obscura `mcp` binary
//! over stdio JSON-RPC. Lighter than the chromium-backed `browser` skill: no
//! screenshot, no PDF, no chromiumoxide dependency. Intended for non-visual
//! scraping / form-automation flows where the agent doesn't need a PNG.

mod client;
mod config;
mod process;
mod protocol;
mod tools;

use std::sync::Arc;

use workforce_agent::tools::Tool;

use crate::definition::{SkillDefinition, SkillRequirements};

pub use client::ObscuraClient;
pub use config::ObscuraSkillConfig;

pub struct ObscuraSkill {
    client: Arc<ObscuraClient>,
}

impl ObscuraSkill {
    pub fn new(config: ObscuraSkillConfig) -> eyre::Result<Self> {
        Ok(Self {
            client: Arc::new(ObscuraClient::new(config)),
        })
    }

    /// Construct a skill whose `obscura.fill.value` / `obscura.type.text`
    /// resolve `[[vault:<key>]]` markers against the supplied per-agent vault,
    /// addressing values under `obscura_creds/`.
    pub fn with_store(
        config: ObscuraSkillConfig,
        store: Arc<dyn workforce_common::settings::SecretStore>,
    ) -> eyre::Result<Self> {
        Ok(Self {
            client: Arc::new(ObscuraClient::with_store(config, store)),
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
            name: "obscura".to_string(),
            version: "1.0.0".to_string(),
            description:
                "Headless browser automation via Obscura over stdio MCP. Lower memory than the \
                 chromium-backed `browser` skill, but with two important limitations: \
                 (1) no screenshot or PDF capability, and \
                 (2) the HTTP client truncates large responses to 0 bytes — verified 2026-05-28 \
                 against a React SPA where the JS bundle returned 200 with no body and the page \
                 never hydrated. SUITABLE FOR: static HTML pages, server-rendered content, \
                 simple forms, text-only scraping. NOT SUITABLE FOR: modern SPAs (React, Vue, \
                 Angular) or any page that depends on a large client-side JS bundle — use the \
                 `browser.*` skill for those."
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
        let def = ObscuraSkill::definition();
        let names: Vec<&str> = ObscuraSkill::tool_names();
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
        let skill = ObscuraSkill::new(ObscuraSkillConfig::default()).unwrap();
        let registered: Vec<String> = skill
            .tools()
            .into_iter()
            .map(|t| t.name().to_string())
            .collect();
        let declared: Vec<String> = ObscuraSkill::tool_names()
            .into_iter()
            .map(String::from)
            .collect();
        assert_eq!(registered, declared);
    }

    #[test]
    fn skill_constructs_without_spawning_subprocess() {
        let skill = ObscuraSkill::new(ObscuraSkillConfig {
            obscura_binary: "/definitely-not-real".to_string(),
            ..ObscuraSkillConfig::default()
        });
        assert!(skill.is_ok(), "construction should not spawn obscura");
    }

    /// End-to-end smoke test: spawn the real obscura binary, run the MCP
    /// handshake, navigate to a local HTML fixture, fill an input, evaluate
    /// the readback. Gated on `OBSCURA_TEST_BIN` so CI without the binary
    /// skips it; run locally with:
    ///   OBSCURA_TEST_BIN=$(which obscura) cargo test -p workforce-skills \
    ///       --features obscura -- --ignored obscura_e2e
    #[tokio::test]
    #[ignore]
    async fn obscura_e2e_navigate_fill_evaluate() {
        let Ok(binary) = std::env::var("OBSCURA_TEST_BIN") else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let html_path = dir.path().join("fixture.html");
        std::fs::write(
            &html_path,
            "<!doctype html><title>obscura-fix</title><body>\
             <input id=\"q\" /><div id=\"out\"></div>\
             <script>document.getElementById('q').addEventListener('input', e => \
             document.getElementById('out').textContent = 'got:' + e.target.value);</script>\
             </body>",
        )
        .unwrap();
        let url = format!("file://{}", html_path.display());

        let skill = ObscuraSkill::new(ObscuraSkillConfig {
            obscura_binary: binary,
            ..ObscuraSkillConfig::default()
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

        let nav = find("obscura.navigate")
            .execute(serde_json::json!({ "url": url }))
            .await
            .unwrap();
        assert!(nav.success, "navigate failed: {:?}", nav.error);

        let snap = find("obscura.snapshot")
            .execute(serde_json::json!({}))
            .await
            .unwrap();
        assert!(snap.success, "snapshot failed: {:?}", snap.error);
        let snap_dump = serde_json::to_string(&snap.output).unwrap();
        assert!(
            snap_dump.contains("obscura-fix"),
            "snapshot should include fixture title, got: {snap_dump}"
        );

        let fill = find("obscura.fill")
            .execute(serde_json::json!({ "selector": "#q", "value": "alice" }))
            .await
            .unwrap();
        assert!(fill.success, "fill failed: {:?}", fill.error);

        let read = find("obscura.evaluate")
            .execute(serde_json::json!({
                "expression": "document.getElementById('out').textContent"
            }))
            .await
            .unwrap();
        assert!(read.success, "evaluate failed: {:?}", read.error);
        let read_dump = serde_json::to_string(&read.output).unwrap();
        assert!(
            read_dump.contains("got:alice"),
            "evaluate should reflect fill state, got: {read_dump}"
        );
    }
}
