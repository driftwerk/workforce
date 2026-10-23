//! Postmark skill — agent-callable tools for the Postmark REST API.
//!
//! Authentication uses two header tokens:
//! - `X-Postmark-Server-Token` — required. Covers send, messages, stats,
//!   bounces, templates, streams, suppressions.
//! - `X-Postmark-Account-Token` — optional. Covers listing servers, domains,
//!   and sender signatures; without it the account-scoped tools (servers_list,
//!   domains_list, domain_get, and `postmark.api` with `token_type=account`)
//!   return a clear runtime error instead of crashing at load time.
//!
//! Multi-workspace support mirrors the Datadog / Jira / Linear / Metabase
//! pattern: each named workspace holds its own server + account token pair.
//! Routing is via an explicit `workspace` parameter or the configured default
//! — no prefix auto-routing (Postmark IDs have no stable per-tenant prefix).
//!
//! Default policies split read tools (`allow`) from write / side-effecting
//! tools (`require_approval`): sending email, adding/removing suppressions,
//! activating bounces, and the generic `postmark.api` escape hatch all
//! require approval, matching Datadog's convention.

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

pub use config::{PostmarkSkillConfig, PostmarkWorkspaceConfig};

/// Read-only Postmark tools — granted `allow` and the worker-safe read surface
/// fed to [`crate::adapters::read_only_adapter_tools`].
pub const READ_TOOLS: &[&str] = &[
    "postmark.messages_outbound_search",
    "postmark.message_outbound_get",
    "postmark.message_outbound_dump",
    "postmark.messages_outbound_opens",
    "postmark.messages_outbound_clicks",
    "postmark.messages_inbound_search",
    "postmark.message_inbound_get",
    "postmark.bounces_list",
    "postmark.bounce_get",
    "postmark.delivery_stats",
    "postmark.stats_outbound",
    "postmark.stats_bounces",
    "postmark.stats_opens",
    "postmark.stats_clicks",
    "postmark.templates_list",
    "postmark.template_get",
    "postmark.message_streams_list",
    "postmark.suppressions_dump",
    "postmark.servers_list",
    "postmark.domains_list",
    "postmark.domain_get",
];

/// Write/sending Postmark tools — granted `require_approval`.
pub const WRITE_TOOLS: &[&str] = &[
    "postmark.email_send",
    "postmark.email_send_batch",
    "postmark.email_send_template",
    "postmark.bounce_activate",
    "postmark.suppressions_add",
    "postmark.suppressions_delete",
    "postmark.api",
];

/// The Postmark skill — provides tools for the Postmark REST API across
/// multiple server/account token pairs.
pub struct PostmarkSkill {
    clients: Arc<HashMap<String, Arc<client::PostmarkClient>>>,
    default_workspace: String,
}

impl PostmarkSkill {
    /// Build a `PostmarkSkill` with static credentials. Use
    /// [`PostmarkSkill::with_store`] for per-user resolution.
    pub fn new(config: PostmarkSkillConfig) -> eyre::Result<Self> {
        Self::build(config, |_, ws| {
            client::PostmarkClient::new(
                &ws.server_token,
                ws.account_token.as_deref(),
                ws.base_url.as_deref(),
            )
        })
    }

    /// Build a `PostmarkSkill` whose clients resolve credentials from
    /// `store` per request. Pass a `LayeredUserVault` for per-user
    /// resolution.
    pub fn with_store(
        config: PostmarkSkillConfig,
        store: Arc<dyn workforce_common::settings::SecretStore>,
    ) -> eyre::Result<Self> {
        Self::build(config, |name, ws| {
            client::PostmarkClient::with_store(Arc::clone(&store), name, ws.base_url.as_deref())
        })
    }

    fn build(
        config: PostmarkSkillConfig,
        make: impl Fn(&str, &PostmarkWorkspaceConfig) -> eyre::Result<client::PostmarkClient>,
    ) -> eyre::Result<Self> {
        let Instances { clients, default } = build_instances(
            "postmark",
            &config.resolve(),
            config.default_workspace.as_deref(),
            "No Postmark workspaces configured. Set server_token or workspaces.*.",
            make,
        )?;
        Ok(Self {
            clients,
            default_workspace: default,
        })
    }

