//! Metabase skill — agent-callable tools for the Metabase REST API.
//!
//! Auth is via API key (`X-API-KEY`, Metabase v0.49+). Supports multi-instance
//! configuration via an optional `instance` parameter on every tool.

pub mod client;
pub mod config;
pub mod schema_prompt;
pub mod sync;
pub mod tools;

use std::collections::HashMap;
use std::sync::Arc;

use workforce_agent::tools::Tool;

use crate::adapters::utils::{build_instances, Instances};
use crate::definition::{
    CredentialRequirement, SkillDefinition, SkillPolicy, SkillRequirements, SkillTool,
};

pub use config::{MetabaseInstanceConfig, MetabaseSkillConfig};

/// Read-only Metabase tools — the worker-safe read surface fed to
/// [`crate::adapters::read_only_adapter_tools`]. Classification only: Metabase's
/// `default_policies` intentionally keep every tool at `allow`, so this list
/// does not alter approval behaviour.
pub const READ_TOOLS: &[&str] = &[
    "metabase.card_list",
    "metabase.card_get",
    "metabase.card_query",
    "metabase.dashboard_list",
    "metabase.dashboard_get",
    "metabase.dashboard_query",
    "metabase.collection_list",
    "metabase.collection_get",
    "metabase.collection_items",
    "metabase.database_list",
    "metabase.database_tables",
    "metabase.table_fields",
    "metabase.dataset_query_native",
    "metabase.search",
];

pub const WRITE_TOOLS: &[&str] = &[
    "metabase.card_create",
    "metabase.card_update",
    "metabase.card_archive",
    "metabase.dashboard_create",
    "metabase.dashboard_update",
    "metabase.dashboard_archive",
    "metabase.dashboard_add_cards",
    "metabase.dashboard_remove_card",
    "metabase.collection_create",
    "metabase.collection_update",
];

/// The Metabase skill — holds a client per configured instance.
pub struct MetabaseSkill {
    clients: Arc<HashMap<String, Arc<client::MetabaseClient>>>,
    default_instance: String,
}

impl MetabaseSkill {
    /// Build from config with static credentials. Use
    /// [`MetabaseSkill::with_store`] for per-user resolution.
    pub fn new(config: MetabaseSkillConfig) -> eyre::Result<Self> {
        Self::build(config, |name, inst| {
            let site_url = inst.site_url.as_deref().ok_or_else(|| {
                eyre::eyre!("Metabase instance '{name}' has no site_url configured")
            })?;
            client::MetabaseClient::new(&inst.api_key, site_url, inst.max_query_rows)
        })
    }

    /// Build a `MetabaseSkill` whose clients resolve their api_key from
    /// `store` per request. Pass a `LayeredUserVault` for per-user
    /// resolution.
    pub fn with_store(
        config: MetabaseSkillConfig,
        store: Arc<dyn workforce_common::settings::SecretStore>,
        schema_store: Option<Arc<dyn workforce_common::metabase_schema::MetabaseSchemaStore>>,
    ) -> eyre::Result<Self> {
        Self::build(config, |name, inst| {
            Ok(client::MetabaseClient::with_store(
                Arc::clone(&store),
                name,
                inst.site_url.as_deref(),
                inst.max_query_rows,
            )?
            .with_schema_store(schema_store.clone()))
        })
    }

    /// The instance clients, for the sync pass.
    pub fn clients(&self) -> Arc<HashMap<String, Arc<client::MetabaseClient>>> {
        Arc::clone(&self.clients)
    }

    fn build(
        config: MetabaseSkillConfig,
        make: impl Fn(&str, &MetabaseInstanceConfig) -> eyre::Result<client::MetabaseClient>,
    ) -> eyre::Result<Self> {
        let Instances { clients, default } = build_instances(
            "metabase",
            &config.resolve(),
            config.default_instance.as_deref(),
            "No Metabase instances configured. Set `api_key`+`site_url` or `[instances.<name>]`.",
            make,
        )?;
        Ok(Self {
            clients,
            default_instance: default,
        })
    }

    /// The instances this skill exposes, for the environments of the agent
    /// that loaded it.
    pub fn variants(&self) -> Option<workforce_agent::llm::environment::SkillVariants> {
        workforce_agent::llm::environment::SkillVariants::of(
            self.clients.keys().cloned(),
            &self.default_instance,
        )
    }

