//! Browser skill: the person's own Chrome, signed in as them, through
//! Desk's Chrome extension (`extension`). Each `(agent, person)` gets a tab
//! in Desk's own Chrome window; nothing here launches a browser, and a hub
//! with no extension connected (no screen, a job) has no browser at all.
//! Everything the page says reaches the model fenced as untrusted
//! (`untrusted`); a key page's keys never reach it (`keys`).

mod address;
mod client;
mod config;
mod extension;
mod hooks;
pub mod keys;
mod tools;
mod untrusted;

use std::sync::Arc;

use workforce_agent::tools::Tool;

use crate::definition::{SkillDefinition, SkillRequirements};

pub use address::{host_of, origin_of};
pub use client::{
    close, forget_decision, live, page_state, sign_in_ended, stop_session, tab_closed, turn_ended,
    BrowserClient, Live, Which, MAX_UPLOAD_BYTES,
};
pub use config::BrowserSkillConfig;
pub use extension::{install_link, ExtensionLink, NOT_CONNECTED, NO_BROWSER};
pub use hooks::{install_hooks, BrowserHooks, Done, OnPage, Settings, Sites};
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

    /// A skill whose `browser.fill.value` / `browser.type.text` resolve
    /// `[[vault:<key>]]` markers against the supplied per-agent vault, under
    /// `browser_creds/`.
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
            version: "3.0.0".to_string(),
            description:
                "The person's own Chrome, signed in as them, through Desk's Chrome extension: open \
                 pages, read them, click, type and fill, take screenshots, and have the person sign \
                 in themselves. Only in a Desk turn the person is in."
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
mod tests;
