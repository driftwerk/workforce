//! Postmark agent-callable tools.
//!
//! Each tool accepts an optional `workspace` parameter that selects which
//! Postmark server/account pair to use. Postmark has no stable per-tenant
//! identifier prefix (IDs are UUIDs or opaque integers), so routing is via
//! explicit `workspace` or the configured default — no prefix auto-routing.
//!
//! Two-token model: tools that call `client.server_*` use the server token;
//! tools that call `client.account_*` use the account token (and fail cleanly
//! when it's not configured — see [`super::client`]). The `postmark.api`
//! escape hatch takes a `token_type` parameter and routes accordingly.
//!
//! User-authored content (subjects, body previews, from/to display names,
//! tags, raw MIME dumps) is passed through `wrap_external_content` so
//! prompt-injection attempts in Postmark-fetched data can't escape the
//! security boundary.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use eyre::Result;
use serde_json::{json, Value};
use tracing::{info, warn};

use workforce_agent::tools::{Tool, ToolResult};

use crate::adapters::utils::{
    artifact_attachments_schema, coerce_to_u64, html_with_inline_images, instance_choices,
    resolve_artifact_attachments, truncate_string_fields, wrap_field, wrap_fields_in_place,
};

use super::client::PostmarkClient;

// ──────────────────────────────────────────────────────────────────────
// Constants
// ──────────────────────────────────────────────────────────────────────

/// Maximum `count + offset` for /messages/outbound, /messages/inbound, and
/// /bounces. Beyond this Postmark returns 422 — we enforce upfront so the LLM
/// gets a readable error, not a wire-level validation failure.
const MESSAGE_WINDOW_MAX: u64 = 10_000;

/// Maximum size of a /email/batch payload.
const BATCH_MAX: usize = 500;

/// Default `MessageStream` used by /email when the caller doesn't specify one.
const DEFAULT_OUTBOUND_STREAM: &str = "outbound";

/// Byte cap for `HtmlBody` / `TextBody` on message detail tools — sized to
/// keep real emails intact while preventing inline base64 images from blowing
/// the agent's context window before runtime compaction can react.
const MESSAGE_BODY_MAX_BYTES: usize = 25_000;

// ──────────────────────────────────────────────────────────────────────
// Helpers
// ──────────────────────────────────────────────────────────────────────

/// Resolve which Postmark workspace to use for a given tool input.
///
/// Precedence:
/// 1. Explicit `workspace` parameter (errors if unknown).
/// 2. Fall back to `default_workspace`.
fn resolve_client(
    clients: &HashMap<String, Arc<PostmarkClient>>,
    default_workspace: &str,
    input: &Value,
) -> Result<Arc<PostmarkClient>> {
    let name = workforce_agent::llm::environment::resolve_instance(
        "postmark",
        input.get("workspace").and_then(|v| v.as_str()),
        default_workspace,
        clients,
    )
    .map_err(|d| eyre::eyre!("{}", d.message))?;
    clients.get(&name).cloned().ok_or_else(|| {
        eyre::eyre!(
            "Unknown Postmark workspace: '{name}'. Available: {:?}",
            clients.keys().collect::<Vec<_>>()
        )
    })
}

/// Build a dynamic description for the `workspace` parameter, listing the
/// available workspaces so the LLM knows what's valid.
fn workspace_description(
    clients: &HashMap<String, Arc<PostmarkClient>>,
    default_workspace: &str,
) -> String {
    if clients.len() <= 1 {
        return "Postmark workspace name. Uses the default if omitted.".to_string();
    }
    format!(
        "Postmark workspace name. {}",
        instance_choices(clients, default_workspace, "workspace")
    )
}

/// Required string field extractor with a clean error message.
fn required_str<'a>(input: &'a Value, key: &str) -> std::result::Result<&'a str, String> {
    input
        .get(key)
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("Missing required parameter '{key}'"))
}

/// Required integer field extractor with a clean error message.
fn required_u64(input: &Value, key: &str) -> std::result::Result<u64, String> {
    coerce_to_u64(&input[key])
        .ok_or_else(|| format!("Missing or non-numeric required parameter '{key}'"))
}

/// Enforce `count + offset <= MESSAGE_WINDOW_MAX` and return the tuple.
fn clamp_message_window(
    input: &Value,
    default_count: u64,
) -> std::result::Result<(u64, u64), String> {
    let count = coerce_to_u64(&input["count"]).unwrap_or(default_count);
    let offset = coerce_to_u64(&input["offset"]).unwrap_or(0);
    if count.saturating_add(offset) > MESSAGE_WINDOW_MAX {
        return Err(format!(
            "count ({count}) + offset ({offset}) exceeds Postmark's 10 000 window cap. \
             Narrow the date range (fromdate/todate) and paginate within each window."
        ));
    }
    Ok((count, offset))
}

