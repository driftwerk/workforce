//! Xero skill — agent-callable tools for the Xero Accounting API.
//!
//! Auth is the client-credentials grant of a Xero "Custom Connection" (one per
//! organisation). Multiple entities are supported via an optional `entity`
//! parameter on every tool. Read tools default to `allow`; mutating tools to
//! `require_approval`.

pub mod client;
pub mod config;
pub mod export;
pub mod report;
pub mod tools;

use std::collections::HashMap;
use std::sync::Arc;

use workforce_agent::tools::Tool;

use crate::adapters::utils::{build_instances, Instances};
use crate::definition::{CredentialRequirement, SkillDefinition, SkillRequirements};

pub use client::XeroClient;
pub use config::{XeroEntityConfig, XeroSkillConfig};

type Clients = HashMap<String, Arc<XeroClient>>;

/// The Xero skill — holds one client per configured entity.
pub struct XeroSkill {
    clients: Arc<Clients>,
    default_entity: String,
}

impl XeroSkill {
    /// Build from config with static credentials.
    pub fn new(config: XeroSkillConfig) -> eyre::Result<Self> {
        Self::build(config, |_, e| {
            XeroClient::new(&e.client_id, &e.client_secret, e.tenant_id.clone())
        })
    }

    /// Build a skill whose clients resolve `client_id`/`client_secret` per
    /// request from `store`. Pass a `LayeredUserVault` for per-user resolution.
    pub fn with_store(
        config: XeroSkillConfig,
        store: Arc<dyn workforce_common::settings::SecretStore>,
    ) -> eyre::Result<Self> {
        Self::build(config, |name, e| {
            XeroClient::with_store(Arc::clone(&store), name, e.tenant_id.clone())
        })
    }

    fn build(
        config: XeroSkillConfig,
        make: impl Fn(&str, &XeroEntityConfig) -> eyre::Result<XeroClient>,
    ) -> eyre::Result<Self> {
        let Instances { clients, default } = build_instances(
            "xero",
            &config.resolve(),
            config.default_entity.as_deref(),
            "No Xero entities configured. Set `client_id`+`client_secret` or `[entities.<name>]`.",
            make,
        )?;
        Ok(Self {
            clients,
            default_entity: default,
        })
    }

    /// The instances this skill exposes, for the environments of the agent
    /// that loaded it.
    pub fn variants(&self) -> Option<workforce_agent::llm::environment::SkillVariants> {
        workforce_agent::llm::environment::SkillVariants::of(
            self.clients.keys().cloned(),
            &self.default_entity,
        )
    }

    /// All agent-callable tools.
    pub fn tools(&self) -> Vec<Arc<dyn Tool>> {
        tools::build_tools(Arc::clone(&self.clients), self.default_entity.clone())
    }

    /// Tool names this skill provides, in registration order.
    pub fn tool_names() -> Vec<&'static str> {
        tools::tool_names()
    }

    /// Static skill definition (metadata + policies).
    pub fn definition() -> SkillDefinition {
        let (tools, default_policies) = tools::skill_tools_and_policies();
        SkillDefinition {
            name: "xero-integration".to_string(),
            version: "1.0.0".to_string(),
            description: "Query and act on Xero accounting data (P&L, balance sheet, invoices, \
                          contacts, journals) across one or more organisations."
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
                        name: "xero_client_id".to_string(),
                        r#type: "oauth2".to_string(),
                        description: "Xero custom-connection client id".to_string(),
                    },
                    CredentialRequirement {
                        name: "xero_client_secret".to_string(),
                        r#type: "oauth2".to_string(),
                        description: "Xero custom-connection client secret".to_string(),
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

    fn config_single() -> XeroSkillConfig {
        XeroSkillConfig {
            client_id: Some("cid".into()),
            client_secret: Some("secret".into()),
            ..XeroSkillConfig::default()
        }
    }

    #[test]
    fn new_errors_without_entities() {
        let err = XeroSkill::new(XeroSkillConfig::default())
            .err()
            .expect("expected error");
        assert!(err.to_string().contains("No Xero entities"));
    }

    #[test]
    fn new_promotes_flat_to_default() {
        let skill = XeroSkill::new(config_single()).unwrap();
        assert_eq!(skill.default_entity, "default");
        assert!(!skill.tools().is_empty());
    }

    fn named(names: &[&str], default_entity: Option<&str>) -> XeroSkillConfig {
        XeroSkillConfig {
            entities: names
                .iter()
                .map(|name| {
                    (
                        name.to_string(),
                        XeroEntityConfig {
                            client_id: "a".into(),
                            client_secret: "b".into(),
                            tenant_id: None,
                        },
                    )
                })
                .collect(),
            default_entity: default_entity.map(str::to_string),
            ..XeroSkillConfig::default()
        }
    }

    #[test]
    fn a_sole_named_entity_becomes_the_default_even_over_a_stale_selector() {
        assert_eq!(
            XeroSkill::new(named(&["harbor"], None))
                .unwrap()
                .default_entity,
            "harbor"
        );
        assert_eq!(
            XeroSkill::new(named(&["harbor"], Some("nope")))
                .unwrap()
                .default_entity,
            "harbor"
        );
        assert_eq!(
            XeroSkill::new(XeroSkillConfig {
                default_entity: Some("nope".into()),
                ..config_single()
            })
            .unwrap()
            .default_entity,
            "default"
        );
    }

    #[test]
    fn several_entities_without_a_selector_load_and_keep_default_for_the_call_error() {
        let skill = XeroSkill::new(named(&["harbor", "beacon"], None)).unwrap();
        assert_eq!(skill.default_entity, "default");
        let skill = XeroSkill::new(named(&["harbor", "beacon"], Some("beacon"))).unwrap();
        assert_eq!(skill.default_entity, "beacon");
    }

    #[test]
    fn definition_lists_all_tools_with_policies() {
        let def = XeroSkill::definition();
        assert_eq!(def.name, "xero-integration");
        assert_eq!(def.default_policies.len(), def.tools.len());
        let names: Vec<&str> = def.tools.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"xero.profit_and_loss"));
        assert!(names.contains(&"xero.manual_journal_create"));
    }

    #[test]
    fn tool_names_matches_registered() {
        let skill = XeroSkill::new(config_single()).unwrap();
        let registered: Vec<String> = skill.tools().iter().map(|t| t.name().to_string()).collect();
        let declared: Vec<String> = XeroSkill::tool_names()
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(registered, declared);
    }
}
