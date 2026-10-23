//! FreeAgent skill — agent-callable tools for the FreeAgent v2 accounting API.
//!
//! Auth is 3-legged OAuth 2.0: an admin configures one OAuth app
//! (`skills/freeagent/oauth_client/*`), each user connects a company via the
//! hub-hosted web-redirect flow (`freeagent.setup`), and per-workspace tokens
//! are minted and refreshed by the shared `workforce-oauth-client`
//! `TokenStore`. Read tools default to `allow`; mutating tools to
//! `require_approval`.

pub mod client;
pub mod provider;
pub mod setup;
pub mod token_store;
pub mod tools;

use std::sync::Arc;

use eyre::{eyre, Result};
use workforce_agent::tools::Tool;
use workforce_common::settings::SecretStore;
use workforce_oauth_client::OAuthClientCreds;

use crate::definition::{CredentialRequirement, SkillDefinition, SkillRequirements};

use client::FreeAgentContext;

/// Vault prefix for the admin-configured OAuth app credentials.
pub const OAUTH_CLIENT_PREFIX: &str = "skills/freeagent/oauth_client/";

fn oauth_field(store: &dyn SecretStore, field: &str) -> Option<String> {
    store.get(&format!("{OAUTH_CLIENT_PREFIX}{field}"))
}

/// Whether the connection targets the FreeAgent sandbox.
pub fn read_sandbox(store: &dyn SecretStore) -> bool {
    oauth_field(store, "sandbox")
        .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "true" | "1" | "yes"))
        .unwrap_or(false)
}

/// Read the OAuth app credentials. `redirect_uri` is included when present —
/// the web flow needs it; refresh and data calls do not.
pub fn read_client_creds(store: &dyn SecretStore) -> Result<OAuthClientCreds> {
    let client_id = oauth_field(store, "client_id")
        .ok_or_else(|| eyre!("FreeAgent is not configured: set {OAUTH_CLIENT_PREFIX}client_id"))?;
    let client_secret = oauth_field(store, "client_secret").ok_or_else(|| {
        eyre!("FreeAgent is not configured: set {OAUTH_CLIENT_PREFIX}client_secret")
    })?;
    Ok(OAuthClientCreds {
        client_id,
        client_secret: Some(client_secret),
        redirect_uri: oauth_field(store, "redirect_uri"),
    })
}

/// The FreeAgent skill — one lazily-built client per connected company,
/// resolving per-user tokens from `store` on each call.
pub struct FreeAgentSkill {
    context: Arc<FreeAgentContext>,
}

impl FreeAgentSkill {
    /// Build from a vault that resolves credentials per request. Errors when
    /// the admin OAuth app is unconfigured.
    pub fn with_store(store: Arc<dyn SecretStore>) -> Result<Self> {
        let creds = read_client_creds(store.as_ref())?;
        let sandbox = read_sandbox(store.as_ref());
        let http = client::build_http_client()?;
        let context = Arc::new(FreeAgentContext::new(store, http, creds, sandbox));
        Ok(Self { context })
    }

    /// All agent-callable data tools.
    pub fn tools(&self) -> Vec<Arc<dyn Tool>> {
        tools::build_tools(Arc::clone(&self.context))
    }

