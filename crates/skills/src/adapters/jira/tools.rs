//! Jira Cloud agent-callable tools.
//!
//! Each tool accepts an optional `workspace` parameter that overrides the
//! default workspace selection. When the input contains a `project_key` or
//! an `issue_key` like `ENG-123`, the prefix is matched against the
//! configured `prefix_map` to auto-route the request to the right tenant.
//!
//! User-authored content (summaries, descriptions, comment bodies, rendered
//! HTML) is passed through `wrap_external_content` so prompt-injection
//! attempts in Jira data can't escape the security boundary.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use eyre::Result;
use serde_json::{json, Value};
use tracing::{info, warn};

use workforce_agent::security::external_content::wrap_external_content;
use workforce_agent::tools::image_view::{
    stage_for_view, view_pending_note, within_view_cap, MAX_VIEW_BYTES, VIEWED_IMAGES_KEY,
};
use workforce_agent::tools::{Tool, ToolResult};

use crate::adapters::utils::{coerce_to_i64, coerce_to_u64, instance_choices};

use super::client::{JiraClient, AGILE, REST};
use super::markdown::markdown_to_adf;

// ──────────────────────────────────────────────────────────────────────
// Helpers
// ──────────────────────────────────────────────────────────────────────

/// Wrap a JSON string field with security boundary markers if it contains content.
fn wrap_field(value: &Value, source: &str) -> Value {
    match value.as_str() {
        Some(s) if !s.is_empty() => Value::String(wrap_external_content(s, source).wrapped_content),
        _ => value.clone(),
    }
}