    /// All agent-callable tools. This is the point a client becomes a live
    /// tool client the hub publishes schema to; the sync pass's throwaway
    /// clients only ever call [`MetabaseSkill::clients`].
    pub fn tools(&self) -> Vec<Arc<dyn Tool>> {
        self.clients.values().for_each(client::track_live);
        let c = &self.clients;
        let d = &self.default_instance;
        vec![
            Arc::new(tools::CardListTool::new(Arc::clone(c), d.clone())),
            Arc::new(tools::CardGetTool::new(Arc::clone(c), d.clone())),
            Arc::new(tools::CardQueryTool::new(Arc::clone(c), d.clone())),
            Arc::new(tools::CardCreateTool::new(Arc::clone(c), d.clone())),
            Arc::new(tools::CardUpdateTool::new(Arc::clone(c), d.clone())),
            Arc::new(tools::CardArchiveTool::new(Arc::clone(c), d.clone())),
            Arc::new(tools::DashboardListTool::new(Arc::clone(c), d.clone())),
            Arc::new(tools::DashboardGetTool::new(Arc::clone(c), d.clone())),
            Arc::new(tools::DashboardQueryTool::new(Arc::clone(c), d.clone())),
            Arc::new(tools::DashboardCreateTool::new(Arc::clone(c), d.clone())),
            Arc::new(tools::DashboardUpdateTool::new(Arc::clone(c), d.clone())),
            Arc::new(tools::DashboardArchiveTool::new(Arc::clone(c), d.clone())),
            Arc::new(tools::DashboardAddCardsTool::new(Arc::clone(c), d.clone())),
            Arc::new(tools::DashboardRemoveCardTool::new(
                Arc::clone(c),
                d.clone(),
            )),
            Arc::new(tools::CollectionListTool::new(Arc::clone(c), d.clone())),
            Arc::new(tools::CollectionGetTool::new(Arc::clone(c), d.clone())),
            Arc::new(tools::CollectionItemsTool::new(Arc::clone(c), d.clone())),
            Arc::new(tools::CollectionCreateTool::new(Arc::clone(c), d.clone())),
            Arc::new(tools::CollectionUpdateTool::new(Arc::clone(c), d.clone())),
            Arc::new(tools::DatabaseListTool::new(Arc::clone(c), d.clone())),
            Arc::new(tools::DatabaseTablesTool::new(Arc::clone(c), d.clone())),
            Arc::new(tools::TableFieldsTool::new(Arc::clone(c), d.clone())),
            Arc::new(tools::DatasetQueryNativeTool::new(Arc::clone(c), d.clone())),
            Arc::new(tools::SearchTool::new(Arc::clone(c), d.clone())),
        ]
    }