    /// Tool names this skill provides, in registration order — the data tools
    /// plus the separately-built `freeagent.setup` onboarding tool.
    pub fn tool_names() -> Vec<&'static str> {
        let mut names = tools::tool_names();
        names.push(setup::SETUP_TOOL_NAME);
        names
    }

    /// Static skill definition (metadata + policies).
    pub fn definition() -> SkillDefinition {
        use crate::definition::{SkillPolicy, SkillTool};
        let (mut tools, mut default_policies) = tools::skill_tools_and_policies();
        // The setup tool manages only the caller's own connection, so `allow`.
        tools.push(SkillTool {
            name: setup::SETUP_TOOL_NAME.to_string(),
            description: setup::description().to_string(),
            input_schema: setup::input_schema(),
        });
        default_policies.push(SkillPolicy {
            tool: setup::SETUP_TOOL_NAME.to_string(),
            action: "allow".to_string(),
        });
        SkillDefinition {
            name: "freeagent-integration".to_string(),
            version: "1.0.0".to_string(),
            description: "Query and act on FreeAgent accounting data (invoices, bills, contacts, \
                          estimates, expenses, bank transactions, projects, timeslips, P&L) for a \
                          connected company."
                .to_string(),
            author: "workforce".to_string(),
            license: "MIT".to_string(),
            tools,
            event_sources: Vec::new(),
            default_policies,
            memory_schemas: Vec::new(),
            requirements: SkillRequirements {
                credentials: vec![
                    CredentialRequirement {
                        name: "freeagent_client_id".to_string(),
                        r#type: "oauth2".to_string(),
                        description: "FreeAgent OAuth app client id".to_string(),
                    },
                    CredentialRequirement {
                        name: "freeagent_client_secret".to_string(),
                        r#type: "oauth2".to_string(),
                        description: "FreeAgent OAuth app client secret".to_string(),
                    },
                ],
                config: Vec::new(),
            },
            dependencies: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::test_support::InMem;

    fn configured_store() -> Arc<InMem> {
        InMem::from(&[
            ("skills/freeagent/oauth_client/client_id", "cid"),
            ("skills/freeagent/oauth_client/client_secret", "secret"),
        ])
    }

    #[test]
    fn with_store_errors_without_credentials() {
        let store = InMem::from(&[]) as Arc<dyn SecretStore>;
        let err = FreeAgentSkill::with_store(store)
            .err()
            .expect("expected error");
        assert!(err.to_string().contains("not configured"));
    }

    #[test]
    fn with_store_builds_and_exposes_tools() {
        let skill = FreeAgentSkill::with_store(configured_store() as Arc<dyn SecretStore>).unwrap();
        assert!(!skill.tools().is_empty());
    }

    #[test]
    fn read_sandbox_parses_truthy_values() {
        let store = InMem::from(&[("skills/freeagent/oauth_client/sandbox", "TRUE")]);
        assert!(read_sandbox(store.as_ref()));
        let store = InMem::from(&[("skills/freeagent/oauth_client/sandbox", "no")]);
        assert!(!read_sandbox(store.as_ref()));
        assert!(!read_sandbox(InMem::from(&[]).as_ref()));
    }

    #[test]
    fn definition_lists_all_tools_with_policies() {
        let def = FreeAgentSkill::definition();
        assert_eq!(def.name, "freeagent-integration");
        assert_eq!(def.default_policies.len(), def.tools.len());
        let names: Vec<&str> = def.tools.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"freeagent.invoices_list"));
        assert!(names.contains(&"freeagent.timeslip_create"));
    }

    #[test]
    fn tool_names_matches_registered() {
        let skill = FreeAgentSkill::with_store(configured_store() as Arc<dyn SecretStore>).unwrap();
        // `tools()` builds the data tools; the setup tool is built separately
        // by the hub (it needs the shared state store).
        let data: Vec<String> = skill.tools().iter().map(|t| t.name().to_string()).collect();
        assert_eq!(data, tools::tool_names());

        let all = FreeAgentSkill::tool_names();
        assert_eq!(all.len(), data.len() + 1);
        assert_eq!(*all.last().unwrap(), setup::SETUP_TOOL_NAME);
    }

    #[test]
    fn definition_includes_setup_tool_as_allow() {
        let def = FreeAgentSkill::definition();
        let setup = def
            .default_policies
            .iter()
            .find(|p| p.tool == setup::SETUP_TOOL_NAME)
            .expect("setup policy present");
        assert_eq!(setup.action, "allow");
        assert!(def.tools.iter().any(|t| t.name == setup::SETUP_TOOL_NAME));
    }
}