/// Append a non-empty string parameter from input to a query list under the
/// same key name.
fn push_string_param(query: &mut Vec<(&'static str, String)>, input: &Value, key: &'static str) {
    if let Some(s) = input.get(key).and_then(|v| v.as_str()) {
        if !s.is_empty() {
            query.push((key, s.to_string()));
        }
    }
}

/// Convert a `Vec<(&str, String)>` into a `Vec<(&str, &str)>` for
/// `reqwest::RequestBuilder::query`.
fn query_pairs<'a>(pairs: &'a [(&'static str, String)]) -> Vec<(&'static str, &'a str)> {
    pairs.iter().map(|(k, v)| (*k, v.as_str())).collect()
}

/// Copy a non-null entry from `input[key]` into `body[key]`.
fn copy_field_into(body: &mut serde_json::Map<String, Value>, input: &Value, key: &str) {
    if let Some(v) = input.get(key) {
        if !v.is_null() {
            body.insert(key.to_string(), v.clone());
        }
    }
}

macro_rules! resolve_or_fail {
    ($self:ident, $input:ident, $start:ident) => {{
        match resolve_client(&$self.clients, &$self.default_workspace, &$input) {
            Ok(c) => c,
            Err(e) => {
                return Ok(ToolResult::failure(
                    format!("{e}"),
                    $start.elapsed().as_millis() as u64,
                ))
            }
        }
    }};
}

macro_rules! pm_tool_struct {
    ($name:ident) => {
        pub struct $name {
            clients: Arc<HashMap<String, Arc<PostmarkClient>>>,
            default_workspace: String,
        }

        impl $name {
            pub fn new(
                clients: Arc<HashMap<String, Arc<PostmarkClient>>>,
                default_workspace: String,
            ) -> Self {
                Self {
                    clients,
                    default_workspace,
                }
            }
        }
    };
}

// ──────────────────────────────────────────────────────────────────────
// 1. postmark.messages_outbound_search — GET /messages/outbound
// ──────────────────────────────────────────────────────────────────────

pm_tool_struct!(PostmarkMessagesOutboundSearchTool);

#[async_trait]
impl Tool for PostmarkMessagesOutboundSearchTool {
    fn name(&self) -> &str {
        "postmark.messages_outbound_search"
    }
    fn description(&self) -> &str {
        "Search sent Postmark messages by recipient, sender, tag, subject, status, stream, or date range. Returns paginated summaries with MessageID, Status, Recipients."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "workspace": { "type": "string", "description": workspace_description(&self.clients, &self.default_workspace) },
                "recipient": { "type": "string", "description": "Filter by recipient email (exact match)" },
                "fromemail": { "type": "string", "description": "Filter by sender email (exact match)" },
                "tag":       { "type": "string", "description": "Filter by the Tag field set at send time" },
                "status":    { "type": "string", "enum": ["queued", "sent"], "description": "Postmark status filter" },
                "subject":   { "type": "string", "description": "Substring match on Subject" },
                "messagestream": { "type": "string", "description": "Filter by message stream ID (e.g. 'outbound', 'broadcast')" },
                "metadata":  { "type": "object", "description": "Metadata key/value pairs to match (passed as metadata_<key>=<value> query params)" },
                "fromdate":  { "type": "string", "description": "ISO date/datetime in Eastern Time (NOT UTC), e.g. '2026-04-20' or '2026-04-20T09:00:00'" },
                "todate":    { "type": "string", "description": "ISO date/datetime in Eastern Time (NOT UTC)" },
                "count":     { "type": "integer", "description": "Page size (default 50). count + offset must be <= 10 000." },
                "offset":    { "type": "integer", "description": "Pagination offset (default 0). count + offset must be <= 10 000." }
            }
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();
        let client = resolve_or_fail!(self, input, start);
        let (count, offset) = match clamp_message_window(&input, 50) {
            Ok(v) => v,
            Err(e) => return Ok(ToolResult::failure(e, start.elapsed().as_millis() as u64)),
        };

        let mut query: Vec<(&'static str, String)> =
            vec![("count", count.to_string()), ("offset", offset.to_string())];
        for key in [
            "recipient",
            "fromemail",
            "tag",
            "status",
            "subject",
            "messagestream",
            "fromdate",
            "todate",
        ] {
            push_string_param(&mut query, &input, key);
        }
        // Metadata: Postmark accepts metadata_<key>=<value> query params.
        // Build owned (String, String) pairs, then borrow for reqwest.
        let meta_pairs: Vec<(String, String)> = input
            .get("metadata")
            .and_then(|v| v.as_object())
            .map(|obj| {
                obj.iter()
                    .filter_map(|(k, v)| {
                        v.as_str().map(|s| (format!("metadata_{k}"), s.to_string()))
                    })
                    .collect()
            })
            .unwrap_or_default();
        let mut q_ref: Vec<(&str, &str)> = query_pairs(&query);
        for (k, v) in &meta_pairs {
            q_ref.push((k.as_str(), v.as_str()));
        }

        let duration_ms = start.elapsed().as_millis() as u64;
        match client
            .server_get_with_query("/messages/outbound", &q_ref)
            .await
        {
            Ok(resp) => {
                let total_count = resp.get("TotalCount").cloned().unwrap_or(Value::Null);
                let messages = resp
                    .get("Messages")
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default();
                let summary: Vec<Value> = messages
                    .iter()
                    .map(|m| {
                        json!({
                            "MessageID":     m.get("MessageID"),
                            "Status":        m.get("Status"),
                            "Subject":       wrap_field(m.get("Subject"), "postmark.messages_outbound_search.subject"),
                            "From":          wrap_field(m.get("From"), "postmark.messages_outbound_search.from"),
                            "To":            m.get("To"),
                            "ReceivedAt":    m.get("ReceivedAt"),
                            "MessageStream": m.get("MessageStream"),
                            "Tag":           m.get("Tag"),
                            "TrackOpens":    m.get("TrackOpens"),
                            "TrackLinks":    m.get("TrackLinks"),
                        })
                    })
                    .collect();
                info!(count = summary.len(), "Postmark messages_outbound_search");
                Ok(ToolResult::success(
                    json!({
                        "TotalCount": total_count,
                        "Messages":   summary,
                    }),
                    duration_ms,
                ))
            }
            Err(e) => {
                warn!(error = %e, "Postmark messages_outbound_search failed");
                Ok(ToolResult::from_report(
                    &e.wrap_err("Postmark messages_outbound_search failed"),
                    duration_ms,
                ))
            }
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// 2. postmark.message_outbound_get — GET /messages/outbound/{id}/details
// ──────────────────────────────────────────────────────────────────────

pm_tool_struct!(PostmarkMessageOutboundGetTool);

#[async_trait]
impl Tool for PostmarkMessageOutboundGetTool {
    fn name(&self) -> &str {
        "postmark.message_outbound_get"
    }
    fn description(&self) -> &str {
        "Get full details of a sent Postmark message including headers, body, recipient delivery events, bounce state."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "workspace":  { "type": "string", "description": workspace_description(&self.clients, &self.default_workspace) },
                "message_id": { "type": "string", "description": "Outbound Postmark MessageID (UUID)" }
            },
            "required": ["message_id"]
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();
        let client = resolve_or_fail!(self, input, start);
        let id = match required_str(&input, "message_id") {
            Ok(s) => s.to_string(),
            Err(e) => return Ok(ToolResult::failure(e, start.elapsed().as_millis() as u64)),
        };
        let path = format!("/messages/outbound/{id}/details");
        let duration_ms = start.elapsed().as_millis() as u64;
        match client.server_get(&path).await {
            Ok(mut resp) => {
                // `Body` is the raw SMTP dump (base64 attachments inline,
                // easily MBs); `message_outbound_dump` is the explicit-raw path.
                if let Some(obj) = resp.as_object_mut() {
                    obj.remove("Body");
                }
                truncate_string_fields(
                    &mut resp,
                    &["HtmlBody", "TextBody"],
                    MESSAGE_BODY_MAX_BYTES,
                );
                wrap_fields_in_place(
                    &mut resp,
                    &["Subject", "From", "TextBody", "HtmlBody", "Tag"],
                    "postmark.message_outbound_get",
                );
                Ok(ToolResult::success(resp, duration_ms))
            }
            Err(e) => Ok(ToolResult::from_report(
                &e.wrap_err(format!("Failed to get Postmark message {id}")),
                duration_ms,
            )),
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// 3. postmark.message_outbound_dump — GET /messages/outbound/{id}/dump
// ──────────────────────────────────────────────────────────────────────

pm_tool_struct!(PostmarkMessageOutboundDumpTool);

#[async_trait]
impl Tool for PostmarkMessageOutboundDumpTool {
    fn name(&self) -> &str {
        "postmark.message_outbound_dump"
    }
    fn description(&self) -> &str {
        "Get the raw MIME body of a sent message (30-day retention). Returns { Body: string }."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "workspace":  { "type": "string", "description": workspace_description(&self.clients, &self.default_workspace) },
                "message_id": { "type": "string", "description": "Outbound Postmark MessageID (UUID)" }
            },
            "required": ["message_id"]
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();
        let client = resolve_or_fail!(self, input, start);
        let id = match required_str(&input, "message_id") {
            Ok(s) => s.to_string(),
            Err(e) => return Ok(ToolResult::failure(e, start.elapsed().as_millis() as u64)),
        };
        let path = format!("/messages/outbound/{id}/dump");
        let duration_ms = start.elapsed().as_millis() as u64;
        match client.server_get(&path).await {
            Ok(mut resp) => {
                wrap_fields_in_place(&mut resp, &["Body"], "postmark.message_outbound_dump");
                Ok(ToolResult::success(resp, duration_ms))
            }
            Err(e) => Ok(ToolResult::from_report(
                &e.wrap_err(format!("Failed to dump Postmark message {id}")),
                duration_ms,
            )),
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// 4-5. Outbound event tools — opens + clicks share schema + dispatch.
// ──────────────────────────────────────────────────────────────────────

const OUTBOUND_EVENT_FILTERS: &[&str] = &[
    "recipient",
    "tag",
    "client_name",
    "os_family",
    "platform",
    "country",
    "messagestream",
    "fromdate",
    "todate",
];

fn outbound_events_schema(workspace_desc: String) -> Value {
    json!({
        "type": "object",
        "properties": {
            "workspace":     { "type": "string", "description": workspace_desc },
            "message_id":    { "type": "string", "description": "Scope to a single MessageID (optional)" },
            "recipient":     { "type": "string" },
            "tag":           { "type": "string" },
            "client_name":   { "type": "string", "description": "Email client, e.g. 'Gmail'" },
            "os_family":     { "type": "string" },
            "platform":      { "type": "string", "enum": ["desktop", "mobile", "webmail", "unknown"] },
            "country":       { "type": "string" },
            "messagestream": { "type": "string" },
            "fromdate":      { "type": "string", "description": "Eastern Time" },
            "todate":        { "type": "string", "description": "Eastern Time" },
            "count":         { "type": "integer", "description": "Default 50, cap 500" },
            "offset":        { "type": "integer" }
        }
    })
}

async fn run_outbound_events_endpoint(
    client: &PostmarkClient,
    input: &Value,
    event: &str,
    label: &str,
    start: std::time::Instant,
) -> ToolResult {
    let count = coerce_to_u64(&input["count"]).unwrap_or(50).min(500);
    let offset = coerce_to_u64(&input["offset"]).unwrap_or(0);
    let path = match input.get("message_id").and_then(|v| v.as_str()) {
        Some(id) if !id.is_empty() => format!("/messages/outbound/{event}/{id}"),
        _ => format!("/messages/outbound/{event}"),
    };
    let mut query: Vec<(&'static str, String)> =
        vec![("count", count.to_string()), ("offset", offset.to_string())];
    for &key in OUTBOUND_EVENT_FILTERS {
        push_string_param(&mut query, input, key);
    }
    let q_ref = query_pairs(&query);
    let duration_ms = start.elapsed().as_millis() as u64;
    match client.server_get_with_query(&path, &q_ref).await {
        Ok(resp) => ToolResult::success(resp, duration_ms),
        Err(e) => {
            ToolResult::from_report(&e.wrap_err(format!("Postmark {label} failed")), duration_ms)
        }
    }
}

pm_tool_struct!(PostmarkMessagesOutboundOpensTool);

#[async_trait]
impl Tool for PostmarkMessagesOutboundOpensTool {
    fn name(&self) -> &str {
        "postmark.messages_outbound_opens"
    }
    fn description(&self) -> &str {
        "List open events. Global across all messages, or scoped to one message_id."
    }
    fn input_schema(&self) -> Value {
        outbound_events_schema(workspace_description(
            &self.clients,
            &self.default_workspace,
        ))
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();
        let client = resolve_or_fail!(self, input, start);
        Ok(
            run_outbound_events_endpoint(
                &client,
                &input,
                "opens",
                "messages_outbound_opens",
                start,
            )
            .await,
        )
    }
}

pm_tool_struct!(PostmarkMessagesOutboundClicksTool);

#[async_trait]
impl Tool for PostmarkMessagesOutboundClicksTool {
    fn name(&self) -> &str {
        "postmark.messages_outbound_clicks"
    }
    fn description(&self) -> &str {
        "List click-tracking events. Global across all messages, or scoped to one message_id."
    }
    fn input_schema(&self) -> Value {
        outbound_events_schema(workspace_description(
            &self.clients,
            &self.default_workspace,
        ))
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();
        let client = resolve_or_fail!(self, input, start);
        Ok(run_outbound_events_endpoint(
            &client,
            &input,
            "clicks",
            "messages_outbound_clicks",
            start,
        )
        .await)
    }
}

// ──────────────────────────────────────────────────────────────────────
// 6. postmark.messages_inbound_search — GET /messages/inbound
// ──────────────────────────────────────────────────────────────────────

pm_tool_struct!(PostmarkMessagesInboundSearchTool);

#[async_trait]
impl Tool for PostmarkMessagesInboundSearchTool {
    fn name(&self) -> &str {
        "postmark.messages_inbound_search"
    }
    fn description(&self) -> &str {
        "Search inbound Postmark messages by recipient, sender, tag, subject, mailbox hash, or status."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "workspace":   { "type": "string", "description": workspace_description(&self.clients, &self.default_workspace) },
                "recipient":   { "type": "string" },
                "fromemail":   { "type": "string" },
                "tag":         { "type": "string" },
                "subject":     { "type": "string" },
                "mailboxhash": { "type": "string" },
                "status":      { "type": "string", "enum": ["blocked", "processed", "queued", "failed", "scheduled"] },
                "fromdate":    { "type": "string", "description": "Eastern Time" },
                "todate":      { "type": "string", "description": "Eastern Time" },
                "count":       { "type": "integer", "description": "Default 50. count + offset must be <= 10 000." },
                "offset":      { "type": "integer" }
            }
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();
        let client = resolve_or_fail!(self, input, start);
        let (count, offset) = match clamp_message_window(&input, 50) {
            Ok(v) => v,
            Err(e) => return Ok(ToolResult::failure(e, start.elapsed().as_millis() as u64)),
        };
        let mut query: Vec<(&'static str, String)> =
            vec![("count", count.to_string()), ("offset", offset.to_string())];
        for key in [
            "recipient",
            "fromemail",
            "tag",
            "subject",
            "mailboxhash",
            "status",
            "fromdate",
            "todate",
        ] {
            push_string_param(&mut query, &input, key);
        }
        let q_ref = query_pairs(&query);
        let duration_ms = start.elapsed().as_millis() as u64;
        match client
            .server_get_with_query("/messages/inbound", &q_ref)
            .await
        {
            Ok(resp) => {
                let total_count = resp.get("TotalCount").cloned().unwrap_or(Value::Null);
                let messages = resp
                    .get("InboundMessages")
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default();
                let summary: Vec<Value> = messages
                    .iter()
                    .map(|m| {
                        json!({
                            "MessageID":   m.get("MessageID"),
                            "Status":      m.get("Status"),
                            "From":        wrap_field(m.get("From"), "postmark.messages_inbound_search.from"),
                            "FromName":    wrap_field(m.get("FromName"), "postmark.messages_inbound_search.from_name"),
                            "To":          m.get("To"),
                            "Subject":     wrap_field(m.get("Subject"), "postmark.messages_inbound_search.subject"),
                            "Date":        m.get("Date"),
                            "MailboxHash": m.get("MailboxHash"),
                            "Tag":         m.get("Tag"),
                        })
                    })
                    .collect();
                Ok(ToolResult::success(
                    json!({
                        "TotalCount":      total_count,
                        "InboundMessages": summary,
                    }),
                    duration_ms,
                ))
            }
            Err(e) => Ok(ToolResult::from_report(
                &e.wrap_err("Postmark messages_inbound_search failed"),
                duration_ms,
            )),
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// 7. postmark.message_inbound_get — GET /messages/inbound/{id}/details
// ──────────────────────────────────────────────────────────────────────

pm_tool_struct!(PostmarkMessageInboundGetTool);

#[async_trait]
impl Tool for PostmarkMessageInboundGetTool {
    fn name(&self) -> &str {
        "postmark.message_inbound_get"
    }
    fn description(&self) -> &str {
        "Get full details of a single inbound Postmark message."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "workspace":  { "type": "string", "description": workspace_description(&self.clients, &self.default_workspace) },
                "message_id": { "type": "string", "description": "Inbound MessageID" }
            },
            "required": ["message_id"]
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();
        let client = resolve_or_fail!(self, input, start);
        let id = match required_str(&input, "message_id") {
            Ok(s) => s.to_string(),
            Err(e) => return Ok(ToolResult::failure(e, start.elapsed().as_millis() as u64)),
        };
        let path = format!("/messages/inbound/{id}/details");
        let duration_ms = start.elapsed().as_millis() as u64;
        match client.server_get(&path).await {
            Ok(mut resp) => {
                truncate_string_fields(
                    &mut resp,
                    &["HtmlBody", "TextBody"],
                    MESSAGE_BODY_MAX_BYTES,
                );
                wrap_fields_in_place(
                    &mut resp,
                    &["Subject", "From", "FromName", "TextBody", "HtmlBody", "Tag"],
                    "postmark.message_inbound_get",
                );
                Ok(ToolResult::success(resp, duration_ms))
            }
            Err(e) => Ok(ToolResult::from_report(
                &e.wrap_err(format!("Failed to get inbound message {id}")),
                duration_ms,
            )),
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// 8. postmark.email_send — POST /email
// ──────────────────────────────────────────────────────────────────────

pm_tool_struct!(PostmarkEmailSendTool);

fn non_empty_str(input: &Value, key: &str) -> bool {
    input
        .get(key)
        .and_then(|v| v.as_str())
        .is_some_and(|s| !s.is_empty())
}

fn validate_email_body(input: &Value) -> std::result::Result<(), String> {
    if !non_empty_str(input, "HtmlBody") && !non_empty_str(input, "TextBody") {
        return Err(
            "Postmark /email requires at least one of 'HtmlBody' or 'TextBody' (non-empty)"
                .to_string(),
        );
    }
    Ok(())
}

fn validate_batch_size(messages: &[Value]) -> std::result::Result<(), String> {
    if messages.is_empty() {
        return Err("Batch must contain at least one message".to_string());
    }
    if messages.len() > BATCH_MAX {
        return Err(format!(
            "Postmark /email/batch caps at {BATCH_MAX} messages per request (got {}).",
            messages.len()
        ));
    }
    Ok(())
}

fn validate_template_selector(input: &Value) -> std::result::Result<(), String> {
    let has_id = input.get("TemplateId").is_some_and(|v| !v.is_null());
    let has_alias = non_empty_str(input, "TemplateAlias");
    if has_id == has_alias {
        return Err(
            "Postmark email_send_template requires exactly one of 'TemplateId' or 'TemplateAlias'"
                .to_string(),
        );
    }
    if input.get("TemplateModel").is_none_or(Value::is_null) {
        return Err("Missing required parameter 'TemplateModel' (object)".to_string());
    }
    Ok(())
}

fn build_email_body(input: &Value, include_bodies: bool) -> Value {
    let scalar_keys: &[&str] = &[
        "From",
        "To",
        "Cc",
        "Bcc",
        "Subject",
        "Tag",
        "ReplyTo",
        "TrackOpens",
        "TrackLinks",
    ];
    let array_keys: &[&str] = &["Headers", "Attachments"];

    let mut body = serde_json::Map::new();
    for &key in scalar_keys {
        copy_field_into(&mut body, input, key);
    }
    if include_bodies {
        copy_field_into(&mut body, input, "HtmlBody");
        copy_field_into(&mut body, input, "TextBody");
    }
    copy_field_into(&mut body, input, "Metadata");
    for &key in array_keys {
        if let Some(arr) = input.get(key).and_then(|v| v.as_array()) {
            body.insert(key.to_string(), Value::Array(arr.clone()));
        }
    }
    let stream = input
        .get("MessageStream")
        .and_then(|v| v.as_str())
        .unwrap_or(DEFAULT_OUTBOUND_STREAM)
        .to_string();
    body.insert("MessageStream".to_string(), Value::String(stream));

    Value::Object(body)
}

/// Postmark caps the total size of a message (body + all attachments) at 10 MB.
const MAX_ATTACHMENT_BYTES: usize = 10 * 1024 * 1024;

/// Append any `attach_artifact_ids` to `input["Attachments"]` as Postmark
/// attachment objects (inline ones carry a `cid:` `ContentID`), and make sure
/// `HtmlBody` shows every inline image. Mutates `input` in place.
fn merge_artifact_attachments(input: &mut Value) -> Result<(), String> {
    let extra: Vec<Value> =
        resolve_artifact_attachments(input, MAX_ATTACHMENT_BYTES, "Postmark's 10 MB limit")?
            .into_iter()
            .map(|a| {
                let mut entry = json!({
                    "Name": a.name, "Content": a.content_base64(), "ContentType": a.content_type
                });
                if let Some(cid) = &a.content_id {
                    entry["ContentID"] = json!(format!("cid:{cid}"));
                }
                entry
            })
            .collect();
    let has_explicit = input["Attachments"]
        .as_array()
        .is_some_and(|a| !a.is_empty());
    if extra.is_empty() && !has_explicit {
        return Ok(());
    }
    let attachments = input
        .as_object_mut()
        .and_then(|o| {
            o.entry("Attachments")
                .or_insert_with(|| Value::Array(Vec::new()))
                .as_array_mut()
        })
        .ok_or_else(|| "Attachments must be an array".to_string())?;
    attachments.extend(extra);
    let inline: Vec<(String, String)> = attachments
        .iter_mut()
        .filter_map(|a| {
            let cid = a["ContentID"]
                .as_str()?
                .trim_start_matches("cid:")
                .to_string();
            a["ContentID"] = json!(format!("cid:{cid}"));
            Some((cid, a["Name"].as_str().unwrap_or_default().to_string()))
        })
        .collect();
    if let Some(html) = input["HtmlBody"].as_str() {
        input["HtmlBody"] = Value::String(html_with_inline_images(
            html,
            inline
                .iter()
                .map(|(cid, name)| (cid.as_str(), name.as_str())),
        ));
    }
    Ok(())
}

fn email_send_schema_properties(workspace_desc: String) -> Value {
    json!({
        "workspace":     { "type": "string", "description": workspace_desc },
        "From":          { "type": "string", "description": "Sender address. Must be a verified Sender Signature in Postmark." },
        "To":            { "type": "string", "description": "Recipient(s). Comma-separated string (NOT an array)." },
        "Cc":            { "type": "string" },
        "Bcc":           { "type": "string" },
        "Subject":       { "type": "string" },
        "Tag":           { "type": "string" },
        "ReplyTo":       { "type": "string" },
        "HtmlBody":      { "type": "string", "description": "At least one of HtmlBody or TextBody is required." },
        "TextBody":      { "type": "string", "description": "At least one of HtmlBody or TextBody is required." },
        "TrackOpens":    { "type": "boolean" },
        "TrackLinks":    { "type": "string", "enum": ["None", "HtmlOnly", "TextOnly", "HtmlAndText"] },
        "Metadata":      { "type": "object", "description": "Arbitrary {string: string} map. String values only." },
        "Headers":       {
            "type": "array",
            "description": "Custom headers, e.g. [{Name: 'X-Campaign', Value: 'foo'}].",
            "items": { "type": "object", "properties": {
                "Name":  { "type": "string" },
                "Value": { "type": "string" }
            }, "required": ["Name", "Value"] }
        },
        "Attachments":   {
            "type": "array",
            "description": "Array of base64-encoded attachments.",
            "items": { "type": "object", "properties": {
                "Name":        { "type": "string" },
                "Content":     { "type": "string", "description": "Base64-encoded file content." },
                "ContentType": { "type": "string" },
                "ContentID":   { "type": "string", "description": "cid:<id> for inline images (optional)." }
            }, "required": ["Name", "Content", "ContentType"] }
        },
        "MessageStream": { "type": "string", "description": "Defaults to 'outbound'." }
    })
}

#[async_trait]
impl Tool for PostmarkEmailSendTool {
    fn name(&self) -> &str {
        "postmark.email_send"
    }
    fn description(&self) -> &str {
        "Send a single transactional email via Postmark. Requires at least one of HtmlBody or TextBody."
    }
    fn input_schema(&self) -> Value {
        let mut props = email_send_schema_properties(workspace_description(
            &self.clients,
            &self.default_workspace,
        ));
        props
            .as_object_mut()
            .expect("email_send_schema_properties returns an object")
            .insert(
                "attach_artifact_ids".to_string(),
                artifact_attachments_schema("Attachments"),
            );
        json!({
            "type": "object",
            "properties": props,
            "required": ["From", "To", "Subject"]
        })
    }
    async fn execute(&self, mut input: Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();
        let client = resolve_or_fail!(self, input, start);
        if let Err(e) = required_str(&input, "From") {
            return Ok(ToolResult::failure(e, start.elapsed().as_millis() as u64));
        }
        if let Err(e) = required_str(&input, "To") {
            return Ok(ToolResult::failure(e, start.elapsed().as_millis() as u64));
        }
        if let Err(e) = required_str(&input, "Subject") {
            return Ok(ToolResult::failure(e, start.elapsed().as_millis() as u64));
        }
        if let Err(e) = validate_email_body(&input) {
            return Ok(ToolResult::failure(e, start.elapsed().as_millis() as u64));
        }
        if let Err(e) = merge_artifact_attachments(&mut input) {
            return Ok(ToolResult::failure(e, start.elapsed().as_millis() as u64));
        }

        let body = build_email_body(&input, true);
        let duration_ms = start.elapsed().as_millis() as u64;
        match client.server_post("/email", &body).await {
            Ok(resp) => {
                info!(message_id = ?resp.get("MessageID"), "Postmark email sent");
                Ok(ToolResult::success(resp, duration_ms))
            }
            Err(e) => Ok(ToolResult::from_report(
                &e.wrap_err("Postmark email_send failed"),
                duration_ms,
            )),
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// 9. postmark.email_send_batch — POST /email/batch
// ──────────────────────────────────────────────────────────────────────

pm_tool_struct!(PostmarkEmailSendBatchTool);

#[async_trait]
impl Tool for PostmarkEmailSendBatchTool {
    fn name(&self) -> &str {
        "postmark.email_send_batch"
    }
    fn description(&self) -> &str {
        "Send up to 500 transactional emails in a single batch via Postmark. Each item follows the email_send schema."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "workspace": { "type": "string", "description": workspace_description(&self.clients, &self.default_workspace) },
                "messages":  {
                    "type": "array",
                    "description": "1..500 email objects (same shape as postmark.email_send).",
                    "items": { "type": "object", "properties": email_send_schema_properties("".to_string()) }
                }
            },
            "required": ["messages"]
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();
        let client = resolve_or_fail!(self, input, start);
        let Some(messages) = input.get("messages").and_then(|v| v.as_array()) else {
            return Ok(ToolResult::failure(
                "Missing required parameter 'messages' (array)".to_string(),
                start.elapsed().as_millis() as u64,
            ));
        };
        if let Err(e) = validate_batch_size(messages) {
            return Ok(ToolResult::failure(e, start.elapsed().as_millis() as u64));
        }

        let bodies: std::result::Result<Vec<Value>, String> = messages
            .iter()
            .enumerate()
            .map(|(idx, m)| {
                for req in ["From", "To", "Subject"] {
                    required_str(m, req).map_err(|e| format!("messages[{idx}]: {e}"))?;
                }
                validate_email_body(m).map_err(|e| format!("messages[{idx}]: {e}"))?;
                Ok(build_email_body(m, true))
            })
            .collect();
        let bodies = match bodies {
            Ok(b) => b,
            Err(e) => return Ok(ToolResult::failure(e, start.elapsed().as_millis() as u64)),
        };
        let body = Value::Array(bodies);
        let duration_ms = start.elapsed().as_millis() as u64;
        match client.server_post("/email/batch", &body).await {
            Ok(resp) => Ok(ToolResult::success(resp, duration_ms)),
            Err(e) => Ok(ToolResult::from_report(
                &e.wrap_err("Postmark email_send_batch failed"),
                duration_ms,
            )),
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// 10. postmark.email_send_template — POST /email/withTemplate
// ──────────────────────────────────────────────────────────────────────

pm_tool_struct!(PostmarkEmailSendTemplateTool);

#[async_trait]
impl Tool for PostmarkEmailSendTemplateTool {
    fn name(&self) -> &str {
        "postmark.email_send_template"
    }
    fn description(&self) -> &str {
        "Send a transactional email rendered from a Postmark template. Requires one of TemplateId or TemplateAlias plus a TemplateModel object."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "workspace":      { "type": "string", "description": workspace_description(&self.clients, &self.default_workspace) },
                "From":           { "type": "string" },
                "To":             { "type": "string", "description": "Recipient(s), comma-separated." },
                "Cc":             { "type": "string" },
                "Bcc":            { "type": "string" },
                "Tag":             { "type": "string" },
                "ReplyTo":         { "type": "string" },
                "TrackOpens":      { "type": "boolean" },
                "TrackLinks":      { "type": "string", "enum": ["None", "HtmlOnly", "TextOnly", "HtmlAndText"] },
                "TemplateId":     { "type": "integer", "description": "Numeric ID of a Postmark template. Pass this OR TemplateAlias." },
                "TemplateAlias":  { "type": "string", "description": "String alias of a Postmark template. Pass this OR TemplateId." },
                "TemplateModel":  { "type": "object", "description": "Object of variables to interpolate into the template." },
                "InlineCss":      { "type": "boolean" },
                "Metadata":       { "type": "object" },
                "Headers":        { "type": "array" },
                "Attachments":    { "type": "array" },
                "attach_artifact_ids": artifact_attachments_schema("Attachments"),
                "MessageStream":  { "type": "string" }
            },
            "required": ["From", "To", "TemplateModel"]
        })
    }
    async fn execute(&self, mut input: Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();
        let client = resolve_or_fail!(self, input, start);
        for req in ["From", "To"] {
            if let Err(e) = required_str(&input, req) {
                return Ok(ToolResult::failure(e, start.elapsed().as_millis() as u64));
            }
        }
        if let Err(e) = validate_template_selector(&input) {
            return Ok(ToolResult::failure(e, start.elapsed().as_millis() as u64));
        }
        if let Err(e) = merge_artifact_attachments(&mut input) {
            return Ok(ToolResult::failure(e, start.elapsed().as_millis() as u64));
        }

        let mut body = build_email_body(&input, false);
        let body_obj = body
            .as_object_mut()
            .expect("build_email_body returns object");
        // `validate_template_selector` guarantees exactly one of the two is
        // set; `copy_field_into` is a no-op for the absent one.
        copy_field_into(body_obj, &input, "TemplateId");
        copy_field_into(body_obj, &input, "TemplateAlias");
        copy_field_into(body_obj, &input, "TemplateModel");
        copy_field_into(body_obj, &input, "InlineCss");
        // Subject comes from the template; drop if accidentally set.
        body_obj.remove("Subject");

        let duration_ms = start.elapsed().as_millis() as u64;
        match client.server_post("/email/withTemplate", &body).await {
            Ok(resp) => Ok(ToolResult::success(resp, duration_ms)),
            Err(e) => Ok(ToolResult::from_report(
                &e.wrap_err("Postmark email_send_template failed"),
                duration_ms,
            )),
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// 11. postmark.bounces_list — GET /bounces
// ──────────────────────────────────────────────────────────────────────

pm_tool_struct!(PostmarkBouncesListTool);

#[async_trait]
impl Tool for PostmarkBouncesListTool {
    fn name(&self) -> &str {
        "postmark.bounces_list"
    }
    fn description(&self) -> &str {
        "List bounces with filters (type, inactive, emailFilter, tag, messageID, date range, messagestream)."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "workspace":     { "type": "string", "description": workspace_description(&self.clients, &self.default_workspace) },
                "type":          { "type": "string", "description": "HardBounce | SoftBounce | Transient | SpamComplaint | SpamNotification | AutoResponder | DnsError | SMTPApiError | BadEmailAddress | ManuallyDeactivated | Unconfirmed | Blocked | SubscribeError | AddressChange" },
                "inactive":      { "type": "boolean", "description": "Filter by inactive flag" },
                "emailFilter":   { "type": "string", "description": "Substring match on recipient email" },
                "tag":           { "type": "string" },
                "messageID":     { "type": "string", "description": "Outbound MessageID (UUID)" },
                "messagestream": { "type": "string" },
                "fromdate":      { "type": "string", "description": "Eastern Time" },
                "todate":        { "type": "string", "description": "Eastern Time" },
                "count":         { "type": "integer", "description": "Default 25. count + offset must be <= 10 000." },
                "offset":        { "type": "integer" }
            }
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();
        let client = resolve_or_fail!(self, input, start);
        let (count, offset) = match clamp_message_window(&input, 25) {
            Ok(v) => v,
            Err(e) => return Ok(ToolResult::failure(e, start.elapsed().as_millis() as u64)),
        };
        let mut query: Vec<(&'static str, String)> =
            vec![("count", count.to_string()), ("offset", offset.to_string())];
        for key in [
            "type",
            "emailFilter",
            "tag",
            "messageID",
            "messagestream",
            "fromdate",
            "todate",
        ] {
            push_string_param(&mut query, &input, key);
        }
        if let Some(b) = input.get("inactive").and_then(|v| v.as_bool()) {
            query.push(("inactive", b.to_string()));
        }
        let q_ref = query_pairs(&query);
        let duration_ms = start.elapsed().as_millis() as u64;
        match client.server_get_with_query("/bounces", &q_ref).await {
            Ok(mut resp) => {
                if let Some(arr) = resp.get_mut("Bounces").and_then(|v| v.as_array_mut()) {
                    for b in arr.iter_mut() {
                        wrap_fields_in_place(
                            b,
                            &["From", "Email", "Subject", "Description", "Details", "Tag"],
                            "postmark.bounces_list",
                        );
                    }
                }
                Ok(ToolResult::success(resp, duration_ms))
            }
            Err(e) => Ok(ToolResult::from_report(
                &e.wrap_err("Postmark bounces_list failed"),
                duration_ms,
            )),
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// 12. postmark.bounce_get — GET /bounces/{id}
// ──────────────────────────────────────────────────────────────────────

pm_tool_struct!(PostmarkBounceGetTool);

#[async_trait]
impl Tool for PostmarkBounceGetTool {
    fn name(&self) -> &str {
        "postmark.bounce_get"
    }
    fn description(&self) -> &str {
        "Get details of a single bounce by its integer bounce ID."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "workspace": { "type": "string", "description": workspace_description(&self.clients, &self.default_workspace) },
                "bounce_id": { "type": "integer", "description": "Integer bounce ID (from bounces_list)" }
            },
            "required": ["bounce_id"]
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();
        let client = resolve_or_fail!(self, input, start);
        let id = match required_u64(&input, "bounce_id") {
            Ok(n) => n,
            Err(e) => return Ok(ToolResult::failure(e, start.elapsed().as_millis() as u64)),
        };
        let path = format!("/bounces/{id}");
        let duration_ms = start.elapsed().as_millis() as u64;
        match client.server_get(&path).await {
            Ok(mut resp) => {
                wrap_fields_in_place(
                    &mut resp,
                    &["From", "Email", "Subject", "Description", "Details", "Tag"],
                    "postmark.bounce_get",
                );
                Ok(ToolResult::success(resp, duration_ms))
            }
            Err(e) => Ok(ToolResult::from_report(
                &e.wrap_err(format!("Failed to get bounce {id}")),
                duration_ms,
            )),
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// 13. postmark.bounce_activate — PUT /bounces/{id}/activate
// ──────────────────────────────────────────────────────────────────────

pm_tool_struct!(PostmarkBounceActivateTool);

#[async_trait]
impl Tool for PostmarkBounceActivateTool {
    fn name(&self) -> &str {
        "postmark.bounce_activate"
    }
    fn description(&self) -> &str {
        "Re-activate a suppressed recipient (clear the inactive bounce). Future sends to this address will be attempted again."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "workspace": { "type": "string", "description": workspace_description(&self.clients, &self.default_workspace) },
                "bounce_id": { "type": "integer" }
            },
            "required": ["bounce_id"]
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();
        let client = resolve_or_fail!(self, input, start);
        let id = match required_u64(&input, "bounce_id") {
            Ok(n) => n,
            Err(e) => return Ok(ToolResult::failure(e, start.elapsed().as_millis() as u64)),
        };
        let path = format!("/bounces/{id}/activate");
        let duration_ms = start.elapsed().as_millis() as u64;
        match client.server_put(&path, &json!({})).await {
            Ok(resp) => Ok(ToolResult::success(resp, duration_ms)),
            Err(e) => Ok(ToolResult::from_report(
                &e.wrap_err(format!("Failed to activate bounce {id}")),
                duration_ms,
            )),
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// 14. postmark.delivery_stats — GET /deliverystats
// ──────────────────────────────────────────────────────────────────────

pm_tool_struct!(PostmarkDeliveryStatsTool);

#[async_trait]
impl Tool for PostmarkDeliveryStatsTool {
    fn name(&self) -> &str {
        "postmark.delivery_stats"
    }
    fn description(&self) -> &str {
        "Get a rollup of delivery counts by bounce type and inactive mail count for the server."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "workspace": { "type": "string", "description": workspace_description(&self.clients, &self.default_workspace) }
            }
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();
        let client = resolve_or_fail!(self, input, start);
        let duration_ms = start.elapsed().as_millis() as u64;
        match client.server_get("/deliverystats").await {
            Ok(resp) => Ok(ToolResult::success(resp, duration_ms)),
            Err(e) => Ok(ToolResult::from_report(
                &e.wrap_err("Postmark delivery_stats failed"),
                duration_ms,
            )),
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// Helper for the four /stats/outbound endpoints
// ──────────────────────────────────────────────────────────────────────

async fn run_stats_endpoint(
    client: &Arc<PostmarkClient>,
    input: &Value,
    path: &str,
    label: &str,
    start: std::time::Instant,
) -> ToolResult {
    let mut query: Vec<(&'static str, String)> = Vec::new();
    for key in ["tag", "fromdate", "todate", "messagestream"] {
        push_string_param(&mut query, input, key);
    }
    let q_ref = query_pairs(&query);
    let duration_ms = start.elapsed().as_millis() as u64;
    match client.server_get_with_query(path, &q_ref).await {
        Ok(resp) => ToolResult::success(resp, duration_ms),
        Err(e) => {
            ToolResult::from_report(&e.wrap_err(format!("Postmark {label} failed")), duration_ms)
        }
    }
}

fn stats_schema(workspace_desc: String) -> Value {
    json!({
        "type": "object",
        "properties": {
            "workspace":     { "type": "string", "description": workspace_desc },
            "tag":           { "type": "string" },
            "fromdate":      { "type": "string", "description": "Eastern Time" },
            "todate":        { "type": "string", "description": "Eastern Time" },
            "messagestream": { "type": "string" }
        }
    })
}

// ──────────────────────────────────────────────────────────────────────
// 15. postmark.stats_outbound — GET /stats/outbound
// ──────────────────────────────────────────────────────────────────────

pm_tool_struct!(PostmarkStatsOutboundTool);

#[async_trait]
impl Tool for PostmarkStatsOutboundTool {
    fn name(&self) -> &str {
        "postmark.stats_outbound"
    }
    fn description(&self) -> &str {
        "Summary of outbound sending stats (sent, bounced, SMTP API errors, opens, tracked, etc.) for a time range."
    }
    fn input_schema(&self) -> Value {
        stats_schema(workspace_description(
            &self.clients,
            &self.default_workspace,
        ))
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();
        let client = resolve_or_fail!(self, input, start);
        Ok(run_stats_endpoint(&client, &input, "/stats/outbound", "stats_outbound", start).await)
    }
}

// ──────────────────────────────────────────────────────────────────────
// 16. postmark.stats_bounces — GET /stats/outbound/bounces
// ──────────────────────────────────────────────────────────────────────

pm_tool_struct!(PostmarkStatsBouncesTool);

#[async_trait]
impl Tool for PostmarkStatsBouncesTool {
    fn name(&self) -> &str {
        "postmark.stats_bounces"
    }
    fn description(&self) -> &str {
        "Bounce counts broken down by type across a time range."
    }
    fn input_schema(&self) -> Value {
        stats_schema(workspace_description(
            &self.clients,
            &self.default_workspace,
        ))
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();
        let client = resolve_or_fail!(self, input, start);
        Ok(run_stats_endpoint(
            &client,
            &input,
            "/stats/outbound/bounces",
            "stats_bounces",
            start,
        )
        .await)
    }
}

// ──────────────────────────────────────────────────────────────────────
// 17. postmark.stats_opens — GET /stats/outbound/opens
// ──────────────────────────────────────────────────────────────────────

pm_tool_struct!(PostmarkStatsOpensTool);

#[async_trait]
impl Tool for PostmarkStatsOpensTool {
    fn name(&self) -> &str {
        "postmark.stats_opens"
    }
    fn description(&self) -> &str {
        "Open-tracking totals and unique opens across a time range."
    }
    fn input_schema(&self) -> Value {
        stats_schema(workspace_description(
            &self.clients,
            &self.default_workspace,
        ))
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();
        let client = resolve_or_fail!(self, input, start);
        Ok(run_stats_endpoint(
            &client,
            &input,
            "/stats/outbound/opens",
            "stats_opens",
            start,
        )
        .await)
    }
}

// ──────────────────────────────────────────────────────────────────────
// 18. postmark.stats_clicks — GET /stats/outbound/clicks
// ──────────────────────────────────────────────────────────────────────

pm_tool_struct!(PostmarkStatsClicksTool);

#[async_trait]
impl Tool for PostmarkStatsClicksTool {
    fn name(&self) -> &str {
        "postmark.stats_clicks"
    }
    fn description(&self) -> &str {
        "Click-tracking totals and unique clicks across a time range."
    }
    fn input_schema(&self) -> Value {
        stats_schema(workspace_description(
            &self.clients,
            &self.default_workspace,
        ))
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();
        let client = resolve_or_fail!(self, input, start);
        Ok(run_stats_endpoint(
            &client,
            &input,
            "/stats/outbound/clicks",
            "stats_clicks",
            start,
        )
        .await)
    }
}

// ──────────────────────────────────────────────────────────────────────
// 19. postmark.templates_list — GET /templates
// ──────────────────────────────────────────────────────────────────────

pm_tool_struct!(PostmarkTemplatesListTool);

#[async_trait]
impl Tool for PostmarkTemplatesListTool {
    fn name(&self) -> &str {
        "postmark.templates_list"
    }
    fn description(&self) -> &str {
        "List Postmark templates on the current server."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "workspace":       { "type": "string", "description": workspace_description(&self.clients, &self.default_workspace) },
                "count":           { "type": "integer", "description": "Default 100" },
                "offset":          { "type": "integer" },
                "templateType":    { "type": "string", "enum": ["Standard", "Layout"] },
                "layoutTemplate":  { "type": "string", "description": "Filter by layout template alias" }
            }
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();
        let client = resolve_or_fail!(self, input, start);
        let count = coerce_to_u64(&input["count"]).unwrap_or(100);
        let offset = coerce_to_u64(&input["offset"]).unwrap_or(0);
        let mut query: Vec<(&'static str, String)> =
            vec![("count", count.to_string()), ("offset", offset.to_string())];
        for key in ["templateType", "layoutTemplate"] {
            push_string_param(&mut query, &input, key);
        }
        let q_ref = query_pairs(&query);
        let duration_ms = start.elapsed().as_millis() as u64;
        match client.server_get_with_query("/templates", &q_ref).await {
            Ok(resp) => Ok(ToolResult::success(resp, duration_ms)),
            Err(e) => Ok(ToolResult::from_report(
                &e.wrap_err("Postmark templates_list failed"),
                duration_ms,
            )),
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// 20. postmark.template_get — GET /templates/{idOrAlias}
// ──────────────────────────────────────────────────────────────────────

pm_tool_struct!(PostmarkTemplateGetTool);

#[async_trait]
impl Tool for PostmarkTemplateGetTool {
    fn name(&self) -> &str {
        "postmark.template_get"
    }
    fn description(&self) -> &str {
        "Get a single Postmark template by numeric ID or alias string."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "workspace":    { "type": "string", "description": workspace_description(&self.clients, &self.default_workspace) },
                "id_or_alias":  { "type": "string", "description": "Numeric template ID or template alias" }
            },
            "required": ["id_or_alias"]
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();
        let client = resolve_or_fail!(self, input, start);
        let id = match required_str(&input, "id_or_alias") {
            Ok(s) => s.to_string(),
            Err(e) => return Ok(ToolResult::failure(e, start.elapsed().as_millis() as u64)),
        };
        let path = format!("/templates/{id}");
        let duration_ms = start.elapsed().as_millis() as u64;
        match client.server_get(&path).await {
            Ok(mut resp) => {
                wrap_fields_in_place(
                    &mut resp,
                    &["Subject", "HtmlBody", "TextBody"],
                    "postmark.template_get",
                );
                Ok(ToolResult::success(resp, duration_ms))
            }
            Err(e) => Ok(ToolResult::from_report(
                &e.wrap_err(format!("Failed to get template {id}")),
                duration_ms,
            )),
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// 21. postmark.message_streams_list — GET /message-streams
// ──────────────────────────────────────────────────────────────────────

pm_tool_struct!(PostmarkMessageStreamsListTool);

#[async_trait]
impl Tool for PostmarkMessageStreamsListTool {
    fn name(&self) -> &str {
        "postmark.message_streams_list"
    }
    fn description(&self) -> &str {
        "List message streams on the current server (transactional, broadcast, inbound)."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "workspace":          { "type": "string", "description": workspace_description(&self.clients, &self.default_workspace) },
                "MessageStreamType":  { "type": "string", "enum": ["All", "Transactional", "Inbound", "Broadcasts"] },
                "IncludeArchivedStreams": { "type": "boolean" }
            }
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();
        let client = resolve_or_fail!(self, input, start);
        let mut query: Vec<(&'static str, String)> = Vec::new();
        push_string_param(&mut query, &input, "MessageStreamType");
        if let Some(b) = input
            .get("IncludeArchivedStreams")
            .and_then(|v| v.as_bool())
        {
            query.push(("IncludeArchivedStreams", b.to_string()));
        }
        let q_ref = query_pairs(&query);
        let duration_ms = start.elapsed().as_millis() as u64;
        match client
            .server_get_with_query("/message-streams", &q_ref)
            .await
        {
            Ok(resp) => Ok(ToolResult::success(resp, duration_ms)),
            Err(e) => Ok(ToolResult::from_report(
                &e.wrap_err("Postmark message_streams_list failed"),
                duration_ms,
            )),
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// 22. postmark.suppressions_dump — GET /message-streams/{id}/suppressions/dump
// ──────────────────────────────────────────────────────────────────────

pm_tool_struct!(PostmarkSuppressionsDumpTool);

#[async_trait]
impl Tool for PostmarkSuppressionsDumpTool {
    fn name(&self) -> &str {
        "postmark.suppressions_dump"
    }
    fn description(&self) -> &str {
        "List all suppressions on a given message stream (paginated)."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "workspace":      { "type": "string", "description": workspace_description(&self.clients, &self.default_workspace) },
                "stream_id":      { "type": "string", "description": "Message stream ID, e.g. 'outbound'" },
                "SuppressionReason": { "type": "string", "description": "Filter by reason (ManualSuppression, HardBounce, SpamComplaint, etc.)" },
                "Origin":         { "type": "string", "description": "Recipient | Customer | Admin" },
                "fromdate":       { "type": "string" },
                "todate":         { "type": "string" },
                "emailaddress":   { "type": "string" }
            },
            "required": ["stream_id"]
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();
        let client = resolve_or_fail!(self, input, start);
        let stream = match required_str(&input, "stream_id") {
            Ok(s) => s.to_string(),
            Err(e) => return Ok(ToolResult::failure(e, start.elapsed().as_millis() as u64)),
        };
        let path = format!("/message-streams/{stream}/suppressions/dump");
        let mut query: Vec<(&'static str, String)> = Vec::new();
        for key in [
            "SuppressionReason",
            "Origin",
            "fromdate",
            "todate",
            "emailaddress",
        ] {
            push_string_param(&mut query, &input, key);
        }
        let q_ref = query_pairs(&query);
        let duration_ms = start.elapsed().as_millis() as u64;
        match client.server_get_with_query(&path, &q_ref).await {
            Ok(resp) => Ok(ToolResult::success(resp, duration_ms)),
            Err(e) => Ok(ToolResult::from_report(
                &e.wrap_err("Postmark suppressions_dump failed"),
                duration_ms,
            )),
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// 23. postmark.suppressions_add — POST /message-streams/{id}/suppressions
// ──────────────────────────────────────────────────────────────────────

pm_tool_struct!(PostmarkSuppressionsAddTool);

#[async_trait]
impl Tool for PostmarkSuppressionsAddTool {
    fn name(&self) -> &str {
        "postmark.suppressions_add"
    }
    fn description(&self) -> &str {
        "Add email addresses to the suppression list for a message stream."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "workspace":    { "type": "string", "description": workspace_description(&self.clients, &self.default_workspace) },
                "stream_id":    { "type": "string" },
                "Suppressions": {
                    "type": "array",
                    "items": { "type": "object", "properties": {
                        "EmailAddress":      { "type": "string" },
                        "SuppressionReason": { "type": "string", "description": "Typically 'ManualSuppression'" }
                    }, "required": ["EmailAddress"] }
                }
            },
            "required": ["stream_id", "Suppressions"]
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();
        let client = resolve_or_fail!(self, input, start);
        let stream = match required_str(&input, "stream_id") {
            Ok(s) => s.to_string(),
            Err(e) => return Ok(ToolResult::failure(e, start.elapsed().as_millis() as u64)),
        };
        let suppressions = input
            .get("Suppressions")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        if suppressions.is_empty() {
            return Ok(ToolResult::failure(
                "Missing or empty 'Suppressions' array".to_string(),
                start.elapsed().as_millis() as u64,
            ));
        }
        let body = json!({ "Suppressions": suppressions });
        let path = format!("/message-streams/{stream}/suppressions");
        let duration_ms = start.elapsed().as_millis() as u64;
        match client.server_post(&path, &body).await {
            Ok(resp) => Ok(ToolResult::success(resp, duration_ms)),
            Err(e) => Ok(ToolResult::from_report(
                &e.wrap_err("Postmark suppressions_add failed"),
                duration_ms,
            )),
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// 24. postmark.suppressions_delete — POST /message-streams/{id}/suppressions/delete
// ──────────────────────────────────────────────────────────────────────

pm_tool_struct!(PostmarkSuppressionsDeleteTool);

#[async_trait]
impl Tool for PostmarkSuppressionsDeleteTool {
    fn name(&self) -> &str {
        "postmark.suppressions_delete"
    }
    fn description(&self) -> &str {
        "Remove email addresses from a message stream's suppression list. (Note: HTTP method is POST, not DELETE.)"
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "workspace":    { "type": "string", "description": workspace_description(&self.clients, &self.default_workspace) },
                "stream_id":    { "type": "string" },
                "Suppressions": {
                    "type": "array",
                    "items": { "type": "object", "properties": {
                        "EmailAddress": { "type": "string" }
                    }, "required": ["EmailAddress"] }
                }
            },
            "required": ["stream_id", "Suppressions"]
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();
        let client = resolve_or_fail!(self, input, start);
        let stream = match required_str(&input, "stream_id") {
            Ok(s) => s.to_string(),
            Err(e) => return Ok(ToolResult::failure(e, start.elapsed().as_millis() as u64)),
        };
        let suppressions = input
            .get("Suppressions")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        if suppressions.is_empty() {
            return Ok(ToolResult::failure(
                "Missing or empty 'Suppressions' array".to_string(),
                start.elapsed().as_millis() as u64,
            ));
        }
        let body = json!({ "Suppressions": suppressions });
        // Gotcha: this is a POST to /suppressions/delete, not a DELETE.
        let path = format!("/message-streams/{stream}/suppressions/delete");
        let duration_ms = start.elapsed().as_millis() as u64;
        match client.server_post(&path, &body).await {
            Ok(resp) => Ok(ToolResult::success(resp, duration_ms)),
            Err(e) => Ok(ToolResult::from_report(
                &e.wrap_err("Postmark suppressions_delete failed"),
                duration_ms,
            )),
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// 25. postmark.servers_list — GET /servers (ACCOUNT TOKEN)
// ──────────────────────────────────────────────────────────────────────

pm_tool_struct!(PostmarkServersListTool);

#[async_trait]
impl Tool for PostmarkServersListTool {
    fn name(&self) -> &str {
        "postmark.servers_list"
    }
    fn description(&self) -> &str {
        "List Postmark servers on the account. Requires account_token to be configured for this workspace."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "workspace": { "type": "string", "description": workspace_description(&self.clients, &self.default_workspace) },
                "count":     { "type": "integer", "description": "Default 50" },
                "offset":    { "type": "integer" },
                "name":      { "type": "string", "description": "Filter by server name substring" }
            }
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();
        let client = resolve_or_fail!(self, input, start);
        let count = coerce_to_u64(&input["count"]).unwrap_or(50);
        let offset = coerce_to_u64(&input["offset"]).unwrap_or(0);
        let mut query: Vec<(&'static str, String)> =
            vec![("count", count.to_string()), ("offset", offset.to_string())];
        push_string_param(&mut query, &input, "name");
        let q_ref = query_pairs(&query);
        let duration_ms = start.elapsed().as_millis() as u64;
        match client.account_get_with_query("/servers", &q_ref).await {
            Ok(resp) => Ok(ToolResult::success(resp, duration_ms)),
            Err(e) => Ok(ToolResult::from_report(
                &e.wrap_err("Postmark servers_list failed"),
                duration_ms,
            )),
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// 26. postmark.domains_list — GET /domains (ACCOUNT TOKEN)
// ──────────────────────────────────────────────────────────────────────

pm_tool_struct!(PostmarkDomainsListTool);

#[async_trait]
impl Tool for PostmarkDomainsListTool {
    fn name(&self) -> &str {
        "postmark.domains_list"
    }
    fn description(&self) -> &str {
        "List sender domains configured on the account. Requires account_token."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "workspace": { "type": "string", "description": workspace_description(&self.clients, &self.default_workspace) },
                "count":     { "type": "integer", "description": "Default 50" },
                "offset":    { "type": "integer" }
            }
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();
        let client = resolve_or_fail!(self, input, start);
        let count = coerce_to_u64(&input["count"]).unwrap_or(50);
        let offset = coerce_to_u64(&input["offset"]).unwrap_or(0);
        let q_ref_owned: Vec<(&'static str, String)> =
            vec![("count", count.to_string()), ("offset", offset.to_string())];
        let q_ref = query_pairs(&q_ref_owned);
        let duration_ms = start.elapsed().as_millis() as u64;
        match client.account_get_with_query("/domains", &q_ref).await {
            Ok(resp) => Ok(ToolResult::success(resp, duration_ms)),
            Err(e) => Ok(ToolResult::from_report(
                &e.wrap_err("Postmark domains_list failed"),
                duration_ms,
            )),
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// 27. postmark.domain_get — GET /domains/{id} (ACCOUNT TOKEN)
// ──────────────────────────────────────────────────────────────────────

pm_tool_struct!(PostmarkDomainGetTool);

#[async_trait]
impl Tool for PostmarkDomainGetTool {
    fn name(&self) -> &str {
        "postmark.domain_get"
    }
    fn description(&self) -> &str {
        "Get details (DKIM, SPF, ReturnPath verification flags) for a sender domain. Requires account_token."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "workspace": { "type": "string", "description": workspace_description(&self.clients, &self.default_workspace) },
                "domain_id": { "type": "integer", "description": "Numeric domain ID (from domains_list)" }
            },
            "required": ["domain_id"]
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();
        let client = resolve_or_fail!(self, input, start);
        let id = match required_u64(&input, "domain_id") {
            Ok(n) => n,
            Err(e) => return Ok(ToolResult::failure(e, start.elapsed().as_millis() as u64)),
        };
        let path = format!("/domains/{id}");
        let duration_ms = start.elapsed().as_millis() as u64;
        match client.account_get(&path).await {
            Ok(resp) => Ok(ToolResult::success(resp, duration_ms)),
            Err(e) => Ok(ToolResult::from_report(
                &e.wrap_err("Postmark domain_get failed"),
                duration_ms,
            )),
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// 28. postmark.api — generic escape hatch (require_approval)
// ──────────────────────────────────────────────────────────────────────

pm_tool_struct!(PostmarkApiTool);

#[async_trait]
impl Tool for PostmarkApiTool {
    fn name(&self) -> &str {
        "postmark.api"
    }
    fn description(&self) -> &str {
        "Make an arbitrary Postmark REST API request. Use for endpoints not covered by the typed tools. `token_type` selects server or account token (default: server)."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "workspace":  { "type": "string", "description": workspace_description(&self.clients, &self.default_workspace) },
                "method":     { "type": "string", "enum": ["GET", "POST", "PUT", "DELETE"] },
                "path":       { "type": "string", "description": "Path starting with '/', e.g. '/servers' or '/email'" },
                "body":       { "type": "object", "description": "JSON body for POST/PUT (optional)" },
                "query":      { "type": "object", "description": "Query parameters as {key: value}" },
                "token_type": { "type": "string", "enum": ["server", "account"], "description": "Which token to attach. Default: server." }
            },
            "required": ["method", "path"]
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();
        let client = resolve_or_fail!(self, input, start);
        let method = match required_str(&input, "method") {
            Ok(s) => s.to_ascii_uppercase(),
            Err(e) => return Ok(ToolResult::failure(e, start.elapsed().as_millis() as u64)),
        };
        let path = match required_str(&input, "path") {
            Ok(s) => s.to_string(),
            Err(e) => return Ok(ToolResult::failure(e, start.elapsed().as_millis() as u64)),
        };
        let token_type = input
            .get("token_type")
            .and_then(|v| v.as_str())
            .unwrap_or("server")
            .to_ascii_lowercase();
        if token_type != "server" && token_type != "account" {
            return Ok(ToolResult::failure(
                format!("Invalid token_type '{token_type}' (expected 'server' or 'account')"),
                start.elapsed().as_millis() as u64,
            ));
        }

        let query_owned: Vec<(String, String)> = input
            .get("query")
            .and_then(|v| v.as_object())
            .map(|obj| {
                obj.iter()
                    .filter_map(|(k, v)| match v {
                        Value::String(s) => Some((k.clone(), s.clone())),
                        Value::Number(n) => Some((k.clone(), n.to_string())),
                        Value::Bool(b) => Some((k.clone(), b.to_string())),
                        _ => None,
                    })
                    .collect()
            })
            .unwrap_or_default();
        let query_ref: Vec<(&str, &str)> = query_owned
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        let body = input.get("body").cloned().unwrap_or(Value::Null);

        let duration_ms = start.elapsed().as_millis() as u64;
        let result = match (method.as_str(), token_type.as_str()) {
            ("GET", "server") => {
                if query_ref.is_empty() {
                    client.server_get(&path).await
                } else {
                    client.server_get_with_query(&path, &query_ref).await
                }
            }
            ("GET", "account") => {
                if query_ref.is_empty() {
                    client.account_get(&path).await
                } else {
                    client.account_get_with_query(&path, &query_ref).await
                }
            }
            ("POST", "server") => client.server_post(&path, &body).await,
            ("POST", "account") => client.account_post(&path, &body).await,
            ("PUT", "server") => client.server_put(&path, &body).await,
            ("PUT", "account") => client.account_put(&path, &body).await,
            ("DELETE", "server") => client.server_delete(&path).await,
            ("DELETE", "account") => client.account_delete(&path).await,
            _ => {
                return Ok(ToolResult::failure(
                    format!("Unsupported HTTP method '{method}' (expected GET/POST/PUT/DELETE)"),
                    duration_ms,
                ));
            }
        };
        match result {
            Ok(resp) => Ok(ToolResult::success(resp, duration_ms)),
            Err(e) => Ok(ToolResult::from_report(
                &e.wrap_err(format!("Postmark api {method} {path}")),
                duration_ms,
            )),
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_artifact_attachments_noop_preserves_existing() {
        let mut input = json!({
            "HtmlBody": "<p>hi</p>",
            "Attachments": [{ "Name": "a.txt", "Content": "Zm9v", "ContentType": "text/plain" }]
        });
        merge_artifact_attachments(&mut input).unwrap();
        assert_eq!(input["Attachments"].as_array().unwrap().len(), 1);
        assert_eq!(input["HtmlBody"], "<p>hi</p>");
    }

    #[test]
    fn merge_artifact_attachments_shows_an_inline_image_the_html_omits_and_prefixes_cid() {
        let mut input = json!({
            "HtmlBody": "<p>hi</p>",
            "Attachments": [{ "Name": "q3.png", "Content": "Zm9v", "ContentType": "image/png", "ContentID": "q3" }]
        });
        merge_artifact_attachments(&mut input).unwrap();
        assert_eq!(input["Attachments"][0]["ContentID"], "cid:q3");
        assert_eq!(
            input["HtmlBody"],
            "<p>hi</p>\n<p><img src=\"cid:q3\" alt=\"q3.png\"></p>"
        );
    }

    #[test]
    fn merge_artifact_attachments_carries_a_staged_inline_part_with_a_cid_prefix() {
        use crate::adapters::utils::test_support::{staged_png, AGENT};
        use workforce_agent::runner::{CURRENT_AGENT_ID, CURRENT_ARTIFACT_STORE};

        let (store, id, _dir) = staged_png("signups.png");
        let mut input: Value = json!({
            "HtmlBody": "<p>hi</p>",
            "attach_artifact_ids": [{ "artifact_id": id.as_str(), "inline": true }]
        });
        CURRENT_ARTIFACT_STORE.sync_scope(store, || {
            CURRENT_AGENT_ID.sync_scope(AGENT.to_string(), || {
                merge_artifact_attachments(&mut input).unwrap()
            })
        });
        let cid = id.as_str().trim_start_matches("art_");
        let attachment = &input["Attachments"][0];
        assert_eq!(attachment["Name"], "signups.png");
        assert_eq!(attachment["ContentType"], "image/png");
        assert_eq!(attachment["ContentID"], format!("cid:{cid}"));
        assert!(input["HtmlBody"]
            .as_str()
            .unwrap()
            .contains(&format!("cid:{cid}")));
    }

    fn make_clients() -> Arc<HashMap<String, Arc<PostmarkClient>>> {
        let mut m = HashMap::new();
        m.insert(
            "default".to_string(),
            Arc::new(PostmarkClient::new("pm_server", None, None).unwrap()),
        );
        m.insert(
            "work".to_string(),
            Arc::new(PostmarkClient::new("pm_server_work", Some("pm_acct"), None).unwrap()),
        );
        Arc::new(m)
    }

    #[test]
    fn resolve_client_uses_explicit_workspace() {
        let clients = make_clients();
        let input = json!({ "workspace": "work" });
        let c = resolve_client(&clients, "default", &input).unwrap();
        assert!(c.has_account_token());
    }

    #[test]
    fn resolve_client_falls_back_to_default() {
        let clients = make_clients();
        let input = json!({});
        let c = resolve_client(&clients, "default", &input).unwrap();
        assert!(!c.has_account_token());
    }

    #[test]
    fn resolve_client_unknown_errors() {
        let clients = make_clients();
        let input = json!({ "workspace": "bogus" });
        let result = resolve_client(&clients, "default", &input);
        let err = result.err().expect("should be an error");
        assert!(err.to_string().contains("Unknown Postmark workspace"));
    }

    #[test]
    fn workspace_description_lists_multiple() {
        let clients = make_clients();
        let desc = workspace_description(&clients, "default");
        assert!(desc.contains("default"));
        assert!(desc.contains("work"));
        assert!(workspace_description(&clients, "gone").contains("Default: none — pass workspace"));
    }

    #[test]
    fn workspace_description_single_line_for_one() {
        let mut m = HashMap::new();
        m.insert(
            "default".to_string(),
            Arc::new(PostmarkClient::new("pm_s", None, None).unwrap()),
        );
        let clients = Arc::new(m);
        let desc = workspace_description(&clients, "default");
        assert!(desc.contains("Uses the default if omitted"));
    }

    #[test]
    fn clamp_message_window_rejects_over_cap() {
        let input = json!({ "count": 9000, "offset": 2000 });
        assert!(clamp_message_window(&input, 50).is_err());
    }

    #[test]
    fn clamp_message_window_accepts_at_cap() {
        let input = json!({ "count": 8000, "offset": 2000 });
        assert!(clamp_message_window(&input, 50).is_ok());
    }

    #[test]
    fn clamp_message_window_uses_default_count() {
        let input = json!({});
        let (c, o) = clamp_message_window(&input, 50).unwrap();
        assert_eq!(c, 50);
        assert_eq!(o, 0);
    }

    #[test]
    fn validate_email_body_requires_one_body() {
        assert!(validate_email_body(&json!({})).is_err());
        assert!(validate_email_body(&json!({ "HtmlBody": "" })).is_err());
        assert!(validate_email_body(&json!({ "HtmlBody": "<p>hi</p>" })).is_ok());
        assert!(validate_email_body(&json!({ "TextBody": "hi" })).is_ok());
        assert!(validate_email_body(&json!({ "HtmlBody": "<p>hi</p>", "TextBody": "hi" })).is_ok());
    }

    #[test]
    fn build_email_body_defaults_stream_to_outbound() {
        let body = build_email_body(
            &json!({
                "From": "a@b.com",
                "To": "c@d.com",
                "Subject": "hi",
                "HtmlBody": "<p>hi</p>"
            }),
            true,
        );
        assert_eq!(body["MessageStream"], json!("outbound"));
        assert_eq!(body["HtmlBody"], json!("<p>hi</p>"));
    }

    #[test]
    fn build_email_body_preserves_explicit_stream() {
        let body = build_email_body(
            &json!({
                "From": "a@b.com",
                "To": "c@d.com",
                "Subject": "hi",
                "TextBody": "hi",
                "MessageStream": "broadcasts"
            }),
            true,
        );
        assert_eq!(body["MessageStream"], json!("broadcasts"));
    }

    #[test]
    fn build_email_body_excludes_bodies_for_template() {
        let body = build_email_body(
            &json!({
                "From": "a@b.com",
                "To": "c@d.com",
                "HtmlBody": "ignored",
                "TextBody": "ignored"
            }),
            false,
        );
        assert!(body.get("HtmlBody").is_none());
        assert!(body.get("TextBody").is_none());
    }

    #[test]
    fn required_u64_accepts_string_integer() {
        let input = json!({ "bounce_id": "42" });
        assert_eq!(required_u64(&input, "bounce_id").unwrap(), 42);
    }

    #[test]
    fn required_u64_rejects_missing() {
        let input = json!({});
        assert!(required_u64(&input, "bounce_id").is_err());
    }

    #[test]
    fn validate_batch_size_rejects_empty() {
        assert!(validate_batch_size(&[]).is_err());
    }

    #[test]
    fn validate_batch_size_rejects_over_cap() {
        let oversized: Vec<Value> = (0..=BATCH_MAX).map(|_| json!({})).collect();
        let err = validate_batch_size(&oversized).unwrap_err();
        assert!(err.contains("caps at 500"), "{err}");
    }

    #[test]
    fn validate_batch_size_accepts_at_cap() {
        let at_cap: Vec<Value> = (0..BATCH_MAX).map(|_| json!({})).collect();
        assert!(validate_batch_size(&at_cap).is_ok());
    }

    #[test]
    fn validate_template_selector_rejects_neither() {
        let err = validate_template_selector(&json!({ "TemplateModel": {} })).unwrap_err();
        assert!(err.contains("exactly one of"), "{err}");
    }

    #[test]
    fn validate_template_selector_rejects_both() {
        let err = validate_template_selector(&json!({
            "TemplateId": 123,
            "TemplateAlias": "welcome",
            "TemplateModel": {}
        }))
        .unwrap_err();
        assert!(err.contains("exactly one of"), "{err}");
    }

    #[test]
    fn validate_template_selector_rejects_missing_model() {
        let err = validate_template_selector(&json!({ "TemplateId": 123 })).unwrap_err();
        assert!(err.contains("TemplateModel"), "{err}");
    }

    #[test]
    fn validate_template_selector_accepts_id_only() {
        assert!(validate_template_selector(&json!({
            "TemplateId": 42,
            "TemplateModel": { "name": "Ada" }
        }))
        .is_ok());
    }

    #[test]
    fn validate_template_selector_accepts_alias_only() {
        assert!(validate_template_selector(&json!({
            "TemplateAlias": "welcome",
            "TemplateModel": {}
        }))
        .is_ok());
    }

    #[test]
    fn validate_template_selector_rejects_empty_alias_as_absent() {
        // Empty string should count as absent; with no TemplateId, the rule
        // "exactly one of" fails.
        let err = validate_template_selector(&json!({
            "TemplateAlias": "",
            "TemplateModel": {}
        }))
        .unwrap_err();
        assert!(err.contains("exactly one of"), "{err}");
    }

    #[test]
    fn outbound_get_response_shaping_drops_body_and_truncates_html() {
        let mut resp = json!({
            "Body":     "raw MIME with megabytes of base64",
            "HtmlBody": "x".repeat(MESSAGE_BODY_MAX_BYTES + 10),
            "TextBody": "short",
            "Subject":  "hello",
        });
        if let Some(obj) = resp.as_object_mut() {
            obj.remove("Body");
        }
        truncate_string_fields(&mut resp, &["HtmlBody", "TextBody"], MESSAGE_BODY_MAX_BYTES);
        assert!(resp.get("Body").is_none());
        let html = resp["HtmlBody"].as_str().unwrap();
        assert!(html.contains("truncated"));
        assert!(html.len() < MESSAGE_BODY_MAX_BYTES + 200);
        assert_eq!(resp["TextBody"], json!("short"));
    }
}
