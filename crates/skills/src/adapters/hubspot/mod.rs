//! HubSpot skill — agent-callable tools for the HubSpot CRM API.
//!
//! Auth is via Private App access tokens (`Authorization: Bearer <token>`).
//! Supports multi-portal configuration via an optional `portal` parameter
//! on every tool. `tools` holds the typed contact/company/deal/ticket tools,
//! `notes` the note lifecycle, `objects` the generic record, association,
//! owner and property tools, and `api` the raw-endpoint escape hatch.

pub mod api;
pub mod client;
pub mod config;
pub mod notes;
pub mod objects;
pub mod tools;

use std::collections::HashMap;
use std::sync::Arc;

use workforce_agent::tools::Tool;

use crate::adapters::utils::{build_instances, Instances};
use crate::definition::{
    CredentialRequirement, SkillDefinition, SkillPolicy, SkillRequirements, SkillTool,
};

pub use config::{HubspotPortalConfig, HubspotSkillConfig};

/// The HubSpot skill — holds a client per configured portal.
pub struct HubspotSkill {
    clients: Arc<HashMap<String, Arc<client::HubspotClient>>>,
    default_portal: String,
}

impl HubspotSkill {
    /// Build from config with static credentials. Errors if no portals are configured.
    pub fn new(config: HubspotSkillConfig) -> eyre::Result<Self> {
        Self::build(config, |_, portal| {
            client::HubspotClient::new(&portal.access_token, portal.max_results)
        })
    }

    /// Build a `HubspotSkill` whose clients resolve their access token from
    /// `store` per request. Pass a `LayeredUserVault` for per-user resolution.
    pub fn with_store(
        config: HubspotSkillConfig,
        store: Arc<dyn workforce_common::settings::SecretStore>,
    ) -> eyre::Result<Self> {
        Self::build(config, |name, portal| {
            client::HubspotClient::with_store(Arc::clone(&store), name, portal.max_results)
        })
    }

    fn build(
        config: HubspotSkillConfig,
        make: impl Fn(&str, &HubspotPortalConfig) -> eyre::Result<client::HubspotClient>,
    ) -> eyre::Result<Self> {
        let Instances { clients, default } = build_instances(
            "hubspot",
            &config.resolve(),
            config.default_portal.as_deref(),
            "No HubSpot portals configured. Set `access_token` or `[portals.<name>]`.",
            make,
        )?;
        Ok(Self {
            clients,
            default_portal: default,
        })
    }

    /// All agent-callable tools.
    pub fn tools(&self) -> Vec<Arc<dyn Tool>> {
        let c = &self.clients;
        let d = &self.default_portal;
        vec![
            Arc::new(tools::ContactListTool::new(Arc::clone(c), d.clone())),
            Arc::new(tools::ContactGetTool::new(Arc::clone(c), d.clone())),
            Arc::new(tools::ContactCreateTool::new(Arc::clone(c), d.clone())),
            Arc::new(tools::ContactUpdateTool::new(Arc::clone(c), d.clone())),
            Arc::new(tools::ContactSearchTool::new(Arc::clone(c), d.clone())),
            Arc::new(tools::CompanyListTool::new(Arc::clone(c), d.clone())),
            Arc::new(tools::CompanyGetTool::new(Arc::clone(c), d.clone())),
            Arc::new(tools::CompanyCreateTool::new(Arc::clone(c), d.clone())),
            Arc::new(tools::CompanyUpdateTool::new(Arc::clone(c), d.clone())),
            Arc::new(tools::CompanySearchTool::new(Arc::clone(c), d.clone())),
            Arc::new(tools::DealListTool::new(Arc::clone(c), d.clone())),
            Arc::new(tools::DealGetTool::new(Arc::clone(c), d.clone())),
            Arc::new(tools::DealCreateTool::new(Arc::clone(c), d.clone())),
            Arc::new(tools::DealUpdateTool::new(Arc::clone(c), d.clone())),
            Arc::new(tools::DealSearchTool::new(Arc::clone(c), d.clone())),
            Arc::new(tools::TicketListTool::new(Arc::clone(c), d.clone())),
            Arc::new(tools::TicketGetTool::new(Arc::clone(c), d.clone())),
            Arc::new(tools::TicketCreateTool::new(Arc::clone(c), d.clone())),
            Arc::new(tools::TicketUpdateTool::new(Arc::clone(c), d.clone())),
            Arc::new(tools::PipelineListTool::new(Arc::clone(c), d.clone())),
            Arc::new(tools::OwnerListTool::new(Arc::clone(c), d.clone())),
            Arc::new(tools::PropertyListTool::new(Arc::clone(c), d.clone())),
            Arc::new(tools::NoteCreateTool::new(Arc::clone(c), d.clone())),
            Arc::new(tools::CrmSearchTool::new(Arc::clone(c), d.clone())),
            Arc::new(notes::NoteGetTool::new(Arc::clone(c), d.clone())),
            Arc::new(notes::NoteListTool::new(Arc::clone(c), d.clone())),
            Arc::new(notes::NoteUpdateTool::new(Arc::clone(c), d.clone())),
            Arc::new(notes::NoteDeleteTool::new(Arc::clone(c), d.clone())),
            Arc::new(objects::ObjectGetTool::new(Arc::clone(c), d.clone())),
            Arc::new(objects::ObjectListTool::new(Arc::clone(c), d.clone())),
            Arc::new(objects::ObjectBatchReadTool::new(Arc::clone(c), d.clone())),
            Arc::new(objects::ObjectCreateTool::new(Arc::clone(c), d.clone())),
            Arc::new(objects::ObjectUpdateTool::new(Arc::clone(c), d.clone())),
            Arc::new(objects::ObjectDeleteTool::new(Arc::clone(c), d.clone())),
            Arc::new(objects::AssociationListTool::new(Arc::clone(c), d.clone())),
            Arc::new(objects::AssociationCreateTool::new(
                Arc::clone(c),
                d.clone(),
            )),
            Arc::new(objects::AssociationDeleteTool::new(
                Arc::clone(c),
                d.clone(),
            )),
            Arc::new(objects::OwnerGetTool::new(Arc::clone(c), d.clone())),
            Arc::new(objects::PropertyGetTool::new(Arc::clone(c), d.clone())),
            Arc::new(api::ApiTool::new(Arc::clone(c), d.clone())),
        ]
    }

