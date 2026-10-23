//! Datadog skill — agent-callable tools for the Datadog REST API.
//!
//! Authentication uses the per-org `DD-API-KEY` plus per-user
//! `DD-APPLICATION-KEY` headers. Multi-account support mirrors the GitHub
//! and Linear skills: configure named accounts in the vault and select with
//! the `account` parameter.
//!
//! Default policies split read tools (`allow`) from write/destructive tools
//! (`require_approval`) because Datadog ops actions have direct production
//! blast radius.

pub mod client;
pub mod config;
pub mod tools;

use std::collections::HashMap;
use std::sync::Arc;

use workforce_agent::tools::Tool;

use crate::adapters::utils::{build_instances, Instances};
use crate::definition::{
    CredentialRequirement, SkillDefinition, SkillPolicy, SkillRequirements, SkillTool,
};

pub use config::{DatadogAccountConfig, DatadogSkillConfig};

/// Read-only Datadog tools — granted the `allow` policy and the single source
/// of truth for the adapter's worker-safe read surface (see
/// [`crate::adapters::read_only_adapter_tools`]).
pub const READ_TOOLS: &[&str] = &[
    "datadog.metrics_query",
    "datadog.metrics_scalar",
    "datadog.metrics_list",
    "datadog.metrics_metadata",
    "datadog.logs_search",
    "datadog.logs_export",
    "datadog.logs_aggregate",
    "datadog.monitors_list",
    "datadog.monitors_get",
    "datadog.monitors_search",
    "datadog.dashboards_list",
    "datadog.dashboards_get",
    "datadog.events_list",
    "datadog.incidents_list",
    "datadog.incidents_get",
    "datadog.slos_list",
    "datadog.slos_get",
    "datadog.hosts_list",
    "datadog.downtimes_list",
    "datadog.synthetics_list",
];

/// Write/destructive Datadog tools — granted `require_approval`.
pub const WRITE_TOOLS: &[&str] = &[
    "datadog.monitors_create",
    "datadog.monitors_update",
    "datadog.monitors_delete",
    "datadog.monitors_mute",
    "datadog.monitors_unmute",
    "datadog.dashboards_create",
    "datadog.events_post",
    "datadog.hosts_mute",
    "datadog.hosts_unmute",
    "datadog.downtimes_create",
    "datadog.downtimes_cancel",
    "datadog.api",
];

/// The Datadog skill — provides tools for the Datadog REST API across
/// multiple accounts/regions.
pub struct DatadogSkill {
    clients: Arc<HashMap<String, Arc<client::DatadogClient>>>,
    default_account: String,
}

impl DatadogSkill {
    /// Build a `DatadogSkill` with static credentials. Use
    /// [`DatadogSkill::with_store`] for per-user credential resolution.
    pub fn new(config: DatadogSkillConfig) -> eyre::Result<Self> {
        Self::build(config, |_, acct| {
            client::DatadogClient::new(
                &acct.api_key,
                &acct.app_key,
                &acct.site,
                acct.base_url.as_deref(),
            )
        })
    }

    /// Build a `DatadogSkill` whose clients resolve api/app keys from
    /// `store` per request. Pass a `LayeredUserVault` for per-user
    /// resolution.
    pub fn with_store(
        config: DatadogSkillConfig,
        store: Arc<dyn workforce_common::settings::SecretStore>,
    ) -> eyre::Result<Self> {
        Self::build(config, |name, acct| {
            client::DatadogClient::with_store(
                Arc::clone(&store),
                name,
                &acct.site,
                acct.base_url.as_deref(),
            )
        })
    }

    fn build(
        config: DatadogSkillConfig,
        make: impl Fn(&str, &DatadogAccountConfig) -> eyre::Result<client::DatadogClient>,
    ) -> eyre::Result<Self> {
        let Instances { clients, default } = build_instances(
            "datadog",
            &config.resolve(),
            config.default_account.as_deref(),
            "No Datadog accounts configured. Set api_key/app_key or accounts.*.",
            make,
        )?;
        Ok(Self {
            clients,
            default_account: default,
        })
    }

