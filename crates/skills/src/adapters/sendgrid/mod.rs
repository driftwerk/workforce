//! SendGrid skill — agent-callable tools for the Twilio SendGrid v3 API.
//!
//! One API key per workspace, sent as a bearer token. Read tools (senders,
//! domains, templates, stats, email activity, suppressions) default to
//! `allow`; sending mail, mutating suppressions and the `sendgrid.api`
//! escape hatch default to `require_approval`. Each workspace may carry a
//! default From (`from_email` / `from_name`), which — like the key — resolves
//! through the layered vault per request so a per-user key pairs with a
//! per-user sender.

pub mod client;
pub mod config;
pub mod tools;

use std::sync::Arc;

use eyre::Result;
use workforce_agent::tools::Tool;
use workforce_common::settings::SecretStore;

use crate::adapters::utils::{
    build_instances, live_from_config, InstancePaths, Instances, LiveInstances,
};
use crate::definition::{CredentialRequirement, SkillDefinition, SkillRequirements};

pub use client::{SendGridClient, Sender};
pub use config::{SendGridSkillConfig, SendGridWorkspaceConfig};

/// Read-only tools — granted `allow` and offered to research workers.
pub const READ_TOOLS: &[&str] = &[
    "sendgrid.senders_list",
    "sendgrid.domains_list",
    "sendgrid.templates_list",
    "sendgrid.template_get",
    "sendgrid.stats_global",
    "sendgrid.stats_categories",
    "sendgrid.messages_search",
    "sendgrid.message_get",
    "sendgrid.suppressions_list",
    "sendgrid.suppression_get",
    "sendgrid.unsubscribe_groups_list",
];

/// Mutating tools — granted `require_approval`.
pub const WRITE_TOOLS: &[&str] = &[
    "sendgrid.email_send",
    "sendgrid.email_send_batch",
    "sendgrid.suppression_delete",
    "sendgrid.unsubscribes_add",
    "sendgrid.api",
];

/// The SendGrid skill — one client per configured workspace.
pub struct SendGridSkill {
    clients: Arc<LiveInstances<SendGridClient>>,
    default_workspace: String,
}

impl SendGridSkill {
    /// Static credentials (local-mcp / config.toml). Use
    /// [`SendGridSkill::with_store`] for per-user resolution.
    pub fn new(config: SendGridSkillConfig) -> Result<Self> {
        Self::build(config, None, |_, ws| {
            let sender = ws.from_email.clone().map(|email| Sender {
                email,
                name: ws.from_name.clone(),
            });
            SendGridClient::new(&ws.api_key, sender, ws.base_url.as_deref())
        })
    }

    /// Credentials resolved from `store` on every request; pass a
    /// `LayeredUserVault` so per-user overlays apply.
    pub fn with_store(config: SendGridSkillConfig, store: Arc<dyn SecretStore>) -> Result<Self> {
        let live = (Arc::clone(&store), Self::instance_paths());
        Self::build(config, Some(live), move |name, ws| {
            SendGridClient::with_store(Arc::clone(&store), name, ws.base_url.as_deref())
        })
    }

    fn build(
        config: SendGridSkillConfig,
        live: Option<(
            Arc<dyn workforce_common::settings::SecretStore>,
            InstancePaths,
        )>,
        make: impl Fn(&str, &SendGridWorkspaceConfig) -> Result<SendGridClient> + Send + Sync + 'static,
    ) -> Result<Self> {
        let resolved = config.resolve();
        let Instances { clients, default } = build_instances(
            "sendgrid",
            &resolved,
            config.default_workspace.as_deref(),
            "No SendGrid workspaces configured. Set api_key or workspaces.*.",
            &make,
        )?;
        // Store-backed: the instances are the caller's, read per call; the
        // loader's are only asked whether there is anything to load.
        let clients = Arc::new(match live {
            None => LiveInstances::fixed(Instances {
                clients,
                default: default.clone(),
            }),
            Some((store, paths)) => live_from_config(
                store,
                paths,
                config.default_workspace.clone(),
                resolved,
                SendGridWorkspaceConfig {
                    api_key: String::new(),
                    from_email: None,
                    from_name: None,
                    base_url: None,
                },
                make,
            ),
        });
        Ok(Self {
            clients,
            default_workspace: default,
        })
    }