/// Map an issue's `fields.attachment` array into a compact, agent-facing list of
/// `{ id, filename, mime_type, size }`. The agent passes an `id` to
/// `jira.get_attachment` to read the file. `filename` is attacker-controllable,
/// so it is wrapped; absent or non-array input yields an empty list.
fn attachment_summaries(fields: &Value) -> Vec<Value> {
    fields["attachment"]
        .as_array()
        .map(|atts| {
            atts.iter()
                .map(|a| {
                    json!({
                        "id": a["id"],
                        "filename": wrap_field(&a["filename"], "jira.get_issue.attachment_filename"),
                        "mime_type": a["mimeType"],
                        "size": a["size"],
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Resolve which Jira workspace to use for a given tool input.
///
/// Resolution order:
/// 1. Explicit `workspace` parameter (errors if unknown).
/// 2. `project_key` (uppercase) → `prefix_map` lookup.
/// 3. `issue_key` "ENG-123" → take "ENG" → `prefix_map` lookup.
/// 4. Fall back to `default_workspace`.
fn resolve_client(
    clients: &HashMap<String, Arc<JiraClient>>,
    default_workspace: &str,
    prefix_map: &HashMap<String, String>,
    input: &Value,
) -> Result<Arc<JiraClient>> {
    let explicit = input.get("workspace").and_then(|v| v.as_str());
    let requested = explicit
        .map(str::to_string)
        .or_else(|| routed_workspace(clients, prefix_map, input));
    let name = workforce_agent::llm::environment::resolve_instance(
        "jira",
        requested.as_deref(),
        default_workspace,
        clients,
    )
    .map_err(|d| eyre::eyre!("{}", d.message))?;
    clients.get(&name).cloned().ok_or_else(|| {
        let available = clients.keys().collect::<Vec<_>>();
        if explicit == Some(name.as_str()) {
            eyre::eyre!("Unknown Jira workspace: '{name}'. Available: {available:?}")
        } else {
            eyre::eyre!("Default Jira workspace '{name}' not configured. Available: {available:?}")
        }
    })
}

/// Workspace auto-routed from a `project_key` / `issue_key` prefix (uppercased),
/// but only when the mapped workspace is actually configured — otherwise `None`,
/// so the caller falls back to the default (or the environment pin).
fn routed_workspace(
    clients: &HashMap<String, Arc<JiraClient>>,
    prefix_map: &HashMap<String, String>,
    input: &Value,
) -> Option<String> {
    let from_prefix = |prefix: &str| {
        prefix_map
            .get(&prefix.to_uppercase())
            .filter(|name| clients.contains_key(*name))
            .cloned()
    };
    if let Some(pk) = input.get("project_key").and_then(|v| v.as_str()) {
        if let Some(name) = from_prefix(pk) {
            return Some(name);
        }
    }
    input
        .get("issue_key")
        .and_then(|v| v.as_str())
        .and_then(|ik| ik.split_once('-'))
        .and_then(|(prefix, _)| from_prefix(prefix))
}

/// Build a dynamic description for the `workspace` parameter, listing the
/// available workspaces so the LLM knows what's valid.
fn workspace_description(
    clients: &HashMap<String, Arc<JiraClient>>,
    default_workspace: &str,
) -> String {
    if clients.len() <= 1 {
        return "Jira workspace name. Uses the default if omitted.".to_string();
    }
    format!(
        "Jira workspace name. {} Issue/project keys also auto-route via configured project prefixes.",
        instance_choices(clients, default_workspace, "workspace")
    )
}

/// Coerce a caller-supplied value into an Atlassian Document Format (ADF)
/// document for REST v3 `description` and comment `body` fields.
///
/// - Objects pass through unchanged so callers can hand over raw ADF.
/// - `null` and empty / whitespace-only strings produce an empty ADF doc.
/// - Everything else is treated as markdown and rendered into ADF nodes.
fn to_adf(value: &Value) -> Value {
    if value.is_object() {
        return value.clone();
    }
    markdown_to_adf(value.as_str().unwrap_or(""))
}

/// Whether a flattened Jira field error is a priority-name rejection — the
/// case where listing the instance's actual priorities lets the caller
/// self-correct in one round.
fn is_priority_rejection(error: &str) -> bool {
    let lower = error.to_ascii_lowercase();
    lower.contains("priority") && (lower.contains("invalid") || lower.contains("not valid"))
}

/// The teaching suffix: every priority name quoted VERBATIM — priority
/// names can carry significant whitespace (a live instance has
/// "Not Prioritised "), and a trimmed name is one Jira then rejects.
fn priority_hint(names: &[String]) -> String {
    let quoted: Vec<String> = names.iter().map(|n| format!("\"{n}\"")).collect();
    format!("Valid priorities on this Jira: {}", quoted.join(", "))
}

/// Append the instance's valid priorities when Jira rejected the one
/// supplied. Fetches only when the error classifies, and fails open to the
/// original message on any lookup problem — a worse error is never worth a
/// different failure.
async fn enrich_priority_error(error: eyre::Report, client: &JiraClient) -> eyre::Report {
    let text = error.to_string();
    if !is_priority_rejection(&text) {
        return error;
    }
    let Ok(list) = client.get(&format!("{REST}/priority")).await else {
        return error;
    };
    let names: Vec<String> = list
        .as_array()
        .map(|priorities| {
            priorities
                .iter()
                .filter_map(|p| p["name"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    if names.is_empty() {
        return error;
    }
    eyre::eyre!("{text}\n{}", priority_hint(&names))
}

/// Resolve the project key from the explicit input or the client's default.
fn resolve_project_key<'a>(
    input: &'a Value,
    client: &'a JiraClient,
) -> std::result::Result<String, String> {
    if let Some(s) = input.get("project_key").and_then(|v| v.as_str()) {
        return Ok(s.to_string());
    }
    client
        .default_project()
        .map(|s| s.to_string())
        .ok_or_else(|| {
            "Missing 'project_key' parameter and no default_project configured".to_string()
        })
}

/// Required string field extractor with a clean error message.
fn required_str<'a>(input: &'a Value, key: &str) -> std::result::Result<&'a str, String> {
    input
        .get(key)
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("Missing required parameter '{key}'"))
}

macro_rules! resolve_or_fail {
    ($self:ident, $input:ident, $start:ident) => {{
        match resolve_client(
            &$self.clients,
            &$self.default_workspace,
            &$self.prefix_map,
            &$input,
        ) {
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

macro_rules! jira_tool_struct {
    ($name:ident) => {
        pub struct $name {
            clients: Arc<HashMap<String, Arc<JiraClient>>>,
            default_workspace: String,
            prefix_map: Arc<HashMap<String, String>>,
        }

        impl $name {
            pub fn new(
                clients: Arc<HashMap<String, Arc<JiraClient>>>,
                default_workspace: String,
                prefix_map: Arc<HashMap<String, String>>,
            ) -> Self {
                Self {
                    clients,
                    default_workspace,
                    prefix_map,
                }
            }
        }
    };
}

// ──────────────────────────────────────────────────────────────────────
// 1. jira.search — JQL search via the new /search/jql endpoint
// ──────────────────────────────────────────────────────────────────────

jira_tool_struct!(JiraSearchTool);

#[async_trait]
impl Tool for JiraSearchTool {
    fn name(&self) -> &str {
        "jira.search"
    }
    fn description(&self) -> &str {
        "Search Jira issues using JQL. Returns a paginated list with key, summary, status, assignee."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "workspace": { "type": "string", "description": workspace_description(&self.clients, &self.default_workspace) },
                "jql": { "type": "string", "description": "JQL query, e.g. 'project = ENG AND status = \"In Progress\"'" },
                "fields": { "type": "array", "items": { "type": "string" }, "description": "Field IDs to return (default: summary,status,assignee,priority,updated)" },
                "next_page_token": { "type": "string", "description": "Pagination cursor from a previous response" },
                "max_results": { "type": "integer", "description": "Page size (default 50, max 100)" }
            },
            "required": ["jql"]
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();
        let client = resolve_or_fail!(self, input, start);
        let jql = match required_str(&input, "jql") {
            Ok(s) => s.to_string(),
            Err(e) => return Ok(ToolResult::failure(e, start.elapsed().as_millis() as u64)),
        };
        let max_results = coerce_to_u64(&input["max_results"]).unwrap_or(50).min(100);

        let default_fields = vec![
            "summary".to_string(),
            "status".to_string(),
            "assignee".to_string(),
            "priority".to_string(),
            "updated".to_string(),
            "issuetype".to_string(),
        ];
        let fields: Vec<String> = input
            .get("fields")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or(default_fields);

        let mut body = json!({
            "jql": jql,
            "maxResults": max_results,
            "fields": fields,
        });
        if let Some(token) = input.get("next_page_token").and_then(|v| v.as_str()) {
            body["nextPageToken"] = json!(token);
        }

        let path = format!("{REST}/search/jql");
        let duration_ms = start.elapsed().as_millis() as u64;
        match client.post(&path, &body).await {
            Ok(resp) => {
                let issues = resp
                    .get("issues")
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default();
                let summary: Vec<Value> = issues
                    .iter()
                    .map(|i| {
                        let f = &i["fields"];
                        json!({
                            "key": i["key"],
                            "id": i["id"],
                            "summary": wrap_field(&f["summary"], "jira.search.summary"),
                            "status": f["status"]["name"],
                            "assignee": f["assignee"]["displayName"],
                            "priority": f["priority"]["name"],
                            "issuetype": f["issuetype"]["name"],
                            "updated": f["updated"],
                        })
                    })
                    .collect();
                info!(count = summary.len(), "Jira search");
                Ok(ToolResult::success(
                    json!({
                        "count": summary.len(),
                        "issues": summary,
                        "next_page_token": resp.get("nextPageToken"),
                        "is_last": resp.get("isLast"),
                    }),
                    duration_ms,
                ))
            }
            Err(e) => {
                warn!(error = %e, "Jira search failed");
                Ok(ToolResult::from_report(
                    &e.wrap_err("Jira search failed"),
                    duration_ms,
                ))
            }
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// 2. jira.get_issue
// ──────────────────────────────────────────────────────────────────────

jira_tool_struct!(JiraGetIssueTool);

#[async_trait]
impl Tool for JiraGetIssueTool {
    fn name(&self) -> &str {
        "jira.get_issue"
    }
    fn description(&self) -> &str {
        "Get full details of a Jira issue including comments, status, assignee, rendered description, and attachment list. Read an attachment's contents with jira.get_attachment."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "workspace": { "type": "string", "description": workspace_description(&self.clients, &self.default_workspace) },
                "issue_key": { "type": "string", "description": "Issue key, e.g. 'ENG-123'" }
            },
            "required": ["issue_key"]
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();
        let client = resolve_or_fail!(self, input, start);
        let key = match required_str(&input, "issue_key") {
            Ok(s) => s.to_string(),
            Err(e) => return Ok(ToolResult::failure(e, start.elapsed().as_millis() as u64)),
        };

        let path = format!("{REST}/issue/{key}?expand=renderedFields,changelog");
        let duration_ms = start.elapsed().as_millis() as u64;
        match client.get(&path).await {
            Ok(issue) => {
                let f = &issue["fields"];
                let rendered = &issue["renderedFields"];
                let out = json!({
                    "key": issue["key"],
                    "id": issue["id"],
                    "summary": wrap_field(&f["summary"], "jira.get_issue.summary"),
                    "description_html": wrap_field(&rendered["description"], "jira.get_issue.description"),
                    "status": f["status"]["name"],
                    "issuetype": f["issuetype"]["name"],
                    "priority": f["priority"]["name"],
                    "assignee": {
                        "display_name": f["assignee"]["displayName"],
                        "account_id": f["assignee"]["accountId"],
                    },
                    "reporter": {
                        "display_name": f["reporter"]["displayName"],
                        "account_id": f["reporter"]["accountId"],
                    },
                    "labels": f["labels"],
                    "created": f["created"],
                    "updated": f["updated"],
                    "comment_count": f["comment"]["total"],
                    "attachments": attachment_summaries(f),
                });
                Ok(ToolResult::success(out, duration_ms))
            }
            Err(e) => Ok(ToolResult::from_report(
                &e.wrap_err(format!("Failed to get issue {key}")),
                duration_ms,
            )),
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// 3. jira.create_issue
// ──────────────────────────────────────────────────────────────────────

jira_tool_struct!(JiraCreateIssueTool);

#[async_trait]
impl Tool for JiraCreateIssueTool {
    fn name(&self) -> &str {
        "jira.create_issue"
    }
    fn description(&self) -> &str {
        "Create a new Jira issue. Descriptions accept markdown (rendered to ADF), a raw ADF object, or null."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "workspace": { "type": "string", "description": workspace_description(&self.clients, &self.default_workspace) },
                "project_key": { "type": "string", "description": "Project key, e.g. 'ENG'. Falls back to default_project." },
                "summary": { "type": "string", "description": "Issue title" },
                "issue_type": { "type": "string", "description": "Issue type name (e.g. 'Bug', 'Task', 'Story')" },
                "description": { "type": "string", "description": "Markdown string (rendered to ADF — supports headings, bold, italic, strikethrough, inline code, fenced code blocks, lists, links, blockquotes), raw ADF object, or null." },
                "assignee_account_id": { "type": "string", "description": "Optional account ID to assign" },
                "labels": { "type": "array", "items": { "type": "string" } },
                "priority": { "type": "string", "description": "Priority name, e.g. 'High'" },
                "fields": { "type": "object", "description": "Additional raw fields (e.g. customfield_10035) to merge into the create payload" }
            },
            "required": ["summary", "issue_type"]
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();
        let client = resolve_or_fail!(self, input, start);
        let project_key = match resolve_project_key(&input, &client) {
            Ok(s) => s,
            Err(e) => return Ok(ToolResult::failure(e, start.elapsed().as_millis() as u64)),
        };
        let summary = match required_str(&input, "summary") {
            Ok(s) => s.to_string(),
            Err(e) => return Ok(ToolResult::failure(e, start.elapsed().as_millis() as u64)),
        };
        let issue_type = match required_str(&input, "issue_type") {
            Ok(s) => s.to_string(),
            Err(e) => return Ok(ToolResult::failure(e, start.elapsed().as_millis() as u64)),
        };

        let mut fields = json!({
            "project": { "key": project_key },
            "summary": summary,
            "issuetype": { "name": issue_type },
        });

        if let Some(desc) = input.get("description") {
            if !desc.is_null() {
                fields["description"] = to_adf(desc);
            }
        }
        if let Some(account_id) = input.get("assignee_account_id").and_then(|v| v.as_str()) {
            fields["assignee"] = json!({ "accountId": account_id });
        }
        if let Some(labels) = input.get("labels").and_then(|v| v.as_array()) {
            fields["labels"] = json!(labels);
        }
        if let Some(prio) = input.get("priority").and_then(|v| v.as_str()) {
            fields["priority"] = json!({ "name": prio });
        }
        if let Some(extra) = input.get("fields").and_then(|v| v.as_object()) {
            if let Some(obj) = fields.as_object_mut() {
                for (k, v) in extra {
                    obj.insert(k.clone(), v.clone());
                }
            }
        }

        let body = json!({ "fields": fields });
        let path = format!("{REST}/issue");
        let duration_ms = start.elapsed().as_millis() as u64;
        match client.post(&path, &body).await {
            Ok(resp) => {
                info!(key = ?resp["key"], "Jira issue created");
                Ok(ToolResult::success(
                    json!({ "key": resp["key"], "id": resp["id"], "self": resp["self"] }),
                    duration_ms,
                ))
            }
            Err(e) => {
                let e = enrich_priority_error(e, &client).await;
                Ok(ToolResult::from_report(
                    &e.wrap_err("Failed to create issue"),
                    start.elapsed().as_millis() as u64,
                ))
            }
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// 4. jira.update_issue
// ──────────────────────────────────────────────────────────────────────

jira_tool_struct!(JiraUpdateIssueTool);

#[async_trait]
impl Tool for JiraUpdateIssueTool {
    fn name(&self) -> &str {
        "jira.update_issue"
    }
    fn description(&self) -> &str {
        "Update fields on an existing Jira issue. Descriptions accept markdown (rendered to ADF) or a raw ADF object."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "workspace": { "type": "string", "description": workspace_description(&self.clients, &self.default_workspace) },
                "issue_key": { "type": "string" },
                "summary": { "type": "string" },
                "description": { "type": "string", "description": "Markdown string (rendered to ADF) or raw ADF object." },
                "labels": { "type": "array", "items": { "type": "string" } },
                "priority": { "type": "string" },
                "fields": { "type": "object", "description": "Raw fields object merged into the update payload (use for custom fields)" }
            },
            "required": ["issue_key"]
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();
        let client = resolve_or_fail!(self, input, start);
        let key = match required_str(&input, "issue_key") {
            Ok(s) => s.to_string(),
            Err(e) => return Ok(ToolResult::failure(e, start.elapsed().as_millis() as u64)),
        };

        let mut fields = serde_json::Map::new();
        if let Some(s) = input.get("summary").and_then(|v| v.as_str()) {
            fields.insert("summary".to_string(), json!(s));
        }
        if let Some(desc) = input.get("description") {
            if !desc.is_null() {
                fields.insert("description".to_string(), to_adf(desc));
            }
        }
        if let Some(labels) = input.get("labels").and_then(|v| v.as_array()) {
            fields.insert("labels".to_string(), json!(labels));
        }
        if let Some(prio) = input.get("priority").and_then(|v| v.as_str()) {
            fields.insert("priority".to_string(), json!({ "name": prio }));
        }
        if let Some(extra) = input.get("fields").and_then(|v| v.as_object()) {
            for (k, v) in extra {
                fields.insert(k.clone(), v.clone());
            }
        }

        if fields.is_empty() {
            return Ok(ToolResult::failure(
                "No fields to update".to_string(),
                start.elapsed().as_millis() as u64,
            ));
        }

        let body = json!({ "fields": Value::Object(fields) });
        let path = format!("{REST}/issue/{key}");
        let duration_ms = start.elapsed().as_millis() as u64;
        match client.put(&path, &body).await {
            Ok(_) => Ok(ToolResult::success(
                json!({ "key": key, "updated": true }),
                duration_ms,
            )),
            Err(e) => {
                let e = enrich_priority_error(e, &client).await;
                Ok(ToolResult::from_report(
                    &e.wrap_err(format!("Failed to update {key}")),
                    start.elapsed().as_millis() as u64,
                ))
            }
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// 5. jira.delete_issue
// ──────────────────────────────────────────────────────────────────────

jira_tool_struct!(JiraDeleteIssueTool);

#[async_trait]
impl Tool for JiraDeleteIssueTool {
    fn name(&self) -> &str {
        "jira.delete_issue"
    }
    fn description(&self) -> &str {
        "Delete a Jira issue permanently. Requires delete permission on the project."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "workspace": { "type": "string", "description": workspace_description(&self.clients, &self.default_workspace) },
                "issue_key": { "type": "string" }
            },
            "required": ["issue_key"]
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();
        let client = resolve_or_fail!(self, input, start);
        let key = match required_str(&input, "issue_key") {
            Ok(s) => s.to_string(),
            Err(e) => return Ok(ToolResult::failure(e, start.elapsed().as_millis() as u64)),
        };
        let path = format!("{REST}/issue/{key}");
        let duration_ms = start.elapsed().as_millis() as u64;
        match client.delete(&path).await {
            Ok(_) => Ok(ToolResult::success(
                json!({ "key": key, "deleted": true }),
                duration_ms,
            )),
            Err(e) => Ok(ToolResult::from_report(
                &e.wrap_err(format!("Failed to delete {key}")),
                duration_ms,
            )),
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// 6. jira.assign_issue
// ──────────────────────────────────────────────────────────────────────

jira_tool_struct!(JiraAssignIssueTool);

#[async_trait]
impl Tool for JiraAssignIssueTool {
    fn name(&self) -> &str {
        "jira.assign_issue"
    }
    fn description(&self) -> &str {
        "Assign a Jira issue to a user by accountId. Pass null to unassign."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "workspace": { "type": "string", "description": workspace_description(&self.clients, &self.default_workspace) },
                "issue_key": { "type": "string" },
                "account_id": { "type": ["string", "null"], "description": "Atlassian accountId, or null to unassign" }
            },
            "required": ["issue_key"]
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();
        let client = resolve_or_fail!(self, input, start);
        let key = match required_str(&input, "issue_key") {
            Ok(s) => s.to_string(),
            Err(e) => return Ok(ToolResult::failure(e, start.elapsed().as_millis() as u64)),
        };
        let account_id = input.get("account_id").cloned().unwrap_or(Value::Null);
        let body = json!({ "accountId": account_id });
        let path = format!("{REST}/issue/{key}/assignee");
        let duration_ms = start.elapsed().as_millis() as u64;
        match client.put(&path, &body).await {
            Ok(_) => Ok(ToolResult::success(
                json!({ "key": key, "assignee": account_id }),
                duration_ms,
            )),
            Err(e) => Ok(ToolResult::from_report(
                &e.wrap_err(format!("Failed to assign {key}")),
                duration_ms,
            )),
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// 7. jira.add_comment
// ──────────────────────────────────────────────────────────────────────

jira_tool_struct!(JiraAddCommentTool);

#[async_trait]
impl Tool for JiraAddCommentTool {
    fn name(&self) -> &str {
        "jira.add_comment"
    }
    fn description(&self) -> &str {
        "Add a comment to a Jira issue. Bodies accept markdown (rendered to ADF) or a raw ADF object."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "workspace": { "type": "string", "description": workspace_description(&self.clients, &self.default_workspace) },
                "issue_key": { "type": "string" },
                "body": { "type": "string", "description": "Markdown string (rendered to ADF) or raw ADF object." }
            },
            "required": ["issue_key", "body"]
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();
        let client = resolve_or_fail!(self, input, start);
        let key = match required_str(&input, "issue_key") {
            Ok(s) => s.to_string(),
            Err(e) => return Ok(ToolResult::failure(e, start.elapsed().as_millis() as u64)),
        };
        let body_val = input.get("body").cloned().unwrap_or(Value::Null);
        if body_val.is_null() {
            return Ok(ToolResult::failure(
                "Missing required parameter 'body'".to_string(),
                start.elapsed().as_millis() as u64,
            ));
        }
        let body = json!({ "body": to_adf(&body_val) });
        let path = format!("{REST}/issue/{key}/comment");
        let duration_ms = start.elapsed().as_millis() as u64;
        match client.post(&path, &body).await {
            Ok(resp) => Ok(ToolResult::success(
                json!({ "key": key, "comment_id": resp["id"] }),
                duration_ms,
            )),
            Err(e) => Ok(ToolResult::from_report(
                &e.wrap_err(format!("Failed to add comment to {key}")),
                duration_ms,
            )),
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// 8. jira.list_comments
// ──────────────────────────────────────────────────────────────────────

jira_tool_struct!(JiraListCommentsTool);

#[async_trait]
impl Tool for JiraListCommentsTool {
    fn name(&self) -> &str {
        "jira.list_comments"
    }
    fn description(&self) -> &str {
        "List comments on a Jira issue."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "workspace": { "type": "string", "description": workspace_description(&self.clients, &self.default_workspace) },
                "issue_key": { "type": "string" }
            },
            "required": ["issue_key"]
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();
        let client = resolve_or_fail!(self, input, start);
        let key = match required_str(&input, "issue_key") {
            Ok(s) => s.to_string(),
            Err(e) => return Ok(ToolResult::failure(e, start.elapsed().as_millis() as u64)),
        };
        let path = format!("{REST}/issue/{key}/comment?expand=renderedBody");
        let duration_ms = start.elapsed().as_millis() as u64;
        match client.get(&path).await {
            Ok(resp) => {
                let comments: Vec<Value> = resp
                    .get("comments")
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default()
                    .iter()
                    .map(|c| {
                        json!({
                            "id": c["id"],
                            "author": c["author"]["displayName"],
                            "author_account_id": c["author"]["accountId"],
                            "body_html": wrap_field(&c["renderedBody"], "jira.list_comments.body"),
                            "created": c["created"],
                            "updated": c["updated"],
                        })
                    })
                    .collect();
                Ok(ToolResult::success(
                    json!({ "issue_key": key, "count": comments.len(), "comments": comments }),
                    duration_ms,
                ))
            }
            Err(e) => Ok(ToolResult::from_report(
                &e.wrap_err(format!("Failed to list comments for {key}")),
                duration_ms,
            )),
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// 9. jira.list_transitions
// ──────────────────────────────────────────────────────────────────────

jira_tool_struct!(JiraListTransitionsTool);

#[async_trait]
impl Tool for JiraListTransitionsTool {
    fn name(&self) -> &str {
        "jira.list_transitions"
    }
    fn description(&self) -> &str {
        "List the workflow transitions available for a Jira issue (e.g. 'In Progress', 'Done')."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "workspace": { "type": "string", "description": workspace_description(&self.clients, &self.default_workspace) },
                "issue_key": { "type": "string" }
            },
            "required": ["issue_key"]
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();
        let client = resolve_or_fail!(self, input, start);
        let key = match required_str(&input, "issue_key") {
            Ok(s) => s.to_string(),
            Err(e) => return Ok(ToolResult::failure(e, start.elapsed().as_millis() as u64)),
        };
        let path = format!("{REST}/issue/{key}/transitions");
        let duration_ms = start.elapsed().as_millis() as u64;
        match client.get(&path).await {
            Ok(resp) => {
                let transitions: Vec<Value> = resp
                    .get("transitions")
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default()
                    .iter()
                    .map(|t| {
                        json!({
                            "id": t["id"],
                            "name": t["name"],
                            "to_status": t["to"]["name"],
                        })
                    })
                    .collect();
                Ok(ToolResult::success(
                    json!({ "issue_key": key, "transitions": transitions }),
                    duration_ms,
                ))
            }
            Err(e) => Ok(ToolResult::from_report(
                &e.wrap_err(format!("Failed to list transitions for {key}")),
                duration_ms,
            )),
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// 10. jira.transition_issue
// ──────────────────────────────────────────────────────────────────────

jira_tool_struct!(JiraTransitionIssueTool);

#[async_trait]
impl Tool for JiraTransitionIssueTool {
    fn name(&self) -> &str {
        "jira.transition_issue"
    }
    fn description(&self) -> &str {
        "Move a Jira issue through a workflow transition by transition id or name."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "workspace": { "type": "string", "description": workspace_description(&self.clients, &self.default_workspace) },
                "issue_key": { "type": "string" },
                "transition": { "type": "string", "description": "Transition id (preferred) or display name" }
            },
            "required": ["issue_key", "transition"]
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();
        let client = resolve_or_fail!(self, input, start);
        let key = match required_str(&input, "issue_key") {
            Ok(s) => s.to_string(),
            Err(e) => return Ok(ToolResult::failure(e, start.elapsed().as_millis() as u64)),
        };
        let transition = match required_str(&input, "transition") {
            Ok(s) => s.to_string(),
            Err(e) => return Ok(ToolResult::failure(e, start.elapsed().as_millis() as u64)),
        };

        // Resolve transition id: if input is numeric, use as-is; otherwise look up by name.
        let id = if transition.chars().all(|c| c.is_ascii_digit()) {
            transition.clone()
        } else {
            let path = format!("{REST}/issue/{key}/transitions");
            let resp = match client.get(&path).await {
                Ok(r) => r,
                Err(e) => {
                    return Ok(ToolResult::from_report(
                        &e.wrap_err(format!("Failed to look up transitions for {key}")),
                        start.elapsed().as_millis() as u64,
                    ))
                }
            };
            let found = resp
                .get("transitions")
                .and_then(|v| v.as_array())
                .and_then(|arr| {
                    arr.iter().find(|t| {
                        t["name"]
                            .as_str()
                            .map(|n| n.eq_ignore_ascii_case(&transition))
                            .unwrap_or(false)
                    })
                })
                .and_then(|t| t["id"].as_str().map(String::from));
            match found {
                Some(id) => id,
                None => {
                    return Ok(ToolResult::failure(
                        format!("Transition '{transition}' not available on {key}"),
                        start.elapsed().as_millis() as u64,
                    ))
                }
            }
        };

        let body = json!({ "transition": { "id": id } });
        let path = format!("{REST}/issue/{key}/transitions");
        let duration_ms = start.elapsed().as_millis() as u64;
        match client.post(&path, &body).await {
            Ok(_) => Ok(ToolResult::success(
                json!({ "key": key, "transitioned": true, "transition_id": id }),
                duration_ms,
            )),
            Err(e) => Ok(ToolResult::from_report(
                &e.wrap_err(format!("Failed to transition {key}")),
                duration_ms,
            )),
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// 11. jira.add_attachment
// ──────────────────────────────────────────────────────────────────────

jira_tool_struct!(JiraAddAttachmentTool);

#[async_trait]
impl Tool for JiraAddAttachmentTool {
    fn name(&self) -> &str {
        "jira.add_attachment"
    }
    fn description(&self) -> &str {
        "Upload a local file as an attachment on a Jira issue."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "workspace": { "type": "string", "description": workspace_description(&self.clients, &self.default_workspace) },
                "issue_key": { "type": "string" },
                "path": { "type": "string", "description": "Absolute path to the local file to upload" }
            },
            "required": ["issue_key", "path"]
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();
        let client = resolve_or_fail!(self, input, start);
        let key = match required_str(&input, "issue_key") {
            Ok(s) => s.to_string(),
            Err(e) => return Ok(ToolResult::failure(e, start.elapsed().as_millis() as u64)),
        };
        let file_path = match required_str(&input, "path") {
            Ok(s) => s.to_string(),
            Err(e) => return Ok(ToolResult::failure(e, start.elapsed().as_millis() as u64)),
        };

        let bytes = match tokio::fs::read(&file_path).await {
            Ok(b) => b,
            Err(e) => {
                return Ok(ToolResult::failure(
                    format!("Failed to read {file_path}: {e}"),
                    start.elapsed().as_millis() as u64,
                ))
            }
        };
        let filename = std::path::Path::new(&file_path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("attachment")
            .to_string();

        let part = reqwest::multipart::Part::bytes(bytes).file_name(filename.clone());
        let form = reqwest::multipart::Form::new().part("file", part);

        let path = format!("{REST}/issue/{key}/attachments");
        let duration_ms = start.elapsed().as_millis() as u64;
        match client.post_multipart(&path, form).await {
            Ok(resp) => Ok(ToolResult::success(
                json!({ "key": key, "filename": filename, "result": resp }),
                duration_ms,
            )),
            Err(e) => Ok(ToolResult::from_report(
                &e.wrap_err(format!("Failed to attach to {key}")),
                duration_ms,
            )),
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// 11b. jira.get_attachment
// ──────────────────────────────────────────────────────────────────────

jira_tool_struct!(JiraGetAttachmentTool);

#[async_trait]
impl Tool for JiraGetAttachmentTool {
    fn name(&self) -> &str {
        "jira.get_attachment"
    }
    fn description(&self) -> &str {
        "Download a Jira issue attachment and render it into your view so you can read it — images, \
         PDFs, documents, spreadsheets, audio, or video. Pass the `attachment_id` from a \
         jira.get_issue `attachments` entry. The file appears on your next step; wait for it before \
         describing the contents."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "workspace": { "type": "string", "description": workspace_description(&self.clients, &self.default_workspace) },
                "issue_key": { "type": "string", "description": "Issue the attachment belongs to, e.g. 'ENG-123'. Used only to route to the right workspace." },
                "attachment_id": { "type": "string", "description": "Attachment id from a jira.get_issue `attachments` entry." }
            },
            "required": ["attachment_id"]
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();
        let client = resolve_or_fail!(self, input, start);
        let id = match required_str(&input, "attachment_id") {
            Ok(s) => s.to_string(),
            Err(e) => return Ok(ToolResult::failure(e, start.elapsed().as_millis() as u64)),
        };

        let meta = match client.get(&format!("{REST}/attachment/{id}")).await {
            Ok(m) => m,
            Err(e) => {
                return Ok(ToolResult::from_report(
                    &e.wrap_err(format!("Failed to fetch attachment {id}")),
                    start.elapsed().as_millis() as u64,
                ))
            }
        };
        let size = coerce_to_u64(&meta["size"]).unwrap_or(0);
        if !within_view_cap(size) {
            return Ok(ToolResult::failure(
                format!(
                    "Attachment {id} is {size} bytes, over the {MAX_VIEW_BYTES}-byte limit; \
                     download it with jira.api instead."
                ),
                start.elapsed().as_millis() as u64,
            ));
        }
        let filename = meta["filename"]
            .as_str()
            .filter(|s| !s.is_empty())
            .unwrap_or("attachment")
            .to_string();
        let media_type = meta["mimeType"]
            .as_str()
            .filter(|s| !s.is_empty())
            .unwrap_or("application/octet-stream")
            .to_string();

        let bytes = match client
            .get_bytes(&format!("{REST}/attachment/content/{id}"))
            .await
        {
            Ok(b) => b,
            Err(e) => {
                return Ok(ToolResult::from_report(
                    &e.wrap_err(format!("Failed to download attachment {id}")),
                    start.elapsed().as_millis() as u64,
                ))
            }
        };
        if !within_view_cap(bytes.len() as u64) {
            return Ok(ToolResult::failure(
                format!("Attachment {id} exceeds the {MAX_VIEW_BYTES}-byte limit"),
                start.elapsed().as_millis() as u64,
            ));
        }

        let staged = match stage_for_view("jira.get_attachment", bytes, &media_type, &filename) {
            Ok(s) => s,
            Err(msg) => return Ok(ToolResult::failure(msg, start.elapsed().as_millis() as u64)),
        };

        let duration_ms = start.elapsed().as_millis() as u64;
        info!(attachment = %id, filename = %filename, "Staged Jira attachment for viewing");
        Ok(ToolResult::success(
            json!({
                VIEWED_IMAGES_KEY: staged.marker,
                "note": view_pending_note(&filename),
            }),
            duration_ms,
        ))
    }
}

// ──────────────────────────────────────────────────────────────────────
// 12. jira.list_projects
// ──────────────────────────────────────────────────────────────────────

jira_tool_struct!(JiraListProjectsTool);

#[async_trait]
impl Tool for JiraListProjectsTool {
    fn name(&self) -> &str {
        "jira.list_projects"
    }
    fn description(&self) -> &str {
        "List Jira projects accessible to the authenticated user."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "workspace": { "type": "string", "description": workspace_description(&self.clients, &self.default_workspace) },
                "query": { "type": "string", "description": "Optional substring filter on project key/name" },
                "max_results": { "type": "integer", "description": "Page size (default 50, max 100)" }
            }
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();
        let client = resolve_or_fail!(self, input, start);
        let max = coerce_to_u64(&input["max_results"])
            .unwrap_or(50)
            .min(100)
            .to_string();
        let mut q: Vec<(&str, &str)> = vec![("maxResults", max.as_str())];
        let query_str = input.get("query").and_then(|v| v.as_str()).unwrap_or("");
        if !query_str.is_empty() {
            q.push(("query", query_str));
        }
        let path = format!("{REST}/project/search");
        let duration_ms = start.elapsed().as_millis() as u64;
        match client.get_with_query(&path, &q).await {
            Ok(resp) => {
                let projects: Vec<Value> = resp
                    .get("values")
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default()
                    .iter()
                    .map(|p| {
                        json!({
                            "key": p["key"],
                            "id": p["id"],
                            "name": p["name"],
                            "project_type": p["projectTypeKey"],
                            "lead": p["lead"]["displayName"],
                        })
                    })
                    .collect();
                Ok(ToolResult::success(
                    json!({ "count": projects.len(), "projects": projects }),
                    duration_ms,
                ))
            }
            Err(e) => Ok(ToolResult::from_report(
                &e.wrap_err("Failed to list projects"),
                duration_ms,
            )),
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// 13. jira.get_project
// ──────────────────────────────────────────────────────────────────────

jira_tool_struct!(JiraGetProjectTool);

#[async_trait]
impl Tool for JiraGetProjectTool {
    fn name(&self) -> &str {
        "jira.get_project"
    }
    fn description(&self) -> &str {
        "Get details about a Jira project, including its issue types and components."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "workspace": { "type": "string", "description": workspace_description(&self.clients, &self.default_workspace) },
                "project_key": { "type": "string", "description": "Project key or numeric id" }
            },
            "required": ["project_key"]
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();
        let client = resolve_or_fail!(self, input, start);
        let key = match required_str(&input, "project_key") {
            Ok(s) => s.to_string(),
            Err(e) => return Ok(ToolResult::failure(e, start.elapsed().as_millis() as u64)),
        };
        let path = format!("{REST}/project/{key}");
        let duration_ms = start.elapsed().as_millis() as u64;
        match client.get(&path).await {
            Ok(p) => {
                let issue_types: Vec<Value> = p
                    .get("issueTypes")
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default()
                    .iter()
                    .map(|t| json!({ "id": t["id"], "name": t["name"] }))
                    .collect();
                Ok(ToolResult::success(
                    json!({
                        "key": p["key"],
                        "id": p["id"],
                        "name": p["name"],
                        "description": wrap_field(&p["description"], "jira.get_project.description"),
                        "lead": p["lead"]["displayName"],
                        "issue_types": issue_types,
                    }),
                    duration_ms,
                ))
            }
            Err(e) => Ok(ToolResult::from_report(
                &e.wrap_err(format!("Failed to get project {key}")),
                duration_ms,
            )),
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// 14. jira.search_users
// ──────────────────────────────────────────────────────────────────────

jira_tool_struct!(JiraSearchUsersTool);

#[async_trait]
impl Tool for JiraSearchUsersTool {
    fn name(&self) -> &str {
        "jira.search_users"
    }
    fn description(&self) -> &str {
        "Search Jira users by name or email. Returns accountId for use with assign_issue."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "workspace": { "type": "string", "description": workspace_description(&self.clients, &self.default_workspace) },
                "query": { "type": "string", "description": "Search string (name or email substring)" },
                "max_results": { "type": "integer", "description": "Default 20, max 50" }
            },
            "required": ["query"]
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();
        let client = resolve_or_fail!(self, input, start);
        let q = match required_str(&input, "query") {
            Ok(s) => s.to_string(),
            Err(e) => return Ok(ToolResult::failure(e, start.elapsed().as_millis() as u64)),
        };
        let max = coerce_to_u64(&input["max_results"])
            .unwrap_or(20)
            .min(50)
            .to_string();
        let path = format!("{REST}/user/search");
        let duration_ms = start.elapsed().as_millis() as u64;
        match client
            .get_with_query(
                &path,
                &[("query", q.as_str()), ("maxResults", max.as_str())],
            )
            .await
        {
            Ok(resp) => {
                let users: Vec<Value> = resp
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
                    .iter()
                    .map(|u| {
                        json!({
                            "account_id": u["accountId"],
                            "display_name": u["displayName"],
                            "email": u["emailAddress"],
                            "active": u["active"],
                        })
                    })
                    .collect();
                Ok(ToolResult::success(
                    json!({ "count": users.len(), "users": users }),
                    duration_ms,
                ))
            }
            Err(e) => Ok(ToolResult::from_report(
                &e.wrap_err("Failed to search users"),
                duration_ms,
            )),
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// 15. jira.list_boards (Agile)
// ──────────────────────────────────────────────────────────────────────

jira_tool_struct!(JiraListBoardsTool);

#[async_trait]
impl Tool for JiraListBoardsTool {
    fn name(&self) -> &str {
        "jira.list_boards"
    }
    fn description(&self) -> &str {
        "List Jira Software (Agile) boards accessible to the authenticated user."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "workspace": { "type": "string", "description": workspace_description(&self.clients, &self.default_workspace) },
                "project_key": { "type": "string", "description": "Optional project key filter" },
                "type": { "type": "string", "enum": ["scrum", "kanban", "simple"] },
                "max_results": { "type": "integer", "description": "Default 50" }
            }
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();
        let client = resolve_or_fail!(self, input, start);
        let max = coerce_to_u64(&input["max_results"])
            .unwrap_or(50)
            .to_string();
        let mut q: Vec<(&str, &str)> = vec![("maxResults", max.as_str())];
        let pk = input
            .get("project_key")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if !pk.is_empty() {
            q.push(("projectKeyOrId", pk));
        }
        let board_type = input.get("type").and_then(|v| v.as_str()).unwrap_or("");
        if !board_type.is_empty() {
            q.push(("type", board_type));
        }
        let path = format!("{AGILE}/board");
        let duration_ms = start.elapsed().as_millis() as u64;
        match client.get_with_query(&path, &q).await {
            Ok(resp) => {
                let boards: Vec<Value> = resp
                    .get("values")
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default()
                    .iter()
                    .map(|b| {
                        json!({
                            "id": b["id"],
                            "name": b["name"],
                            "type": b["type"],
                            "project_key": b["location"]["projectKey"],
                        })
                    })
                    .collect();
                Ok(ToolResult::success(
                    json!({ "count": boards.len(), "boards": boards }),
                    duration_ms,
                ))
            }
            Err(e) => Ok(ToolResult::from_report(
                &e.wrap_err("Failed to list boards"),
                duration_ms,
            )),
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// 16. jira.list_sprints (Agile)
// ──────────────────────────────────────────────────────────────────────

jira_tool_struct!(JiraListSprintsTool);

#[async_trait]
impl Tool for JiraListSprintsTool {
    fn name(&self) -> &str {
        "jira.list_sprints"
    }
    fn description(&self) -> &str {
        "List sprints on a Jira board. Filter by state (active, closed, future)."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "workspace": { "type": "string", "description": workspace_description(&self.clients, &self.default_workspace) },
                "board_id": { "type": "integer" },
                "state": { "type": "string", "enum": ["active", "closed", "future"] }
            },
            "required": ["board_id"]
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();
        let client = resolve_or_fail!(self, input, start);
        let board_id = match coerce_to_i64(&input["board_id"]) {
            Some(n) => n,
            None => {
                return Ok(ToolResult::failure(
                    "Missing or invalid 'board_id'".to_string(),
                    start.elapsed().as_millis() as u64,
                ))
            }
        };
        let state = input.get("state").and_then(|v| v.as_str()).unwrap_or("");
        let path = format!("{AGILE}/board/{board_id}/sprint");
        let duration_ms = start.elapsed().as_millis() as u64;
        let result = if state.is_empty() {
            client.get(&path).await
        } else {
            client.get_with_query(&path, &[("state", state)]).await
        };
        match result {
            Ok(resp) => {
                let sprints: Vec<Value> = resp
                    .get("values")
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default()
                    .iter()
                    .map(|s| {
                        json!({
                            "id": s["id"],
                            "name": s["name"],
                            "state": s["state"],
                            "start_date": s["startDate"],
                            "end_date": s["endDate"],
                            "goal": wrap_field(&s["goal"], "jira.list_sprints.goal"),
                        })
                    })
                    .collect();
                Ok(ToolResult::success(
                    json!({ "board_id": board_id, "count": sprints.len(), "sprints": sprints }),
                    duration_ms,
                ))
            }
            Err(e) => Ok(ToolResult::from_report(
                &e.wrap_err("Failed to list sprints"),
                duration_ms,
            )),
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// 17. jira.add_worklog
// ──────────────────────────────────────────────────────────────────────

jira_tool_struct!(JiraAddWorklogTool);

#[async_trait]
impl Tool for JiraAddWorklogTool {
    fn name(&self) -> &str {
        "jira.add_worklog"
    }
    fn description(&self) -> &str {
        "Log time spent on a Jira issue. time_spent uses Jira format like '3h 30m' or '1d 2h'."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "workspace": { "type": "string", "description": workspace_description(&self.clients, &self.default_workspace) },
                "issue_key": { "type": "string" },
                "time_spent": { "type": "string", "description": "Jira duration string, e.g. '3h 30m'" },
                "comment": { "type": "string", "description": "Optional comment — markdown string (rendered to ADF) or raw ADF object." },
                "started": { "type": "string", "description": "Optional ISO-8601 timestamp; defaults to now on the server" }
            },
            "required": ["issue_key", "time_spent"]
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();
        let client = resolve_or_fail!(self, input, start);
        let key = match required_str(&input, "issue_key") {
            Ok(s) => s.to_string(),
            Err(e) => return Ok(ToolResult::failure(e, start.elapsed().as_millis() as u64)),
        };
        let time_spent = match required_str(&input, "time_spent") {
            Ok(s) => s.to_string(),
            Err(e) => return Ok(ToolResult::failure(e, start.elapsed().as_millis() as u64)),
        };
        let mut body = json!({ "timeSpent": time_spent });
        if let Some(c) = input.get("comment") {
            if !c.is_null() {
                body["comment"] = to_adf(c);
            }
        }
        if let Some(s) = input.get("started").and_then(|v| v.as_str()) {
            body["started"] = json!(s);
        }
        let path = format!("{REST}/issue/{key}/worklog");
        let duration_ms = start.elapsed().as_millis() as u64;
        match client.post(&path, &body).await {
            Ok(resp) => Ok(ToolResult::success(
                json!({ "key": key, "worklog_id": resp["id"], "time_spent": time_spent }),
                duration_ms,
            )),
            Err(e) => Ok(ToolResult::from_report(
                &e.wrap_err(format!("Failed to add worklog to {key}")),
                duration_ms,
            )),
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// 18. jira.api — escape hatch
// ──────────────────────────────────────────────────────────────────────

jira_tool_struct!(JiraApiTool);

#[async_trait]
impl Tool for JiraApiTool {
    fn name(&self) -> &str {
        "jira.api"
    }
    fn description(&self) -> &str {
        "Make an arbitrary Jira REST request. Use sparingly — prefer the dedicated tools."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "workspace": { "type": "string", "description": workspace_description(&self.clients, &self.default_workspace) },
                "method": { "type": "string", "enum": ["GET", "POST", "PUT", "DELETE"] },
                "path": { "type": "string", "description": "Path including the /rest/api/3 or /rest/agile/1.0 prefix" },
                "body": { "type": "object", "description": "JSON body for POST/PUT" },
                "query": { "type": "object", "description": "Query string params" }
            },
            "required": ["method", "path"]
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();
        let client = resolve_or_fail!(self, input, start);
        let method = match required_str(&input, "method") {
            Ok(s) => s.to_uppercase(),
            Err(e) => return Ok(ToolResult::failure(e, start.elapsed().as_millis() as u64)),
        };
        let path = match required_str(&input, "path") {
            Ok(s) => s.to_string(),
            Err(e) => return Ok(ToolResult::failure(e, start.elapsed().as_millis() as u64)),
        };
        let empty = json!({});
        let body = input.get("body").unwrap_or(&empty);

        let duration_ms = start.elapsed().as_millis() as u64;
        let result = match method.as_str() {
            "GET" => {
                if let Some(q) = input.get("query").and_then(|v| v.as_object()) {
                    let pairs: Vec<(String, String)> = q
                        .iter()
                        .filter_map(|(k, v)| {
                            v.as_str()
                                .map(String::from)
                                .or_else(|| v.as_i64().map(|n| n.to_string()))
                                .or_else(|| v.as_bool().map(|b| b.to_string()))
                                .map(|s| (k.clone(), s))
                        })
                        .collect();
                    let refs: Vec<(&str, &str)> = pairs
                        .iter()
                        .map(|(k, v)| (k.as_str(), v.as_str()))
                        .collect();
                    client.get_with_query(&path, &refs).await
                } else {
                    client.get(&path).await
                }
            }
            "POST" => client.post(&path, body).await,
            "PUT" => client.put(&path, body).await,
            "DELETE" => client.delete(&path).await,
            other => Err(eyre::eyre!("Unsupported method: {other}")),
        };
        match result {
            Ok(v) => Ok(ToolResult::success(v, duration_ms)),
            Err(e) => Ok(ToolResult::from_report(
                &e.wrap_err("Jira API request failed"),
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

    type ClientMap = Arc<HashMap<String, Arc<JiraClient>>>;
    type PrefixMap = Arc<HashMap<String, String>>;

    fn make_client(site: &str) -> Arc<JiraClient> {
        Arc::new(JiraClient::new("you@acme.com", "tok", site, None).unwrap())
    }

    fn fixture_clients() -> (ClientMap, PrefixMap) {
        let clients: HashMap<String, Arc<JiraClient>> = [
            ("acme", "https://acme.atlassian.net"),
            ("client", "https://client.atlassian.net"),
        ]
        .into_iter()
        .map(|(name, site)| (name.to_string(), make_client(site)))
        .collect();
        let prefix: HashMap<String, String> =
            [("ENG", "acme"), ("INFRA", "acme"), ("OPS", "client")]
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
        (Arc::new(clients), Arc::new(prefix))
    }

    #[test]
    fn attachment_summaries_maps_fields_and_wraps_filename() {
        let fields = json!({
            "attachment": [
                { "id": "69928", "filename": "report.pdf", "mimeType": "application/pdf", "size": 640606 },
                { "id": "70001", "filename": "diagram.png", "mimeType": "image/png", "size": 2054 }
            ]
        });
        let out = attachment_summaries(&fields);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0]["id"], json!("69928"));
        assert_eq!(out[0]["mime_type"], json!("application/pdf"));
        assert_eq!(out[0]["size"], json!(640606));
        assert_eq!(
            out[0]["filename"],
            wrap_field(&json!("report.pdf"), "jira.get_issue.attachment_filename"),
            "filename is routed through the external-content boundary"
        );
    }

    #[test]
    fn attachment_summaries_empty_when_absent_or_not_array() {
        assert!(attachment_summaries(&json!({})).is_empty());
        assert!(attachment_summaries(&json!({ "attachment": null })).is_empty());
        assert!(attachment_summaries(&json!({ "attachment": "nope" })).is_empty());
        assert!(attachment_summaries(&json!({ "attachment": [] })).is_empty());
    }

    #[test]
    fn resolve_client_uses_explicit_workspace() {
        let (clients, prefix) = fixture_clients();
        let input = json!({ "workspace": "client", "issue_key": "ENG-1" });
        let c = resolve_client(&clients, "acme", &prefix, &input).unwrap();
        assert_eq!(c.base_url(), Some("https://client.atlassian.net"));
    }

    #[test]
    fn resolve_client_explicit_unknown_errors() {
        let (clients, prefix) = fixture_clients();
        let input = json!({ "workspace": "ghost" });
        let err = resolve_client(&clients, "acme", &prefix, &input)
            .err()
            .expect("unknown workspace must error")
            .to_string();
        assert!(err.contains("Unknown Jira workspace: 'ghost'"), "{err}");
    }

    #[test]
    fn resolve_client_with_a_default_that_exists_nowhere_lists_the_workspaces() {
        let (clients, prefix) = fixture_clients();
        let err = resolve_client(&clients, "gone", &prefix, &json!({}))
            .err()
            .expect("absent default must error")
            .to_string();
        assert!(
            err.contains("Default Jira workspace 'gone' not configured"),
            "{err}"
        );
        assert!(err.contains("acme") && err.contains("client"), "{err}");
        assert!(workspace_description(&clients, "gone").contains("Default: none — pass workspace"));
    }

    #[test]
    fn resolve_client_under_environment_lock() {
        use workforce_agent::runner::{ACTIVE_ENVIRONMENT, ACTIVE_SKILL_INSTANCES};
        let (clients, prefix) = fixture_clients();
        let pins: workforce_agent::llm::environment::SkillPins =
            [("jira".to_string(), vec!["acme".to_string()])].into();
        ACTIVE_ENVIRONMENT.sync_scope("harbor-prod".to_string(), || {
            ACTIVE_SKILL_INSTANCES.sync_scope(pins, || {
                let acme = clients.get("acme").unwrap();
                // No routable key → the pin (acme), overriding the "client" default.
                let c = resolve_client(&clients, "client", &prefix, &json!({})).unwrap();
                assert!(Arc::ptr_eq(&c, acme));
                // A key routing to the SAME workspace as the pin resolves.
                let c = resolve_client(
                    &clients,
                    "client",
                    &prefix,
                    &json!({ "issue_key": "ENG-1" }),
                )
                .unwrap();
                assert!(Arc::ptr_eq(&c, acme));
                // A key auto-routing to a DIFFERENT workspace is refused as dormant.
                assert!(resolve_client(
                    &clients,
                    "client",
                    &prefix,
                    &json!({ "issue_key": "OPS-9" })
                )
                .is_err());
                // An explicit mismatching workspace is refused.
                assert!(resolve_client(
                    &clients,
                    "client",
                    &prefix,
                    &json!({ "workspace": "client" })
                )
                .is_err());
            });
        });
    }

    #[test]
    fn resolve_client_auto_routes_from_issue_key_prefix() {
        let (clients, prefix) = fixture_clients();
        let input = json!({ "issue_key": "OPS-42" });
        let c = resolve_client(&clients, "acme", &prefix, &input).unwrap();
        assert_eq!(c.base_url(), Some("https://client.atlassian.net"));
    }

    #[test]
    fn resolve_client_auto_routes_from_project_key() {
        let (clients, prefix) = fixture_clients();
        let input = json!({ "project_key": "infra" }); // case-insensitive
        let c = resolve_client(&clients, "acme", &prefix, &input).unwrap();
        assert_eq!(c.base_url(), Some("https://acme.atlassian.net"));
    }

    #[test]
    fn resolve_client_falls_back_to_default() {
        let (clients, prefix) = fixture_clients();
        let input = json!({});
        let c = resolve_client(&clients, "acme", &prefix, &input).unwrap();
        assert_eq!(c.base_url(), Some("https://acme.atlassian.net"));
    }

    #[test]
    fn resolve_client_unknown_prefix_falls_back_to_default() {
        let (clients, prefix) = fixture_clients();
        let input = json!({ "issue_key": "ZZZ-1" });
        let c = resolve_client(&clients, "acme", &prefix, &input).unwrap();
        assert_eq!(c.base_url(), Some("https://acme.atlassian.net"));
    }

    #[test]
    fn priority_rejections_classify_and_teach_verbatim_names() {
        assert!(is_priority_rejection(
            "Bad request: priority: The priority selected is invalid."
        ));
        assert!(is_priority_rejection(
            "Priority is INVALID for this project"
        ));
        assert!(!is_priority_rejection(
            "Bad request: description: not valid ADF content"
        ));
        assert!(!is_priority_rejection("priority updated"));

        let names = vec![
            "P1".to_string(),
            "Now".to_string(),
            "Not Prioritised ".to_string(),
        ];
        let hint = priority_hint(&names);
        assert_eq!(
            hint,
            "Valid priorities on this Jira: \"P1\", \"Now\", \"Not Prioritised \""
        );
    }

    #[test]
    fn to_adf_renders_plain_text_as_single_paragraph() {
        let v = to_adf(&json!("hello world"));
        assert_eq!(v["type"], "doc");
        assert_eq!(v["version"], 1);
        assert_eq!(v["content"][0]["type"], "paragraph");
        assert_eq!(v["content"][0]["content"][0]["text"], "hello world");
    }

    #[test]
    fn to_adf_passes_object_through() {
        let raw = json!({ "type": "doc", "version": 1, "content": [] });
        let v = to_adf(&raw);
        assert_eq!(v, raw);
    }

    #[test]
    fn to_adf_null_produces_empty_doc() {
        let v = to_adf(&Value::Null);
        assert_eq!(v["type"], "doc");
        assert_eq!(v["version"], 1);
        assert_eq!(v["content"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn to_adf_empty_string_produces_empty_doc() {
        let v = to_adf(&json!(""));
        assert_eq!(v["type"], "doc");
        assert_eq!(v["content"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn to_adf_string_with_markdown_renders_marks() {
        // Smoke test: confirms the string branch delegates to the markdown
        // converter. `**bold**` should not survive as literal asterisks.
        let v = to_adf(&json!("**bold**"));
        let text = &v["content"][0]["content"][0];
        assert_eq!(text["text"], "bold");
        assert_eq!(text["marks"][0]["type"], "strong");
    }

    #[test]
    fn required_str_rejects_empty_string() {
        let input = json!({ "summary": "" });
        assert!(required_str(&input, "summary").is_err());
    }

    #[test]
    fn required_str_rejects_missing_key() {
        let input = json!({});
        assert!(required_str(&input, "summary").is_err());
    }

    #[test]
    fn workspace_description_lists_available_workspaces() {
        let (clients, _) = fixture_clients();
        let desc = workspace_description(&clients, "acme");
        assert!(desc.contains("acme"));
        assert!(desc.contains("client"));
        assert!(desc.contains("Default: acme"));
    }
}