    /// Tool names provided by this skill, in registration order.
    pub fn tool_names() -> Vec<&'static str> {
        vec![
            "metabase.card_list",
            "metabase.card_get",
            "metabase.card_query",
            "metabase.card_create",
            "metabase.card_update",
            "metabase.card_archive",
            "metabase.dashboard_list",
            "metabase.dashboard_get",
            "metabase.dashboard_query",
            "metabase.dashboard_create",
            "metabase.dashboard_update",
            "metabase.dashboard_archive",
            "metabase.dashboard_add_cards",
            "metabase.dashboard_remove_card",
            "metabase.collection_list",
            "metabase.collection_get",
            "metabase.collection_items",
            "metabase.collection_create",
            "metabase.collection_update",
            "metabase.database_list",
            "metabase.database_tables",
            "metabase.table_fields",
            "metabase.dataset_query_native",
            "metabase.search",
        ]
    }

    /// Static skill definition (metadata + policies).
    pub fn definition() -> SkillDefinition {
        let instance_param = || {
            serde_json::json!({
                "type": "string",
                "description": "Metabase instance name (multi-instance setups)"
            })
        };

        let tools = vec![
            SkillTool {
                name: "metabase.card_list".to_string(),
                description: "List saved questions (cards).".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "collection_id": { "type": "integer" },
                        "archived": { "type": "boolean" },
                        "limit": { "type": "integer" },
                        "instance": instance_param(),
                    }
                }),
            },
            SkillTool {
                name: "metabase.card_get".to_string(),
                description: "Get a card by ID.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": { "id": { "type": "integer" }, "instance": instance_param() },
                    "required": ["id"]
                }),
            },
            SkillTool {
                name: "metabase.card_query".to_string(),
                description: "Execute a saved question (card).".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "id": { "type": "integer" },
                        "parameters": { "type": "array" },
                        "limit": { "type": "integer" },
                        "instance": instance_param(),
                    },
                    "required": ["id"]
                }),
            },
            SkillTool {
                name: "metabase.card_create".to_string(),
                description: "Create a new card (mutating).".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "name": { "type": "string" },
                        "description": { "type": "string" },
                        "collection_id": { "type": "integer" },
                        "display": { "type": "string" },
                        "dataset_query": { "type": "object" },
                        "visualization_settings": { "type": "object" },
                        "instance": instance_param(),
                    },
                    "required": ["name", "dataset_query"]
                }),
            },
            SkillTool {
                name: "metabase.card_update".to_string(),
                description: "Update a card (mutating).".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "id": { "type": "integer" },
                        "name": { "type": "string" },
                        "description": { "type": "string" },
                        "collection_id": { "type": "integer" },
                        "display": { "type": "string" },
                        "dataset_query": { "type": "object" },
                        "visualization_settings": { "type": "object" },
                        "instance": instance_param(),
                    },
                    "required": ["id"]
                }),
            },
            SkillTool {
                name: "metabase.card_archive".to_string(),
                description: "Archive or restore a card (mutating).".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "id": { "type": "integer" },
                        "archived": { "type": "boolean" },
                        "instance": instance_param(),
                    },
                    "required": ["id"]
                }),
            },
            SkillTool {
                name: "metabase.dashboard_list".to_string(),
                description: "List dashboards.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "collection_id": { "type": "integer" },
                        "limit": { "type": "integer" },
                        "instance": instance_param(),
                    }
                }),
            },
            SkillTool {
                name: "metabase.dashboard_get".to_string(),
                description: "Get a dashboard by ID.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": { "id": { "type": "integer" }, "instance": instance_param() },
                    "required": ["id"]
                }),
            },
            SkillTool {
                name: "metabase.dashboard_query".to_string(),
                description: "Execute every card in a dashboard.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "id": { "type": "integer" },
                        "parameters": { "type": "array" },
                        "limit": { "type": "integer" },
                        "max_cards": { "type": "integer" },
                        "instance": instance_param(),
                    },
                    "required": ["id"]
                }),
            },
            SkillTool {
                name: "metabase.dashboard_create".to_string(),
                description: "Create a dashboard, optionally with cards/filters/tabs (mutating)."
                    .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "name": { "type": "string" },
                        "description": { "type": "string" },
                        "collection_id": { "type": "integer" },
                        "collection_position": { "type": "integer" },
                        "dashcards": { "type": "array", "items": { "type": "object" } },
                        "parameters": { "type": "array", "items": { "type": "object" } },
                        "tabs": { "type": "array", "items": { "type": "object" } },
                        "instance": instance_param(),
                    },
                    "required": ["name"]
                }),
            },
            SkillTool {
                name: "metabase.dashboard_update".to_string(),
                description: "Update a dashboard; `dashcards` replaces the layout (mutating)."
                    .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "id": { "type": "integer" },
                        "name": { "type": "string" },
                        "description": { "type": "string" },
                        "collection_id": { "type": "integer" },
                        "collection_position": { "type": "integer" },
                        "dashcards": { "type": "array", "items": { "type": "object" } },
                        "parameters": { "type": "array", "items": { "type": "object" } },
                        "tabs": { "type": "array", "items": { "type": "object" } },
                        "instance": instance_param(),
                    },
                    "required": ["id"]
                }),
            },
            SkillTool {
                name: "metabase.dashboard_archive".to_string(),
                description: "Archive or restore a dashboard (mutating).".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "id": { "type": "integer" },
                        "archived": { "type": "boolean" },
                        "instance": instance_param(),
                    },
                    "required": ["id"]
                }),
            },
            SkillTool {
                name: "metabase.dashboard_add_cards".to_string(),
                description: "Append cards to a dashboard, preserving the layout (mutating)."
                    .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "id": { "type": "integer" },
                        "cards": { "type": "array", "items": { "type": "object" } },
                        "instance": instance_param(),
                    },
                    "required": ["id", "cards"]
                }),
            },
            SkillTool {
                name: "metabase.dashboard_remove_card".to_string(),
                description: "Remove one card from a dashboard by dashcard id (mutating)."
                    .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "id": { "type": "integer" },
                        "dashcard_id": { "type": "integer" },
                        "instance": instance_param(),
                    },
                    "required": ["id", "dashcard_id"]
                }),
            },
            SkillTool {
                name: "metabase.collection_list".to_string(),
                description: "List collections.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "archived": { "type": "boolean" },
                        "instance": instance_param(),
                    }
                }),
            },
            SkillTool {
                name: "metabase.collection_get".to_string(),
                description: "Get a collection by ID (or 'root').".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": { "id": {}, "instance": instance_param() },
                    "required": ["id"]
                }),
            },
            SkillTool {
                name: "metabase.collection_items".to_string(),
                description: "List items inside a collection.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "id": {},
                        "models": { "type": "array", "items": { "type": "string" } },
                        "instance": instance_param(),
                    },
                    "required": ["id"]
                }),
            },
            SkillTool {
                name: "metabase.collection_create".to_string(),
                description: "Create a collection (mutating).".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "name": { "type": "string" },
                        "description": { "type": "string" },
                        "parent_id": { "type": "integer" },
                        "color": { "type": "string" },
                        "instance": instance_param(),
                    },
                    "required": ["name"]
                }),
            },
            SkillTool {
                name: "metabase.collection_update".to_string(),
                description: "Update a collection (mutating).".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "id": { "type": "integer" },
                        "name": { "type": "string" },
                        "description": { "type": "string" },
                        "parent_id": { "type": "integer" },
                        "archived": { "type": "boolean" },
                        "instance": instance_param(),
                    },
                    "required": ["id"]
                }),
            },
            SkillTool {
                name: "metabase.database_list".to_string(),
                description: "List databases.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": { "instance": instance_param() }
                }),
            },
            SkillTool {
                name: "metabase.database_tables".to_string(),
                description: "List tables in a database.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "database_id": { "type": "integer" },
                        "instance": instance_param(),
                    },
                    "required": ["database_id"]
                }),
            },
            SkillTool {
                name: "metabase.table_fields".to_string(),
                description: "List fields (columns) for a table.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "table_id": { "type": "integer" },
                        "instance": instance_param(),
                    },
                    "required": ["table_id"]
                }),
            },
            SkillTool {
                name: "metabase.dataset_query_native".to_string(),
                description: "Run raw SQL against a database (mutating execute; row-capped)."
                    .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "database_id": { "type": "integer" },
                        "query": { "type": "string" },
                        "template_tags": { "type": "object" },
                        "parameters": { "type": "array" },
                        "limit": { "type": "integer" },
                        "instance": instance_param(),
                    },
                    "required": ["database_id", "query"]
                }),
            },
            SkillTool {
                name: "metabase.search".to_string(),
                description: "Global search.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "query": { "type": "string" },
                        "models": { "type": "array", "items": { "type": "string" } },
                        "limit": { "type": "integer" },
                        "instance": instance_param(),
                    },
                    "required": ["query"]
                }),
            },
        ];

        let default_policies: Vec<SkillPolicy> = tools
            .iter()
            .map(|t| SkillPolicy {
                tool: t.name.clone(),
                action: "allow".to_string(),
            })
            .collect();

        SkillDefinition {
            name: "metabase-integration".to_string(),
            version: "1.0.0".to_string(),
            description: "Interact with Metabase cards, dashboards, collections, databases, and run native SQL.".to_string(),
            author: "workforce".to_string(),
            license: "MIT".to_string(),
            tools,
            event_sources: Vec::new(),
            default_policies,
            memory_schemas: Vec::new(),
            requirements: SkillRequirements {
                credentials: vec![CredentialRequirement {
                    name: "metabase_api_key".to_string(),
                    r#type: "api_key".to_string(),
                    description: "Metabase API key (X-API-KEY header)".to_string(),
                }],
                config: Vec::new(),
            },
            dependencies: Vec::new(),
        }
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::sync::Mutex;

    use workforce_common::metabase_schema::{
        DbSchema, InstanceSchema, MetabaseSchemaStore, TableUsage,
    };

    #[derive(Default)]
    pub(crate) struct FakeSchemaStore {
        pub(crate) upserts: Mutex<Vec<(i64, DbSchema)>>,
        pub(crate) pruned: Mutex<Vec<Vec<i64>>>,
        pub(crate) usage: Mutex<Vec<(i64, String, TableUsage)>>,
        pub(crate) engines: Mutex<Vec<(i64, String)>>,
        pub(crate) loaded: Mutex<InstanceSchema>,
        pub(crate) fail_upsert: bool,
        /// The `(instance, site_url)` every load and write was keyed by.
        pub(crate) keys: Mutex<Vec<(String, String)>>,
    }

    impl FakeSchemaStore {
        fn key(&self, instance: &str, site_url: &str) {
            self.keys
                .lock()
                .unwrap()
                .push((instance.to_string(), site_url.to_string()));
        }
    }

    #[async_trait::async_trait]
    impl MetabaseSchemaStore for FakeSchemaStore {
        async fn load(&self, instance: &str, site_url: &str) -> eyre::Result<InstanceSchema> {
            self.key(instance, site_url);
            Ok(self.loaded.lock().unwrap().clone())
        }
        async fn upsert_database(
            &self,
            instance: &str,
            site_url: &str,
            database_id: i64,
            schema: &DbSchema,
        ) -> eyre::Result<()> {
            self.key(instance, site_url);
            if self.fail_upsert {
                return Err(eyre::eyre!("store down"));
            }
            self.upserts
                .lock()
                .unwrap()
                .push((database_id, schema.clone()));
            Ok(())
        }
        async fn upsert_engines(
            &self,
            _: &str,
            _: &str,
            engines: &[(i64, String)],
        ) -> eyre::Result<()> {
            self.engines.lock().unwrap().extend_from_slice(engines);
            Ok(())
        }
        async fn prune_databases(&self, _: &str, keep: &[i64]) -> eyre::Result<()> {
            self.pruned.lock().unwrap().push(keep.to_vec());
            Ok(())
        }
        async fn add_usage(
            &self,
            instance: &str,
            deltas: &[(i64, String, TableUsage)],
        ) -> eyre::Result<()> {
            self.key(instance, "");
            self.usage.lock().unwrap().extend_from_slice(deltas);
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn definition_lists_all_tools() {
        let def = MetabaseSkill::definition();
        assert_eq!(def.name, "metabase-integration");
        assert!(def.tools.len() >= 22);
        let names: Vec<&str> = def.tools.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"metabase.card_query"));
        assert!(names.contains(&"metabase.dataset_query_native"));
        assert!(names.contains(&"metabase.dashboard_query"));
        // All tools have a default policy.
        assert_eq!(def.default_policies.len(), def.tools.len());
    }

    #[test]
    fn new_errors_when_no_instances() {
        let err = MetabaseSkill::new(MetabaseSkillConfig::default())
            .err()
            .expect("expected error");
        assert!(err.to_string().contains("No Metabase instances"));
    }

    fn named(names: &[&str], default_instance: Option<&str>) -> MetabaseSkillConfig {
        MetabaseSkillConfig {
            instances: names
                .iter()
                .map(|name| {
                    (
                        name.to_string(),
                        MetabaseInstanceConfig {
                            api_key: format!("mb_{name}"),
                            site_url: Some(format!("https://mb.{name}.example.com")),
                            max_query_rows: 2000,
                        },
                    )
                })
                .collect(),
            default_instance: default_instance.map(str::to_string),
            ..MetabaseSkillConfig::default()
        }
    }

    #[test]
    fn a_sole_named_instance_becomes_the_default_even_over_a_stale_selector() {
        let skill = MetabaseSkill::new(named(&["fabrikam"], None)).unwrap();
        assert_eq!(skill.default_instance, "fabrikam");
        let skill = MetabaseSkill::new(named(&["fabrikam"], Some("nope"))).unwrap();
        assert_eq!(skill.default_instance, "fabrikam");
    }

    #[test]
    fn several_instances_without_a_selector_load_and_keep_default_for_the_call_error() {
        let skill = MetabaseSkill::new(named(&["prod", "staging"], None)).unwrap();
        assert_eq!(skill.default_instance, "default");
        let skill = MetabaseSkill::new(named(&["prod", "staging"], Some("staging"))).unwrap();
        assert_eq!(skill.default_instance, "staging");
    }

    #[test]
    fn new_succeeds_single_instance() {
        let cfg = MetabaseSkillConfig {
            api_key: Some("mb_x".to_string()),
            site_url: Some("https://mb.example.com".to_string()),
            ..MetabaseSkillConfig::default()
        };
        let skill = MetabaseSkill::new(cfg).unwrap();
        let tools = skill.tools();
        assert!(tools.len() >= 22);
    }

    #[test]
    fn tool_names_matches_registered_tools() {
        let cfg = MetabaseSkillConfig {
            api_key: Some("mb_x".to_string()),
            site_url: Some("https://mb.example.com".to_string()),
            ..MetabaseSkillConfig::default()
        };
        let skill = MetabaseSkill::new(cfg).unwrap();
        let registered: Vec<String> = skill.tools().iter().map(|t| t.name().to_string()).collect();
        let declared: Vec<String> = MetabaseSkill::tool_names()
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(
            registered, declared,
            "MetabaseSkill::tool_names() drifted from tools()"
        );
    }

    #[test]
    fn read_and_write_tool_lists_cover_all_tools() {
        crate::adapters::test_support::assert_read_write_partition(
            MetabaseSkill::tool_names(),
            READ_TOOLS,
            WRITE_TOOLS,
        );
    }
}