    /// Where this adapter's instances live in the vault.
    pub fn instance_paths() -> InstancePaths {
        InstancePaths::new(
            "skills/sendgrid/workspaces/",
            "api_key",
            "skills/sendgrid/api_key",
        )
    }

    /// The instances this skill exposes, read as whoever asks.
    pub fn instance_source(&self) -> Arc<dyn workforce_agent::llm::environment::InstanceSource> {
        Arc::clone(&self.clients) as Arc<dyn workforce_agent::llm::environment::InstanceSource>
    }

    pub fn tools(&self) -> Vec<Arc<dyn Tool>> {
        tools::build_tools(Arc::clone(&self.clients), self.default_workspace.clone())
    }

    pub fn tool_names() -> Vec<&'static str> {
        tools::tool_names()
    }

    pub fn definition() -> SkillDefinition {
        let (tools, default_policies) = tools::skill_tools_and_policies();
        SkillDefinition {
            name: "sendgrid-integration".to_string(),
            version: "1.0.0".to_string(),
            description: "Send transactional email and inspect delivery, stats, templates, sender \
                          identities and suppressions via the Twilio SendGrid v3 API across \
                          multiple accounts"
                .to_string(),
            author: "workforce".to_string(),
            license: "MIT".to_string(),
            tools,
            event_sources: Vec::new(),
            default_policies,
            memory_schemas: Vec::new(),
            requirements: SkillRequirements {
                credentials: vec![CredentialRequirement {
                    name: "sendgrid_api_key".to_string(),
                    r#type: "api_key".to_string(),
                    description: "SendGrid API key (Authorization: Bearer)".to_string(),
                }],
                config: Vec::new(),
            },
            dependencies: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::test_support::{assert_read_write_partition, InMem};
    use std::collections::HashMap;

    fn flat() -> SendGridSkillConfig {
        SendGridSkillConfig {
            api_key: Some("SG.flat".into()),
            from_email: Some("noreply@example.com".into()),
            ..SendGridSkillConfig::default()
        }
    }

    #[test]
    fn skill_builds_with_flat_config() {
        let skill = SendGridSkill::new(flat()).unwrap();
        let tools = skill.tools();
        assert_eq!(tools.len(), SendGridSkill::tool_names().len());
        assert!(tools.iter().all(|t| t.name().starts_with("sendgrid.")));
    }

    #[test]
    fn skill_builds_with_workspaces_and_store() {
        let config = SendGridSkillConfig {
            workspaces: HashMap::from([(
                "acme".to_string(),
                SendGridWorkspaceConfig {
                    api_key: "SG.acme".into(),
                    from_email: None,
                    from_name: None,
                    base_url: Some("https://api.eu.sendgrid.com".into()),
                },
            )]),
            default_workspace: None,
            ..SendGridSkillConfig::default()
        };
        let skill = SendGridSkill::with_store(config, InMem::from(&[])).unwrap();
        assert_eq!(skill.default_workspace, "acme");
        assert_eq!(
            skill.clients.for_caller().0["acme"].base_url(),
            "https://api.eu.sendgrid.com"
        );
    }

    #[test]
    fn skill_errors_without_workspaces_or_with_a_bad_key() {
        assert!(SendGridSkill::new(SendGridSkillConfig::default()).is_err());
        let bad = SendGridSkillConfig {
            api_key: Some("bad\nkey".into()),
            ..SendGridSkillConfig::default()
        };
        assert!(SendGridSkill::new(bad).is_err());
    }

    #[test]
    fn tool_names_matches_registered_tools() {
        let registered: Vec<String> = SendGridSkill::new(flat())
            .unwrap()
            .tools()
            .iter()
            .map(|t| t.name().to_string())
            .collect();
        assert_eq!(registered, SendGridSkill::tool_names());
    }

    #[test]
    fn definition_validates_with_one_policy_per_tool() {
        let def = SendGridSkill::definition();
        assert_eq!(def.tools.len(), def.default_policies.len());
        assert_eq!(def.tools.len(), SendGridSkill::tool_names().len());
        assert!(def.validate().is_ok());
        assert_read_write_partition(SendGridSkill::tool_names(), READ_TOOLS, WRITE_TOOLS);
    }
}