    /// The instances this skill exposes, for the environments of the agent
    /// that loaded it.
    pub fn variants(&self) -> Option<workforce_agent::llm::environment::SkillVariants> {
        workforce_agent::llm::environment::SkillVariants::of(
            self.clients.keys().cloned(),
            &self.default_workspace,
        )
    }

    /// Return all agent-callable tools.
    pub fn tools(&self) -> Vec<Arc<dyn Tool>> {
        let c = || Arc::clone(&self.clients);
        let w = || self.default_workspace.clone();
        vec![
            Arc::new(tools::PostmarkMessagesOutboundSearchTool::new(c(), w())),
            Arc::new(tools::PostmarkMessageOutboundGetTool::new(c(), w())),
            Arc::new(tools::PostmarkMessageOutboundDumpTool::new(c(), w())),
            Arc::new(tools::PostmarkMessagesOutboundOpensTool::new(c(), w())),
            Arc::new(tools::PostmarkMessagesOutboundClicksTool::new(c(), w())),
            Arc::new(tools::PostmarkMessagesInboundSearchTool::new(c(), w())),
            Arc::new(tools::PostmarkMessageInboundGetTool::new(c(), w())),
            Arc::new(tools::PostmarkEmailSendTool::new(c(), w())),
            Arc::new(tools::PostmarkEmailSendBatchTool::new(c(), w())),
            Arc::new(tools::PostmarkEmailSendTemplateTool::new(c(), w())),
            Arc::new(tools::PostmarkBouncesListTool::new(c(), w())),
            Arc::new(tools::PostmarkBounceGetTool::new(c(), w())),
            Arc::new(tools::PostmarkBounceActivateTool::new(c(), w())),
            Arc::new(tools::PostmarkDeliveryStatsTool::new(c(), w())),
            Arc::new(tools::PostmarkStatsOutboundTool::new(c(), w())),
            Arc::new(tools::PostmarkStatsBouncesTool::new(c(), w())),
            Arc::new(tools::PostmarkStatsOpensTool::new(c(), w())),
            Arc::new(tools::PostmarkStatsClicksTool::new(c(), w())),
            Arc::new(tools::PostmarkTemplatesListTool::new(c(), w())),
            Arc::new(tools::PostmarkTemplateGetTool::new(c(), w())),
            Arc::new(tools::PostmarkMessageStreamsListTool::new(c(), w())),
            Arc::new(tools::PostmarkSuppressionsDumpTool::new(c(), w())),
            Arc::new(tools::PostmarkSuppressionsAddTool::new(c(), w())),
            Arc::new(tools::PostmarkSuppressionsDeleteTool::new(c(), w())),
            Arc::new(tools::PostmarkServersListTool::new(c(), w())),
            Arc::new(tools::PostmarkDomainsListTool::new(c(), w())),
            Arc::new(tools::PostmarkDomainGetTool::new(c(), w())),
            Arc::new(tools::PostmarkApiTool::new(c(), w())),
        ]
    }