    /// Tool names provided by this skill, in registration order.
    pub fn tool_names() -> Vec<&'static str> {
        vec![
            "hubspot.contact_list",
            "hubspot.contact_get",
            "hubspot.contact_create",
            "hubspot.contact_update",
            "hubspot.contact_search",
            "hubspot.company_list",
            "hubspot.company_get",
            "hubspot.company_create",
            "hubspot.company_update",
            "hubspot.company_search",
            "hubspot.deal_list",
            "hubspot.deal_get",
            "hubspot.deal_create",
            "hubspot.deal_update",
            "hubspot.deal_search",
            "hubspot.ticket_list",
            "hubspot.ticket_get",
            "hubspot.ticket_create",
            "hubspot.ticket_update",
            "hubspot.pipeline_list",
            "hubspot.owner_list",
            "hubspot.property_list",
            "hubspot.note_create",
            "hubspot.crm_search",
            "hubspot.note_get",
            "hubspot.note_list",
            "hubspot.note_update",
            "hubspot.note_delete",
            "hubspot.object_get",
            "hubspot.object_list",
            "hubspot.object_batch_read",
            "hubspot.object_create",
            "hubspot.object_update",
            "hubspot.object_delete",
            "hubspot.association_list",
            "hubspot.association_create",
            "hubspot.association_delete",
            "hubspot.owner_get",
            "hubspot.property_get",
            "hubspot.api",
        ]
    }

    /// Static skill definition (metadata + policies).
    ///
    /// Tool list, descriptions, and schemas are derived from the runtime
    /// [`Tool`] impls in [`tools`] so a single source of truth governs both
    /// the agent-facing schema and the manifest. Portal-dependent description
    /// text reflects the single-placeholder-portal shape; multi-portal
    /// deployments expose richer wording at runtime.
    pub fn definition() -> SkillDefinition {
        let skill = Self::new(HubspotSkillConfig {
            access_token: Some("placeholder".to_string()),
            ..HubspotSkillConfig::default()
        })
        .expect("placeholder HubSpot config builds a metadata skill");

        let tools: Vec<SkillTool> = skill
            .tools()
            .iter()
            .map(|t| SkillTool {
                name: t.name().to_string(),
                description: t.description().to_string(),
                input_schema: t.input_schema(),
            })
            .collect();

        let default_policies: Vec<SkillPolicy> = tools
            .iter()
            .map(|t| SkillPolicy {
                tool: t.name.clone(),
                action: "allow".to_string(),
            })
            .collect();

        SkillDefinition {
            name: "hubspot-integration".to_string(),
            version: "1.0.0".to_string(),
            description: "Interact with HubSpot CRM across multiple portals — contacts, companies, deals, tickets and custom objects; notes, tasks, calls, emails and meetings; associations, pipelines, owners and properties; and any other endpoint through hubspot.api.".to_string(),
            author: "workforce".to_string(),
            license: "MIT".to_string(),
            tools,
            event_sources: Vec::new(),
            default_policies,
            memory_schemas: Vec::new(),
            requirements: SkillRequirements {
                credentials: vec![CredentialRequirement {
                    name: "hubspot_access_token".to_string(),
                    r#type: "api_key".to_string(),
                    description: "HubSpot Private App access token (Bearer auth)".to_string(),
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

    #[test]
    fn definition_lists_all_tools() {
        let def = HubspotSkill::definition();
        assert_eq!(def.name, "hubspot-integration");
        assert_eq!(def.tools.len(), 40);
        let names: Vec<&str> = def.tools.iter().map(|t| t.name.as_str()).collect();
        for expected in [
            "hubspot.contact_list",
            "hubspot.deal_search",
            "hubspot.crm_search",
            "hubspot.note_create",
            "hubspot.note_get",
            "hubspot.note_list",
            "hubspot.note_update",
            "hubspot.note_delete",
            "hubspot.object_delete",
            "hubspot.association_create",
            "hubspot.api",
        ] {
            assert!(names.contains(&expected), "{expected}");
        }
        // All tools have a default policy.
        assert_eq!(def.default_policies.len(), def.tools.len());
    }

    #[test]
    fn new_errors_when_no_portals() {
        let err = HubspotSkill::new(HubspotSkillConfig::default())
            .err()
            .expect("expected error");
        assert!(err.to_string().contains("No HubSpot portals"));
    }

    #[test]
    fn a_stale_selector_is_ignored_and_a_sole_portal_is_the_default() {
        let flat = HubspotSkillConfig {
            access_token: Some("pat-test".to_string()),
            default_portal: Some("nope".to_string()),
            ..HubspotSkillConfig::default()
        };
        assert_eq!(HubspotSkill::new(flat).unwrap().default_portal, "default");

        let named = HubspotSkillConfig {
            portals: HashMap::from([(
                "prod".to_string(),
                HubspotPortalConfig {
                    access_token: "pat-prod".to_string(),
                    max_results: 100,
                },
            )]),
            ..HubspotSkillConfig::default()
        };
        assert_eq!(HubspotSkill::new(named).unwrap().default_portal, "prod");
    }

    #[test]
    fn new_succeeds_single_portal() {
        let cfg = HubspotSkillConfig {
            access_token: Some("pat-test".to_string()),
            ..HubspotSkillConfig::default()
        };
        let skill = HubspotSkill::new(cfg).unwrap();
        let tools = skill.tools();
        assert_eq!(tools.len(), 40);
    }

    #[test]
    fn tool_names_matches_registered_tools() {
        let cfg = HubspotSkillConfig {
            access_token: Some("pat-test".to_string()),
            ..HubspotSkillConfig::default()
        };
        let skill = HubspotSkill::new(cfg).unwrap();
        let registered: Vec<String> = skill.tools().iter().map(|t| t.name().to_string()).collect();
        let declared: Vec<String> = HubspotSkill::tool_names()
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(
            registered, declared,
            "HubspotSkill::tool_names() drifted from tools()"
        );
    }

    fn placeholder_skill() -> HubspotSkill {
        HubspotSkill::new(HubspotSkillConfig {
            access_token: Some("pat-test".to_string()),
            ..HubspotSkillConfig::default()
        })
        .unwrap()
    }

    #[test]
    fn definition_matches_runtime() {
        let skill = placeholder_skill();
        let runtime: Vec<(String, String, serde_json::Value)> = skill
            .tools()
            .iter()
            .map(|t| {
                (
                    t.name().to_string(),
                    t.description().to_string(),
                    t.input_schema(),
                )
            })
            .collect();
        let def = HubspotSkill::definition();
        assert_eq!(
            def.tools.len(),
            runtime.len(),
            "definition() tool count drifted from tools()"
        );
        for (name, description, input_schema) in &runtime {
            let matching = def
                .tools
                .iter()
                .find(|t| &t.name == name)
                .unwrap_or_else(|| panic!("definition() missing tool: {name}"));
            assert_eq!(
                &matching.description, description,
                "description drifted for {name}"
            );
            assert_eq!(
                &matching.input_schema, input_schema,
                "input_schema drifted for {name}"
            );
        }
    }

    #[test]
    fn list_tools_cross_reference_crm_search() {
        let skill = placeholder_skill();
        let list_names = [
            "hubspot.contact_list",
            "hubspot.company_list",
            "hubspot.deal_list",
            "hubspot.ticket_list",
        ];
        let referenced: Vec<&str> = skill
            .tools()
            .iter()
            .filter(|t| list_names.contains(&t.name()))
            .filter(|t| t.description().contains("crm_search"))
            .map(|t| {
                let static_name: &'static str =
                    list_names.iter().copied().find(|n| *n == t.name()).unwrap();
                static_name
            })
            .collect();
        assert_eq!(
            referenced.len(),
            list_names.len(),
            "every *_list description must mention `crm_search` for discoverability; found only: {referenced:?}"
        );
    }

    #[test]
    fn crm_search_advertises_all_object_types() {
        let skill = placeholder_skill();
        let crm_search = skill
            .tools()
            .into_iter()
            .find(|t| t.name() == "hubspot.crm_search")
            .expect("hubspot.crm_search must exist");
        let desc = crm_search.description();
        for object_type in [
            "contacts",
            "companies",
            "deals",
            "tickets",
            "notes",
            "tasks",
            "calls",
            "emails",
            "meetings",
        ] {
            assert!(
                desc.contains(object_type),
                "crm_search must advertise `{object_type}` support; got: {desc}"
            );
        }
    }
}
