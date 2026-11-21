//! FreeAgent skill — agent-callable tools for the FreeAgent v2 accounting API.
//!
//! Auth is 3-legged OAuth 2.0: one OAuth app is kept
//! (`skills/freeagent/oauth_client/*`), each business is connected through
//! Desk's Connections or, elsewhere, the hub-hosted web-redirect flow
//! (`freeagent.setup`), and per-workspace tokens
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

/// The FreeAgent skill — one lazily-built client per connected business,
/// resolving per-user tokens from `store` on each call.
pub struct FreeAgentSkill {
    context: Arc<FreeAgentContext>,
}

/// The tool that names the caller's businesses.
pub const WORKSPACES_TOOL_NAME: &str = "freeagent.workspaces";

const WORKSPACES_DESCRIPTION: &str = "The FreeAgent businesses you can use, by name, as {workspaces, default}: default is the one a call without `workspace` goes to, or null when you must pass one. Takes no arguments.";

impl FreeAgentSkill {
    /// Build from a vault that resolves credentials per request, `selector`
    /// naming the business a call that names none goes to. The app is read
    /// as each business's client is built, so one kept after this works
    /// without loading it again; until then a call answers how to set it up.
    pub fn with_store(store: Arc<dyn SecretStore>, selector: Option<String>) -> Result<Self> {
        let http = client::build_http_client()?;
        let context = Arc::new(FreeAgentContext::new(store, http, selector));
        Ok(Self { context })
    }

    /// Where this adapter's businesses live in the vault: each one's
    /// refresh token, and no flat key.
    pub fn instance_paths() -> crate::adapters::utils::InstancePaths {
        crate::adapters::utils::InstancePaths::new(
            token_store::WORKSPACE_PREFIX,
            "refresh_token",
            "",
        )
    }

    /// The businesses this skill exposes, read as whoever asks.
    pub fn instance_source(&self) -> Arc<dyn workforce_agent::llm::environment::InstanceSource> {
        self.context.clients() as Arc<dyn workforce_agent::llm::environment::InstanceSource>
    }

    /// All agent-callable data tools, and the one naming the businesses.
    pub fn tools(&self) -> Vec<Arc<dyn Tool>> {
        let mut tools = tools::build_tools(Arc::clone(&self.context));
        tools.push(Arc::new(crate::adapters::utils::WorkspacesTool::new(
            WORKSPACES_TOOL_NAME,
            WORKSPACES_DESCRIPTION,
            self.context.clients(),
        )));
        tools
    }

    /// Tool names this skill provides, in registration order — the data
    /// tools, `freeagent.workspaces`, and the separately-built
    /// `freeagent.setup` onboarding tool.
    pub fn tool_names() -> Vec<&'static str> {
        let mut names = tools::tool_names();
        names.push(WORKSPACES_TOOL_NAME);
        names.push(setup::SETUP_TOOL_NAME);
        names
    }

    /// Static skill definition (metadata + policies).
    pub fn definition() -> SkillDefinition {
        use crate::definition::{SkillPolicy, SkillTool};
        let (mut tools, mut default_policies) = tools::skill_tools_and_policies();
        // Naming the businesses reads, and the setup tool manages only the
        // caller's own connection, so both `allow`.
        for (name, description, input_schema) in [
            (
                WORKSPACES_TOOL_NAME,
                WORKSPACES_DESCRIPTION,
                serde_json::json!({ "type": "object", "properties": {}, "required": [] }),
            ),
            (
                setup::SETUP_TOOL_NAME,
                setup::description(),
                setup::input_schema(),
            ),
        ] {
            tools.push(SkillTool {
                name: name.to_string(),
                description: description.to_string(),
                input_schema,
            });
            default_policies.push(SkillPolicy {
                tool: name.to_string(),
                action: "allow".to_string(),
            });
        }
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
    use crate::adapters::test_support::{InMem, People};

    fn configured_store() -> Arc<InMem> {
        InMem::from(&[
            ("skills/freeagent/oauth_client/client_id", "cid"),
            ("skills/freeagent/oauth_client/client_secret", "secret"),
        ])
    }

    #[tokio::test]
    async fn no_app_kept_answers_set_up_in_words() {
        let store = People::new(&[("skills/freeagent/workspaces/harbor/refresh_token", "rt")]);
        let skill = FreeAgentSkill::with_store(store as Arc<dyn SecretStore>, None).unwrap();
        let company = skill
            .tools()
            .into_iter()
            .find(|t| t.name() == "freeagent.company")
            .unwrap();
        let res = company.execute(serde_json::json!({})).await.unwrap();
        assert!(!res.success);
        assert_eq!(res.error.unwrap(), client::NOT_SET_UP);
    }

    #[tokio::test]
    async fn freeagent_workspaces_answers_names_and_default() {
        let store = People::new(&[
            ("skills/freeagent/oauth_client/client_id", "cid"),
            ("skills/freeagent/oauth_client/client_secret", "secret"),
            ("skills/freeagent/workspaces/harbor/refresh_token", "rt"),
            ("skills/freeagent/workspaces/lumen/refresh_token", "rt"),
        ]);
        store.own(
            "sarah",
            "skills/freeagent/workspaces/mine/refresh_token",
            "rt",
        );
        let skill = FreeAgentSkill::with_store(store as Arc<dyn SecretStore>, Some("lumen".into()))
            .unwrap();
        let workspaces = skill
            .tools()
            .into_iter()
            .find(|t| t.name() == WORKSPACES_TOOL_NAME)
            .unwrap();
        let answer = workspaces
            .execute(serde_json::json!({}))
            .await
            .unwrap()
            .output;
        assert_eq!(
            answer,
            serde_json::json!({"workspaces": ["harbor", "lumen"], "default": "lumen"})
        );
        let sarahs = workforce_agent::runner::CURRENT_USER_ID
            .scope(
                "sarah".to_string(),
                workspaces.execute(serde_json::json!({})),
            )
            .await
            .unwrap()
            .output;
        assert_eq!(
            sarahs["workspaces"],
            serde_json::json!(["harbor", "lumen", "mine"])
        );
        let variants = skill.instance_source().variants().unwrap();
        assert_eq!(variants.instances, ["harbor", "lumen"]);
        assert_eq!(variants.default, "lumen");
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
        assert_eq!(names, FreeAgentSkill::tool_names());
    }

    #[test]
    fn tool_names_matches_registered() {
        let skill =
            FreeAgentSkill::with_store(configured_store() as Arc<dyn SecretStore>, None).unwrap();
        // `tools()` builds the data tools and `freeagent.workspaces`; the
        // setup tool is built separately by the hub (it needs the shared
        // state store).
        let registered: Vec<String> = skill.tools().iter().map(|t| t.name().to_string()).collect();
        let all = FreeAgentSkill::tool_names();
        assert_eq!(registered, all[..all.len() - 1]);
        assert_eq!(*all.last().unwrap(), setup::SETUP_TOOL_NAME);
    }

    #[test]
    fn definition_includes_workspaces_and_setup_as_allow() {
        let def = FreeAgentSkill::definition();
        for name in [WORKSPACES_TOOL_NAME, setup::SETUP_TOOL_NAME] {
            let policy = def
                .default_policies
                .iter()
                .find(|p| p.tool == name)
                .expect("policy present");
            assert_eq!(policy.action, "allow", "{name}");
            assert!(def.tools.iter().any(|t| t.name == name), "{name}");
        }
    }
}