    /// The instances this skill exposes, for the environments of the agent
    /// that loaded it.
    pub fn variants(&self) -> Option<workforce_agent::llm::environment::SkillVariants> {
        workforce_agent::llm::environment::SkillVariants::of(
            self.clients.keys().cloned(),
            &self.default_account,
        )
    }

    /// Return all agent-callable tools.
    pub fn tools(&self) -> Vec<Arc<dyn Tool>> {
        let c = || Arc::clone(&self.clients);
        let a = || self.default_account.clone();
        vec![
            Arc::new(tools::DatadogMetricsQueryTool::new(c(), a())),
            Arc::new(tools::DatadogMetricsScalarTool::new(c(), a())),
            Arc::new(tools::DatadogMetricsListTool::new(c(), a())),
            Arc::new(tools::DatadogMetricsMetadataTool::new(c(), a())),
            Arc::new(tools::DatadogLogsSearchTool::new(c(), a())),
            Arc::new(tools::DatadogLogsExportTool::new(c(), a())),
            Arc::new(tools::DatadogLogsAggregateTool::new(c(), a())),
            Arc::new(tools::DatadogMonitorsListTool::new(c(), a())),
            Arc::new(tools::DatadogMonitorsGetTool::new(c(), a())),
            Arc::new(tools::DatadogMonitorsSearchTool::new(c(), a())),
            Arc::new(tools::DatadogMonitorsCreateTool::new(c(), a())),
            Arc::new(tools::DatadogMonitorsUpdateTool::new(c(), a())),
            Arc::new(tools::DatadogMonitorsDeleteTool::new(c(), a())),
            Arc::new(tools::DatadogMonitorsMuteTool::new(c(), a())),
            Arc::new(tools::DatadogMonitorsUnmuteTool::new(c(), a())),
            Arc::new(tools::DatadogDashboardsListTool::new(c(), a())),
            Arc::new(tools::DatadogDashboardsGetTool::new(c(), a())),
            Arc::new(tools::DatadogDashboardsCreateTool::new(c(), a())),
            Arc::new(tools::DatadogEventsListTool::new(c(), a())),
            Arc::new(tools::DatadogEventsPostTool::new(c(), a())),
            Arc::new(tools::DatadogIncidentsListTool::new(c(), a())),
            Arc::new(tools::DatadogIncidentsGetTool::new(c(), a())),
            Arc::new(tools::DatadogSlosListTool::new(c(), a())),
            Arc::new(tools::DatadogSlosGetTool::new(c(), a())),
            Arc::new(tools::DatadogHostsListTool::new(c(), a())),
            Arc::new(tools::DatadogHostsMuteTool::new(c(), a())),
            Arc::new(tools::DatadogHostsUnmuteTool::new(c(), a())),
            Arc::new(tools::DatadogDowntimesListTool::new(c(), a())),
            Arc::new(tools::DatadogDowntimesCreateTool::new(c(), a())),
            Arc::new(tools::DatadogDowntimesCancelTool::new(c(), a())),
            Arc::new(tools::DatadogSyntheticsListTool::new(c(), a())),
            Arc::new(tools::DatadogApiTool::new(c(), a())),
        ]
    }