    /// Tool names provided by this skill, in registration order.
    pub fn tool_names() -> Vec<&'static str> {
        vec![
            "postmark.messages_outbound_search",
            "postmark.message_outbound_get",
            "postmark.message_outbound_dump",
            "postmark.messages_outbound_opens",
            "postmark.messages_outbound_clicks",
            "postmark.messages_inbound_search",
            "postmark.message_inbound_get",
            "postmark.email_send",
            "postmark.email_send_batch",
            "postmark.email_send_template",
            "postmark.bounces_list",
            "postmark.bounce_get",
            "postmark.bounce_activate",
            "postmark.delivery_stats",
            "postmark.stats_outbound",
            "postmark.stats_bounces",
            "postmark.stats_opens",
            "postmark.stats_clicks",
            "postmark.templates_list",
            "postmark.template_get",
            "postmark.message_streams_list",
            "postmark.suppressions_dump",
            "postmark.suppressions_add",
            "postmark.suppressions_delete",
            "postmark.servers_list",
            "postmark.domains_list",
            "postmark.domain_get",
            "postmark.api",
        ]
    }

    /// Skill definition for the Postmark integration.
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
            name: "postmark-integration".to_string(),
            version: "1.0.0".to_string(),
            description:
                "Send transactional email, inspect delivery status, and manage bounces, stats, templates, streams, and suppressions via the Postmark REST API across multiple servers"
                    .to_string(),
            author: "workforce".to_string(),
            license: "MIT".to_string(),

            tools: vec![
                mk_tool(
                    "postmark.messages_outbound_search",
                    "Search sent Postmark messages by recipient, sender, tag, subject, status, stream, or date range",
                    serde_json::json!({
                        "type": "object",
                        "properties": {
                            "workspace":     { "type": "string" },
                            "recipient":     { "type": "string" },
                            "fromemail":     { "type": "string" },
                            "tag":           { "type": "string" },
                            "status":        { "type": "string", "enum": ["queued", "sent"] },
                            "subject":       { "type": "string" },
                            "messagestream": { "type": "string" },
                            "metadata":      { "type": "object" },
                            "fromdate":      { "type": "string" },
                            "todate":        { "type": "string" },
                            "count":         { "type": "integer" },
                            "offset":        { "type": "integer" }
                        }
                    }),
                ),
                mk_tool(
                    "postmark.message_outbound_get",
                    "Get full details of a sent Postmark message",
                    serde_json::json!({
                        "type": "object",
                        "properties": {
                            "workspace":  { "type": "string" },
                            "message_id": { "type": "string" }
                        },
                        "required": ["message_id"]
                    }),
                ),
                mk_tool(
                    "postmark.message_outbound_dump",
                    "Get the raw MIME body of a sent message",
                    serde_json::json!({
                        "type": "object",
                        "properties": {
                            "workspace":  { "type": "string" },
                            "message_id": { "type": "string" }
                        },
                        "required": ["message_id"]
                    }),
                ),
                mk_tool(
                    "postmark.messages_outbound_opens",
                    "List open events across messages or scoped to one MessageID",
                    serde_json::json!({
                        "type": "object",
                        "properties": {
                            "workspace":     { "type": "string" },
                            "message_id":    { "type": "string" },
                            "recipient":     { "type": "string" },
                            "tag":           { "type": "string" },
                            "client_name":   { "type": "string" },
                            "os_family":     { "type": "string" },
                            "platform":      { "type": "string" },
                            "country":       { "type": "string" },
                            "messagestream": { "type": "string" },
                            "fromdate":      { "type": "string" },
                            "todate":        { "type": "string" },
                            "count":         { "type": "integer" },
                            "offset":        { "type": "integer" }
                        }
                    }),
                ),
                mk_tool(
                    "postmark.messages_outbound_clicks",
                    "List click-tracking events across messages or scoped to one MessageID",
                    serde_json::json!({
                        "type": "object",
                        "properties": {
                            "workspace":     { "type": "string" },
                            "message_id":    { "type": "string" },
                            "recipient":     { "type": "string" },
                            "tag":           { "type": "string" },
                            "client_name":   { "type": "string" },
                            "os_family":     { "type": "string" },
                            "platform":      { "type": "string" },
                            "country":       { "type": "string" },
                            "messagestream": { "type": "string" },
                            "fromdate":      { "type": "string" },
                            "todate":        { "type": "string" },
                            "count":         { "type": "integer" },
                            "offset":        { "type": "integer" }
                        }
                    }),
                ),
                mk_tool(
                    "postmark.messages_inbound_search",
                    "Search inbound Postmark messages",
                    serde_json::json!({
                        "type": "object",
                        "properties": {
                            "workspace":   { "type": "string" },
                            "recipient":   { "type": "string" },
                            "fromemail":   { "type": "string" },
                            "tag":         { "type": "string" },
                            "subject":     { "type": "string" },
                            "mailboxhash": { "type": "string" },
                            "status":      { "type": "string" },
                            "fromdate":    { "type": "string" },
                            "todate":      { "type": "string" },
                            "count":       { "type": "integer" },
                            "offset":      { "type": "integer" }
                        }
                    }),
                ),
                mk_tool(
                    "postmark.message_inbound_get",
                    "Get full details of a single inbound message",
                    serde_json::json!({
                        "type": "object",
                        "properties": {
                            "workspace":  { "type": "string" },
                            "message_id": { "type": "string" }
                        },
                        "required": ["message_id"]
                    }),
                ),
                mk_tool(
                    "postmark.email_send",
                    "Send a single transactional email",
                    serde_json::json!({
                        "type": "object",
                        "properties": {
                            "workspace":     { "type": "string" },
                            "From":          { "type": "string" },
                            "To":            { "type": "string" },
                            "Cc":            { "type": "string" },
                            "Bcc":           { "type": "string" },
                            "Subject":       { "type": "string" },
                            "Tag":           { "type": "string" },
                            "ReplyTo":       { "type": "string" },
                            "HtmlBody":      { "type": "string" },
                            "TextBody":      { "type": "string" },
                            "TrackOpens":    { "type": "boolean" },
                            "TrackLinks":    { "type": "string" },
                            "Metadata":      { "type": "object" },
                            "Headers":       { "type": "array" },
                            "Attachments":   { "type": "array" },
                            "MessageStream": { "type": "string" }
                        },
                        "required": ["From", "To", "Subject"]
                    }),
                ),
                mk_tool(
                    "postmark.email_send_batch",
                    "Send up to 500 transactional emails in a single batch",
                    serde_json::json!({
                        "type": "object",
                        "properties": {
                            "workspace": { "type": "string" },
                            "messages":  { "type": "array" }
                        },
                        "required": ["messages"]
                    }),
                ),
                mk_tool(
                    "postmark.email_send_template",
                    "Send a transactional email rendered from a Postmark template",
                    serde_json::json!({
                        "type": "object",
                        "properties": {
                            "workspace":     { "type": "string" },
                            "From":          { "type": "string" },
                            "To":            { "type": "string" },
                            "Cc":            { "type": "string" },
                            "Bcc":           { "type": "string" },
                            "Tag":           { "type": "string" },
                            "ReplyTo":       { "type": "string" },
                            "TrackOpens":    { "type": "boolean" },
                            "TrackLinks":    { "type": "string" },
                            "TemplateId":    { "type": "integer" },
                            "TemplateAlias": { "type": "string" },
                            "TemplateModel": { "type": "object" },
                            "InlineCss":     { "type": "boolean" },
                            "Metadata":      { "type": "object" },
                            "Headers":       { "type": "array" },
                            "Attachments":   { "type": "array" },
                            "MessageStream": { "type": "string" }
                        },
                        "required": ["From", "To", "TemplateModel"]
                    }),
                ),
                mk_tool(
                    "postmark.bounces_list",
                    "List bounces with filters",
                    serde_json::json!({
                        "type": "object",
                        "properties": {
                            "workspace":     { "type": "string" },
                            "type":          { "type": "string" },
                            "inactive":      { "type": "boolean" },
                            "emailFilter":   { "type": "string" },
                            "tag":           { "type": "string" },
                            "messageID":     { "type": "string" },
                            "messagestream": { "type": "string" },
                            "fromdate":      { "type": "string" },
                            "todate":        { "type": "string" },
                            "count":         { "type": "integer" },
                            "offset":        { "type": "integer" }
                        }
                    }),
                ),
                mk_tool(
                    "postmark.bounce_get",
                    "Get details of a single bounce",
                    serde_json::json!({
                        "type": "object",
                        "properties": {
                            "workspace": { "type": "string" },
                            "bounce_id": { "type": "integer" }
                        },
                        "required": ["bounce_id"]
                    }),
                ),
                mk_tool(
                    "postmark.bounce_activate",
                    "Re-activate a suppressed recipient",
                    serde_json::json!({
                        "type": "object",
                        "properties": {
                            "workspace": { "type": "string" },
                            "bounce_id": { "type": "integer" }
                        },
                        "required": ["bounce_id"]
                    }),
                ),
                mk_tool(
                    "postmark.delivery_stats",
                    "Rollup of delivery counts by bounce type and inactive mail count",
                    serde_json::json!({
                        "type": "object",
                        "properties": { "workspace": { "type": "string" } }
                    }),
                ),
                mk_tool(
                    "postmark.stats_outbound",
                    "Outbound sending stats summary",
                    serde_json::json!({
                        "type": "object",
                        "properties": {
                            "workspace":     { "type": "string" },
                            "tag":           { "type": "string" },
                            "fromdate":      { "type": "string" },
                            "todate":        { "type": "string" },
                            "messagestream": { "type": "string" }
                        }
                    }),
                ),
                mk_tool(
                    "postmark.stats_bounces",
                    "Bounce counts broken down by type",
                    serde_json::json!({
                        "type": "object",
                        "properties": {
                            "workspace":     { "type": "string" },
                            "tag":           { "type": "string" },
                            "fromdate":      { "type": "string" },
                            "todate":        { "type": "string" },
                            "messagestream": { "type": "string" }
                        }
                    }),
                ),
                mk_tool(
                    "postmark.stats_opens",
                    "Open-tracking totals across a time range",
                    serde_json::json!({
                        "type": "object",
                        "properties": {
                            "workspace":     { "type": "string" },
                            "tag":           { "type": "string" },
                            "fromdate":      { "type": "string" },
                            "todate":        { "type": "string" },
                            "messagestream": { "type": "string" }
                        }
                    }),
                ),
                mk_tool(
                    "postmark.stats_clicks",
                    "Click-tracking totals across a time range",
                    serde_json::json!({
                        "type": "object",
                        "properties": {
                            "workspace":     { "type": "string" },
                            "tag":           { "type": "string" },
                            "fromdate":      { "type": "string" },
                            "todate":        { "type": "string" },
                            "messagestream": { "type": "string" }
                        }
                    }),
                ),
                mk_tool(
                    "postmark.templates_list",
                    "List Postmark templates on the current server",
                    serde_json::json!({
                        "type": "object",
                        "properties": {
                            "workspace":      { "type": "string" },
                            "count":          { "type": "integer" },
                            "offset":         { "type": "integer" },
                            "templateType":   { "type": "string" },
                            "layoutTemplate": { "type": "string" }
                        }
                    }),
                ),
                mk_tool(
                    "postmark.template_get",
                    "Get a single Postmark template by ID or alias",
                    serde_json::json!({
                        "type": "object",
                        "properties": {
                            "workspace":   { "type": "string" },
                            "id_or_alias": { "type": "string" }
                        },
                        "required": ["id_or_alias"]
                    }),
                ),
                mk_tool(
                    "postmark.message_streams_list",
                    "List message streams on the current server",
                    serde_json::json!({
                        "type": "object",
                        "properties": {
                            "workspace":              { "type": "string" },
                            "MessageStreamType":      { "type": "string" },
                            "IncludeArchivedStreams": { "type": "boolean" }
                        }
                    }),
                ),
                mk_tool(
                    "postmark.suppressions_dump",
                    "List suppressions on a given message stream",
                    serde_json::json!({
                        "type": "object",
                        "properties": {
                            "workspace":         { "type": "string" },
                            "stream_id":         { "type": "string" },
                            "SuppressionReason": { "type": "string" },
                            "Origin":            { "type": "string" },
                            "fromdate":          { "type": "string" },
                            "todate":            { "type": "string" },
                            "emailaddress":      { "type": "string" }
                        },
                        "required": ["stream_id"]
                    }),
                ),
                mk_tool(
                    "postmark.suppressions_add",
                    "Add email addresses to a stream's suppression list",
                    serde_json::json!({
                        "type": "object",
                        "properties": {
                            "workspace":    { "type": "string" },
                            "stream_id":    { "type": "string" },
                            "Suppressions": { "type": "array" }
                        },
                        "required": ["stream_id", "Suppressions"]
                    }),
                ),
                mk_tool(
                    "postmark.suppressions_delete",
                    "Remove email addresses from a stream's suppression list (POST, not DELETE)",
                    serde_json::json!({
                        "type": "object",
                        "properties": {
                            "workspace":    { "type": "string" },
                            "stream_id":    { "type": "string" },
                            "Suppressions": { "type": "array" }
                        },
                        "required": ["stream_id", "Suppressions"]
                    }),
                ),
                mk_tool(
                    "postmark.servers_list",
                    "List Postmark servers on the account (requires account_token)",
                    serde_json::json!({
                        "type": "object",
                        "properties": {
                            "workspace": { "type": "string" },
                            "count":     { "type": "integer" },
                            "offset":    { "type": "integer" },
                            "name":      { "type": "string" }
                        }
                    }),
                ),
                mk_tool(
                    "postmark.domains_list",
                    "List sender domains (requires account_token)",
                    serde_json::json!({
                        "type": "object",
                        "properties": {
                            "workspace": { "type": "string" },
                            "count":     { "type": "integer" },
                            "offset":    { "type": "integer" }
                        }
                    }),
                ),
                mk_tool(
                    "postmark.domain_get",
                    "Get sender domain details (requires account_token)",
                    serde_json::json!({
                        "type": "object",
                        "properties": {
                            "workspace": { "type": "string" },
                            "domain_id": { "type": "integer" }
                        },
                        "required": ["domain_id"]
                    }),
                ),
                mk_tool(
                    "postmark.api",
                    "Make an arbitrary Postmark REST API request",
                    serde_json::json!({
                        "type": "object",
                        "properties": {
                            "workspace":  { "type": "string" },
                            "method":     { "type": "string", "enum": ["GET", "POST", "PUT", "DELETE"] },
                            "path":       { "type": "string" },
                            "body":       { "type": "object" },
                            "query":      { "type": "object" },
                            "token_type": { "type": "string", "enum": ["server", "account"] }
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
                        name: "postmark_server_token".to_string(),
                        r#type: "api_key".to_string(),
                        description:
                            "Postmark Server API token (X-Postmark-Server-Token header)"
                                .to_string(),
                    },
                    CredentialRequirement {
                        name: "postmark_account_token".to_string(),
                        r#type: "api_key".to_string(),
                        description:
                            "Postmark Account API token (X-Postmark-Account-Token header) — optional; needed only for servers/domains/sender-signature endpoints"
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
        let cfg = PostmarkSkillConfig {
            server_token: Some("pm_server".to_string()),
            ..PostmarkSkillConfig::default()
        };
        let skill = PostmarkSkill::new(cfg).unwrap();
        let tools = skill.tools();
        assert_eq!(tools.len(), 28);
        for t in &tools {
            assert!(t.name().starts_with("postmark."));
        }
    }

    #[test]
    fn a_sole_named_workspace_becomes_the_default_without_a_selector() {
        let cfg = PostmarkSkillConfig {
            workspaces: HashMap::from([(
                "prod".to_string(),
                PostmarkWorkspaceConfig {
                    server_token: "pm_server".to_string(),
                    account_token: None,
                    base_url: None,
                },
            )]),
            ..PostmarkSkillConfig::default()
        };
        assert_eq!(PostmarkSkill::new(cfg).unwrap().default_workspace, "prod");
    }

    #[test]
    fn skill_builds_with_both_tokens() {
        let cfg = PostmarkSkillConfig {
            server_token: Some("pm_server".to_string()),
            account_token: Some("pm_acct".to_string()),
            ..PostmarkSkillConfig::default()
        };
        assert!(PostmarkSkill::new(cfg).is_ok());
    }

    #[test]
    fn tool_names_matches_registered_tools() {
        let cfg = PostmarkSkillConfig {
            server_token: Some("pm_server".to_string()),
            account_token: Some("pm_acct".to_string()),
            ..PostmarkSkillConfig::default()
        };
        let skill = PostmarkSkill::new(cfg).unwrap();
        let registered: Vec<String> = skill.tools().iter().map(|t| t.name().to_string()).collect();
        let declared: Vec<String> = PostmarkSkill::tool_names()
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(
            registered, declared,
            "PostmarkSkill::tool_names() drifted from tools()"
        );
    }

    #[test]
    fn skill_errors_with_no_workspaces() {
        assert!(PostmarkSkill::new(PostmarkSkillConfig::default()).is_err());
    }

    #[test]
    fn skill_errors_with_account_token_only() {
        // account_token without server_token is not a valid workspace.
        let cfg = PostmarkSkillConfig {
            account_token: Some("pm_acct".to_string()),
            ..PostmarkSkillConfig::default()
        };
        assert!(PostmarkSkill::new(cfg).is_err());
    }

    #[test]
    fn definition_lists_all_28_tools() {
        let def = PostmarkSkill::definition();
        assert_eq!(def.tools.len(), 28);
        assert_eq!(def.default_policies.len(), 28);
        assert_eq!(def.requirements.credentials.len(), 2);
        assert!(def.validate().is_ok());
    }

    #[test]
    fn definition_write_tools_require_approval() {
        let def = PostmarkSkill::definition();
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
            PostmarkSkill::tool_names(),
            READ_TOOLS,
            WRITE_TOOLS,
        );
    }
}