    /// Tool names provided by this skill, in registration order.
    pub fn tool_names() -> Vec<&'static str> {
        vec![
            "datadog.metrics_query",
            "datadog.metrics_scalar",
            "datadog.metrics_list",
            "datadog.metrics_metadata",
            "datadog.logs_search",
            "datadog.logs_export",
            "datadog.logs_aggregate",
            "datadog.monitors_list",
            "datadog.monitors_get",
            "datadog.monitors_search",
            "datadog.monitors_create",
            "datadog.monitors_update",
            "datadog.monitors_delete",
            "datadog.monitors_mute",
            "datadog.monitors_unmute",
            "datadog.dashboards_list",
            "datadog.dashboards_get",
            "datadog.dashboards_create",
            "datadog.events_list",
            "datadog.events_post",
            "datadog.incidents_list",
            "datadog.incidents_get",
            "datadog.slos_list",
            "datadog.slos_get",
            "datadog.hosts_list",
            "datadog.hosts_mute",
            "datadog.hosts_unmute",
            "datadog.downtimes_list",
            "datadog.downtimes_create",
            "datadog.downtimes_cancel",
            "datadog.synthetics_list",
            "datadog.api",
        ]
    }

    /// Skill definition for the Datadog integration.
    pub fn definition() -> SkillDefinition {
        let mk_tool = |name: &str, description: &str, schema: serde_json::Value| SkillTool {
            name: name.to_string(),
            description: description.to_string(),
            input_schema: schema,
        };
        let policy = |name: &str, action: &str| SkillPolicy {
            tool: name.to_string(),
            action: action.to_string(),
        };

        let default_policies = READ_TOOLS
            .iter()
            .map(|n| policy(n, "allow"))
            .chain(WRITE_TOOLS.iter().map(|n| policy(n, "require_approval")))
            .collect();

        SkillDefinition {
            name: "datadog-integration".to_string(),
            version: "1.0.0".to_string(),
            description:
                "Query Datadog metrics/logs and manage monitors, dashboards, downtimes, incidents, SLOs, and hosts"
                    .to_string(),
            author: "workforce".to_string(),
            license: "MIT".to_string(),

            tools: vec![
                mk_tool(
                    "datadog.metrics_query",
                    "Run a Datadog metrics timeseries query (v2)",
                    serde_json::json!({
                        "type": "object",
                        "properties": {
                            "account": { "type": "string" },
                            "query": { "type": "string" },
                            "from": {},
                            "to": {}
                        },
                        "required": ["query", "from", "to"]
                    }),
                ),
                mk_tool(
                    "datadog.metrics_scalar",
                    "Run a Datadog scalar metrics query (v2)",
                    serde_json::json!({
                        "type": "object",
                        "properties": {
                            "account": { "type": "string" },
                            "query": { "type": "string" },
                            "from": {},
                            "to": {},
                            "aggregator": { "type": "string" }
                        },
                        "required": ["query", "from", "to"]
                    }),
                ),
                mk_tool(
                    "datadog.metrics_list",
                    "List metric names that have reported data since a given time",
                    serde_json::json!({
                        "type": "object",
                        "properties": {
                            "account": { "type": "string" },
                            "from": {}
                        }
                    }),
                ),
                mk_tool(
                    "datadog.metrics_metadata",
                    "Get metadata for a single Datadog metric",
                    serde_json::json!({
                        "type": "object",
                        "properties": {
                            "account": { "type": "string" },
                            "metric_name": { "type": "string" }
                        },
                        "required": ["metric_name"]
                    }),
                ),
                mk_tool(
                    "datadog.logs_search",
                    "Search Datadog logs (v2) with cursor pagination",
                    serde_json::json!({
                        "type": "object",
                        "properties": {
                            "account": { "type": "string" },
                            "query": { "type": "string" },
                            "from": {},
                            "to": {},
                            "limit": { "type": "integer" },
                            "cursor": { "type": "string" }
                        },
                        "required": ["query"]
                    }),
                ),
                mk_tool(
                    "datadog.logs_export",
                    "Bulk-export every matching Datadog log in a window to an .xlsx or a staged tabular_artifact",
                    serde_json::json!({
                        "type": "object",
                        "properties": {
                            "account": { "type": "string" },
                            "query": { "type": "string" },
                            "from": {},
                            "to": {},
                            "max_records": { "type": "integer" },
                            "format": { "type": "string", "enum": ["xlsx", "artifact"] }
                        },
                        "required": ["query", "from"]
                    }),
                ),
                mk_tool(
                    "datadog.logs_aggregate",
                    "Aggregate Datadog logs by computed metrics and groupings",
                    serde_json::json!({
                        "type": "object",
                        "properties": {
                            "account": { "type": "string" },
                            "query": { "type": "string" },
                            "from": {},
                            "to": {},
                            "compute": { "type": "array" },
                            "group_by": { "type": "array" }
                        },
                        "required": ["query"]
                    }),
                ),
                mk_tool(
                    "datadog.monitors_list",
                    "List Datadog monitors",
                    serde_json::json!({
                        "type": "object",
                        "properties": {
                            "account": { "type": "string" },
                            "name": { "type": "string" },
                            "tags": { "type": "string" },
                            "monitor_tags": { "type": "string" },
                            "page_size": { "type": "integer" },
                            "page": { "type": "integer" }
                        }
                    }),
                ),
                mk_tool(
                    "datadog.monitors_get",
                    "Get full details for a Datadog monitor",
                    serde_json::json!({
                        "type": "object",
                        "properties": {
                            "account": { "type": "string" },
                            "monitor_id": { "type": "integer" }
                        },
                        "required": ["monitor_id"]
                    }),
                ),
                mk_tool(
                    "datadog.monitors_search",
                    "Search Datadog monitors",
                    serde_json::json!({
                        "type": "object",
                        "properties": {
                            "account": { "type": "string" },
                            "query": { "type": "string" },
                            "page_size": { "type": "integer" },
                            "page": { "type": "integer" }
                        },
                        "required": ["query"]
                    }),
                ),
                mk_tool(
                    "datadog.monitors_create",
                    "Create a new Datadog monitor",
                    serde_json::json!({
                        "type": "object",
                        "properties": {
                            "account": { "type": "string" },
                            "type": { "type": "string" },
                            "query": { "type": "string" },
                            "name": { "type": "string" },
                            "message": { "type": "string" },
                            "tags": { "type": "array", "items": { "type": "string" } },
                            "priority": { "type": "integer" },
                            "options": { "type": "object" }
                        },
                        "required": ["type", "query", "name", "message"]
                    }),
                ),
                mk_tool(
                    "datadog.monitors_update",
                    "Update an existing Datadog monitor",
                    serde_json::json!({
                        "type": "object",
                        "properties": {
                            "account": { "type": "string" },
                            "monitor_id": { "type": "integer" },
                            "name": { "type": "string" },
                            "query": { "type": "string" },
                            "message": { "type": "string" },
                            "tags": { "type": "array", "items": { "type": "string" } },
                            "priority": { "type": "integer" },
                            "options": { "type": "object" }
                        },
                        "required": ["monitor_id"]
                    }),
                ),
                mk_tool(
                    "datadog.monitors_delete",
                    "Delete a Datadog monitor",
                    serde_json::json!({
                        "type": "object",
                        "properties": {
                            "account": { "type": "string" },
                            "monitor_id": { "type": "integer" }
                        },
                        "required": ["monitor_id"]
                    }),
                ),
                mk_tool(
                    "datadog.monitors_mute",
                    "Mute a Datadog monitor",
                    serde_json::json!({
                        "type": "object",
                        "properties": {
                            "account": { "type": "string" },
                            "monitor_id": { "type": "integer" },
                            "scope": { "type": "string" },
                            "end": {}
                        },
                        "required": ["monitor_id"]
                    }),
                ),
                mk_tool(
                    "datadog.monitors_unmute",
                    "Unmute a Datadog monitor",
                    serde_json::json!({
                        "type": "object",
                        "properties": {
                            "account": { "type": "string" },
                            "monitor_id": { "type": "integer" },
                            "scope": { "type": "string" },
                            "all_scopes": { "type": "boolean" }
                        },
                        "required": ["monitor_id"]
                    }),
                ),
                mk_tool(
                    "datadog.dashboards_list",
                    "List Datadog dashboards",
                    serde_json::json!({
                        "type": "object",
                        "properties": { "account": { "type": "string" } }
                    }),
                ),
                mk_tool(
                    "datadog.dashboards_get",
                    "Get a Datadog dashboard definition",
                    serde_json::json!({
                        "type": "object",
                        "properties": {
                            "account": { "type": "string" },
                            "dashboard_id": { "type": "string" }
                        },
                        "required": ["dashboard_id"]
                    }),
                ),
                mk_tool(
                    "datadog.dashboards_create",
                    "Create a Datadog dashboard",
                    serde_json::json!({
                        "type": "object",
                        "properties": {
                            "account": { "type": "string" },
                            "title": { "type": "string" },
                            "layout_type": { "type": "string" },
                            "description": { "type": "string" },
                            "widgets": { "type": "array" },
                            "template_variables": { "type": "array" },
                            "tags": { "type": "array", "items": { "type": "string" } }
                        },
                        "required": ["title", "layout_type"]
                    }),
                ),
                mk_tool(
                    "datadog.events_list",
                    "List Datadog events in a time range",
                    serde_json::json!({
                        "type": "object",
                        "properties": {
                            "account": { "type": "string" },
                            "from": {},
                            "to": {},
                            "priority": { "type": "string" },
                            "sources": { "type": "string" },
                            "tags": { "type": "string" }
                        },
                        "required": ["from", "to"]
                    }),
                ),
                mk_tool(
                    "datadog.events_post",
                    "Post a custom event to the Datadog event stream",
                    serde_json::json!({
                        "type": "object",
                        "properties": {
                            "account": { "type": "string" },
                            "title": { "type": "string" },
                            "text": { "type": "string" },
                            "tags": { "type": "array", "items": { "type": "string" } },
                            "alert_type": { "type": "string" },
                            "priority": { "type": "string" }
                        },
                        "required": ["title", "text"]
                    }),
                ),
                mk_tool(
                    "datadog.incidents_list",
                    "List Datadog incidents",
                    serde_json::json!({
                        "type": "object",
                        "properties": {
                            "account": { "type": "string" },
                            "page_size": { "type": "integer" }
                        }
                    }),
                ),
                mk_tool(
                    "datadog.incidents_get",
                    "Get a single Datadog incident",
                    serde_json::json!({
                        "type": "object",
                        "properties": {
                            "account": { "type": "string" },
                            "incident_id": { "type": "string" }
                        },
                        "required": ["incident_id"]
                    }),
                ),
                mk_tool(
                    "datadog.slos_list",
                    "List Datadog SLOs",
                    serde_json::json!({
                        "type": "object",
                        "properties": {
                            "account": { "type": "string" },
                            "query": { "type": "string" },
                            "limit": { "type": "integer" }
                        }
                    }),
                ),
                mk_tool(
                    "datadog.slos_get",
                    "Get a single Datadog SLO",
                    serde_json::json!({
                        "type": "object",
                        "properties": {
                            "account": { "type": "string" },
                            "slo_id": { "type": "string" }
                        },
                        "required": ["slo_id"]
                    }),
                ),
                mk_tool(
                    "datadog.hosts_list",
                    "List Datadog hosts",
                    serde_json::json!({
                        "type": "object",
                        "properties": {
                            "account": { "type": "string" },
                            "filter": { "type": "string" },
                            "count": { "type": "integer" },
                            "start": { "type": "integer" }
                        }
                    }),
                ),
                mk_tool(
                    "datadog.hosts_mute",
                    "Mute a Datadog host",
                    serde_json::json!({
                        "type": "object",
                        "properties": {
                            "account": { "type": "string" },
                            "host_name": { "type": "string" },
                            "message": { "type": "string" },
                            "end": {},
                            "override": { "type": "boolean" }
                        },
                        "required": ["host_name"]
                    }),
                ),
                mk_tool(
                    "datadog.hosts_unmute",
                    "Unmute a Datadog host",
                    serde_json::json!({
                        "type": "object",
                        "properties": {
                            "account": { "type": "string" },
                            "host_name": { "type": "string" }
                        },
                        "required": ["host_name"]
                    }),
                ),
                mk_tool(
                    "datadog.downtimes_list",
                    "List Datadog downtimes",
                    serde_json::json!({
                        "type": "object",
                        "properties": {
                            "account": { "type": "string" },
                            "current_only": { "type": "boolean" }
                        }
                    }),
                ),
                mk_tool(
                    "datadog.downtimes_create",
                    "Schedule a new Datadog downtime",
                    serde_json::json!({
                        "type": "object",
                        "properties": {
                            "account": { "type": "string" },
                            "scope": { "type": "string" },
                            "monitor_id": { "type": "integer" },
                            "message": { "type": "string" },
                            "start": {},
                            "end": {}
                        },
                        "required": ["start", "end"]
                    }),
                ),
                mk_tool(
                    "datadog.downtimes_cancel",
                    "Cancel a Datadog downtime",
                    serde_json::json!({
                        "type": "object",
                        "properties": {
                            "account": { "type": "string" },
                            "downtime_id": { "type": "string" }
                        },
                        "required": ["downtime_id"]
                    }),
                ),
                mk_tool(
                    "datadog.synthetics_list",
                    "List Datadog Synthetic tests",
                    serde_json::json!({
                        "type": "object",
                        "properties": { "account": { "type": "string" } }
                    }),
                ),
                mk_tool(
                    "datadog.api",
                    "Make an arbitrary Datadog REST request",
                    serde_json::json!({
                        "type": "object",
                        "properties": {
                            "account": { "type": "string" },
                            "method": { "type": "string", "enum": ["GET", "POST", "PUT", "DELETE"] },
                            "path": { "type": "string" },
                            "body": { "type": "object" },
                            "query": { "type": "object" }
                        },
                        "required": ["method", "path"]
                    }),
                ),
            ],

            event_sources: vec![],

            default_policies,

            memory_schemas: Vec::new(),

            requirements: SkillRequirements {
                credentials: vec![
                    CredentialRequirement {
                        name: "datadog_api_key".to_string(),
                        r#type: "api_key".to_string(),
                        description: "Datadog API key (DD-API-KEY header)".to_string(),
                    },
                    CredentialRequirement {
                        name: "datadog_app_key".to_string(),
                        r#type: "api_key".to_string(),
                        description: "Datadog Application key (DD-APPLICATION-KEY header)"
                            .to_string(),
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

    #[test]
    fn skill_builds_with_flat_config() {
        let cfg = DatadogSkillConfig {
            api_key: Some("api".to_string()),
            app_key: Some("app".to_string()),
            site: Some("us1".to_string()),
            ..DatadogSkillConfig::default()
        };
        let skill = DatadogSkill::new(cfg).unwrap();
        let tools = skill.tools();
        assert_eq!(tools.len(), 32);
        for t in &tools {
            assert!(t.name().starts_with("datadog."));
        }
    }

    #[test]
    fn skill_errors_with_no_accounts() {
        assert!(DatadogSkill::new(DatadogSkillConfig::default()).is_err());
    }

    #[test]
    fn a_sole_named_account_becomes_the_default_without_a_selector() {
        let cfg = DatadogSkillConfig {
            accounts: HashMap::from([(
                "prod-eu".to_string(),
                DatadogAccountConfig {
                    api_key: "api".to_string(),
                    app_key: "app".to_string(),
                    site: "eu1".to_string(),
                    base_url: None,
                },
            )]),
            ..DatadogSkillConfig::default()
        };
        assert_eq!(DatadogSkill::new(cfg).unwrap().default_account, "prod-eu");
    }

    #[test]
    fn tool_names_matches_registered_tools() {
        let cfg = DatadogSkillConfig {
            api_key: Some("api".to_string()),
            app_key: Some("app".to_string()),
            site: Some("us1".to_string()),
            ..DatadogSkillConfig::default()
        };
        let skill = DatadogSkill::new(cfg).unwrap();
        let registered: Vec<String> = skill.tools().iter().map(|t| t.name().to_string()).collect();
        let declared: Vec<String> = DatadogSkill::tool_names()
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(
            registered, declared,
            "DatadogSkill::tool_names() drifted from tools()"
        );
    }

    #[test]
    fn definition_lists_all_32_tools() {
        let def = DatadogSkill::definition();
        assert_eq!(def.tools.len(), 32);
        assert_eq!(def.default_policies.len(), 32);
        assert_eq!(def.requirements.credentials.len(), 2);
        assert!(def.validate().is_ok());
    }

    #[test]
    fn definition_write_tools_require_approval() {
        let def = DatadogSkill::definition();
        let policies: HashMap<&str, &str> = def
            .default_policies
            .iter()
            .map(|p| (p.tool.as_str(), p.action.as_str()))
            .collect();

        for &t in WRITE_TOOLS {
            assert_eq!(
                policies.get(t),
                Some(&"require_approval"),
                "expected '{t}' to require_approval"
            );
        }
        for &t in READ_TOOLS {
            assert_eq!(policies.get(t), Some(&"allow"), "expected '{t}' to allow");
        }
    }

    #[test]
    fn read_and_write_tool_lists_cover_all_tools() {
        crate::adapters::test_support::assert_read_write_partition(
            DatadogSkill::tool_names(),
            READ_TOOLS,
            WRITE_TOOLS,
        );
    }
}
