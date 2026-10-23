//! Notion agent-callable tools.
//!
//! Every tool accepts an optional `workspace` parameter for multi-workspace
//! selection. Rich text and title values from Notion are wrapped via
//! `wrap_external_content` to defend against prompt injection.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use eyre::Result;
use serde_json::{json, Value};
use tracing::{info, warn};

use workforce_agent::security::external_content::wrap_external_content;
use workforce_agent::tools::{Tool, ToolResult};

use super::client::NotionClient;
use super::config::clamp_limit;
use crate::adapters::utils::{coerce_to_u64, instance_choices};

// ── shared helpers ──────────────────────────────────────────────────

fn resolve_client(
    clients: &HashMap<String, Arc<NotionClient>>,
    default_workspace: &str,
    input: &Value,
) -> eyre::Result<Arc<NotionClient>> {
    if let Some(name) = input["workspace"].as_str() {
        return clients.get(name).cloned().ok_or_else(|| {
            eyre::eyre!(
                "Unknown Notion workspace: '{name}'. Available: {:?}",
                clients.keys().collect::<Vec<_>>()
            )
        });
    }
    clients.get(default_workspace).cloned().ok_or_else(|| {
        eyre::eyre!(
            "Unknown Notion workspace: '{default_workspace}'. Available: {:?}",
            clients.keys().collect::<Vec<_>>()
        )
    })
}

fn workspace_description(
    clients: &HashMap<String, Arc<NotionClient>>,
    default_workspace: &str,
) -> String {
    if clients.len() <= 1 {
        return "Notion workspace name (for multi-workspace setups)".to_string();
    }
    format!(
        "Notion workspace. {}",
        instance_choices(clients, default_workspace, "workspace")
    )
}

fn workspace_schema_prop(
    clients: &HashMap<String, Arc<NotionClient>>,
    default_workspace: &str,
) -> Value {
    json!({
        "type": "string",
        "description": workspace_description(clients, default_workspace)
    })
}

/// Wrap `plain_text` fields recursively to defend against prompt injection.
fn wrap_rich_text(mut value: Value, source: &str) -> Value {
    wrap_rich_text_recursive(&mut value, source);
    value
}

fn wrap_rich_text_recursive(value: &mut Value, source: &str) {
    match value {
        Value::Object(map) => {
            if let Some(Value::String(s)) = map.get("plain_text") {
                if !s.is_empty() {
                    let wrapped = wrap_external_content(s, source).wrapped_content;
                    map.insert("plain_text".to_string(), Value::String(wrapped));
                }
            }
            for (_k, v) in map.iter_mut() {
                wrap_rich_text_recursive(v, source);
            }
        }
        Value::Array(arr) => {
            for v in arr.iter_mut() {
                wrap_rich_text_recursive(v, source);
            }
        }
        _ => {}
    }
}

fn wrap_results(results: Vec<Value>, source: &str) -> Vec<Value> {
    results
        .into_iter()
        .map(|r| wrap_rich_text(r, source))
        .collect()
}

fn extract_paging(resp: &Value) -> Value {
    if resp.get("has_more").and_then(|v| v.as_bool()) == Some(true) {
        json!({
            "has_more": true,
            "next_cursor": resp.get("next_cursor").cloned().unwrap_or(Value::Null),
        })
    } else {
        json!({ "has_more": false })
    }
}

fn coerce_id(val: &Value) -> eyre::Result<String> {
    if let Some(s) = val.as_str() {
        Ok(s.to_string())
    } else {
        Err(eyre::eyre!("Missing or invalid 'id' parameter"))
    }
}

#[derive(Clone)]
struct Ctx {
    clients: Arc<HashMap<String, Arc<NotionClient>>>,
    default_workspace: String,
}

impl Ctx {
    fn client(&self, input: &Value) -> Result<Arc<NotionClient>> {
        resolve_client(&self.clients, &self.default_workspace, input)
    }
    fn workspace_prop(&self) -> Value {
        workspace_schema_prop(&self.clients, &self.default_workspace)
    }
}

macro_rules! notion_tool {
    ($struct:ident) => {
        pub struct $struct {
            ctx: Ctx,
        }
        impl $struct {
            pub fn new(
                clients: Arc<HashMap<String, Arc<NotionClient>>>,
                default_workspace: String,
            ) -> Self {
                Self {
                    ctx: Ctx {
                        clients,
                        default_workspace,
                    },
                }
            }
        }
    };
}

async fn run<F, Fut>(ctx: &Ctx, input: Value, f: F) -> Result<ToolResult>
where
    F: FnOnce(Arc<NotionClient>, Value) -> Fut,
    Fut: std::future::Future<Output = Result<Value>>,
{
    let start = std::time::Instant::now();
    let client = match ctx.client(&input) {
        Ok(c) => c,
        Err(e) => {
            return Ok(ToolResult::failure(
                e.to_string(),
                start.elapsed().as_millis() as u64,
            ))
        }
    };
    match f(client, input).await {
        Ok(output) => Ok(ToolResult::success(
            output,
            start.elapsed().as_millis() as u64,
        )),
        Err(e) => {
            warn!(error = %e, "Notion tool failed");
            Ok(ToolResult::from_report(
                &e,
                start.elapsed().as_millis() as u64,
            ))
        }
    }
}

// ── notion.search ───────────────────────────────────────────────────

notion_tool!(SearchTool);

#[async_trait]
impl Tool for SearchTool {
    fn name(&self) -> &str {
        "notion.search"
    }
    fn description(&self) -> &str {
        "Search pages and databases in the workspace by title."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "Text to search for in page/database titles" },
                "filter": {
                    "type": "object",
                    "description": "Filter by object type: {\"value\": \"page\"} or {\"value\": \"database\"}",
                    "properties": {
                        "value": { "type": "string", "enum": ["page", "database"] },
                        "property": { "type": "string", "enum": ["object"] }
                    }
                },
                "sort": {
                    "type": "object",
                    "description": "Sort results: {\"direction\": \"ascending\"|\"descending\", \"timestamp\": \"last_edited_time\"}",
                },
                "page_size": { "type": "integer", "description": "Max results (default/max 100)", "minimum": 1, "maximum": 100 },
                "start_cursor": { "type": "string", "description": "Pagination cursor from previous response" },
                "workspace": self.ctx.workspace_prop(),
            }
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        run(&self.ctx, input, |client, input| async move {
            let page_size = clamp_limit(coerce_to_u64(&input["page_size"]), client.max_results);
            let mut body = json!({ "page_size": page_size });
            if let Some(q) = input["query"].as_str() {
                body["query"] = json!(q);
            }
            if let Some(f) = input.get("filter").filter(|v| v.is_object()) {
                body["filter"] = f.clone();
            }
            if let Some(s) = input.get("sort").filter(|v| v.is_object()) {
                body["sort"] = s.clone();
            }
            if let Some(cursor) = input["start_cursor"].as_str() {
                body["start_cursor"] = json!(cursor);
            }
            let resp = client.post("/search", body).await?;
            let results = resp["results"].as_array().cloned().unwrap_or_default();
            let wrapped = wrap_results(results, "notion_search");
            let paging = extract_paging(&resp);
            info!(count = wrapped.len(), "Notion search");
            Ok(json!({ "count": wrapped.len(), "results": wrapped, "paging": paging }))
        })
        .await
    }
}

// ── notion.page_get ─────────────────────────────────────────────────

notion_tool!(PageGetTool);

#[async_trait]
impl Tool for PageGetTool {
    fn name(&self) -> &str {
        "notion.page_get"
    }
    fn description(&self) -> &str {
        "Get a page by ID. Returns page properties (not content blocks — use block_children for that)."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "description": "Page ID (UUID)" },
                "workspace": self.ctx.workspace_prop(),
            },
            "required": ["id"]
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        run(&self.ctx, input, |client, input| async move {
            let id = coerce_id(&input["id"])?;
            let page = client.get(&format!("/pages/{id}"), &[]).await?;
            Ok(wrap_rich_text(page, "notion_page"))
        })
        .await
    }
}

// ── notion.page_create ──────────────────────────────────────────────

notion_tool!(PageCreateTool);

#[async_trait]
impl Tool for PageCreateTool {
    fn name(&self) -> &str {
        "notion.page_create"
    }
    fn description(&self) -> &str {
        "MUTATING: create a new page. Specify parent as a database or another page."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "parent": {
                    "type": "object",
                    "description": "Parent: {\"database_id\": \"...\"} or {\"page_id\": \"...\"}"
                },
                "properties": {
                    "type": "object",
                    "description": "Page properties matching the parent database schema (or {\"title\": [{\"text\": {\"content\": \"...\"}}]} for page parents)"
                },
                "children": {
                    "type": "array",
                    "description": "Initial content blocks (max 100)",
                    "items": { "type": "object" }
                },
                "icon": { "type": "object", "description": "Page icon: {\"emoji\": \"...\"} or {\"external\": {\"url\": \"...\"}}" },
                "cover": { "type": "object", "description": "Cover image: {\"external\": {\"url\": \"...\"}}" },
                "workspace": self.ctx.workspace_prop(),
            },
            "required": ["parent", "properties"]
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        run(&self.ctx, input, |client, input| async move {
            let parent = input
                .get("parent")
                .filter(|v| v.is_object())
                .cloned()
                .ok_or_else(|| eyre::eyre!("Missing 'parent' object"))?;
            let properties = input
                .get("properties")
                .filter(|v| v.is_object())
                .cloned()
                .ok_or_else(|| eyre::eyre!("Missing 'properties' object"))?;
            let mut body = json!({ "parent": parent, "properties": properties });
            if let Some(children) = input.get("children").filter(|v| v.is_array()) {
                body["children"] = children.clone();
            }
            if let Some(icon) = input.get("icon").filter(|v| v.is_object()) {
                body["icon"] = icon.clone();
            }
            if let Some(cover) = input.get("cover").filter(|v| v.is_object()) {
                body["cover"] = cover.clone();
            }
            let result = client.post("/pages", body).await?;
            info!("Created Notion page");
            Ok(result)
        })
        .await
    }
}

// ── notion.page_update ──────────────────────────────────────────────

notion_tool!(PageUpdateTool);

#[async_trait]
impl Tool for PageUpdateTool {
    fn name(&self) -> &str {
        "notion.page_update"
    }
    fn description(&self) -> &str {
        "MUTATING: update page properties. Include only properties to change."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "description": "Page ID" },
                "properties": { "type": "object", "description": "Properties to update" },
                "icon": { "type": "object", "description": "New icon (or null to remove)" },
                "cover": { "type": "object", "description": "New cover (or null to remove)" },
                "workspace": self.ctx.workspace_prop(),
            },
            "required": ["id"]
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        run(&self.ctx, input, |client, input| async move {
            let id = coerce_id(&input["id"])?;
            let mut body = json!({});
            if let Some(props) = input.get("properties").filter(|v| v.is_object()) {
                body["properties"] = props.clone();
            }
            if let Some(icon) = input.get("icon") {
                body["icon"] = icon.clone();
            }
            if let Some(cover) = input.get("cover") {
                body["cover"] = cover.clone();
            }
            let result = client.patch(&format!("/pages/{id}"), body).await?;
            info!(page_id = %id, "Updated Notion page");
            Ok(result)
        })
        .await
    }
}

// ── notion.page_archive ─────────────────────────────────────────────

notion_tool!(PageArchiveTool);

#[async_trait]
impl Tool for PageArchiveTool {
    fn name(&self) -> &str {
        "notion.page_archive"
    }
    fn description(&self) -> &str {
        "MUTATING: archive or restore a page."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "description": "Page ID" },
                "archived": { "type": "boolean", "description": "true to archive, false to restore (default: true)" },
                "workspace": self.ctx.workspace_prop(),
            },
            "required": ["id"]
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        run(&self.ctx, input, |client, input| async move {
            let id = coerce_id(&input["id"])?;
            let archived = input["archived"].as_bool().unwrap_or(true);
            let body = json!({ "archived": archived });
            let result = client.patch(&format!("/pages/{id}"), body).await?;
            info!(page_id = %id, archived, "Archived/restored Notion page");
            Ok(result)
        })
        .await
    }
}

// ── notion.database_get ─────────────────────────────────────────────

notion_tool!(DatabaseGetTool);

#[async_trait]
impl Tool for DatabaseGetTool {
    fn name(&self) -> &str {
        "notion.database_get"
    }
    fn description(&self) -> &str {
        "Get a database schema (properties, title) by ID."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "description": "Database ID (UUID)" },
                "workspace": self.ctx.workspace_prop(),
            },
            "required": ["id"]
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        run(&self.ctx, input, |client, input| async move {
            let id = coerce_id(&input["id"])?;
            let db = client.get(&format!("/databases/{id}"), &[]).await?;
            Ok(wrap_rich_text(db, "notion_database"))
        })
        .await
    }
}

// ── notion.database_query ───────────────────────────────────────────

notion_tool!(DatabaseQueryTool);

#[async_trait]
impl Tool for DatabaseQueryTool {
    fn name(&self) -> &str {
        "notion.database_query"
    }
    fn description(&self) -> &str {
        "Query a database with optional filters and sorts. Returns pages matching the criteria."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "description": "Database ID" },
                "filter": { "type": "object", "description": "Notion filter object" },
                "sorts": { "type": "array", "description": "Sort criteria", "items": { "type": "object" } },
                "page_size": { "type": "integer", "description": "Max results (default/max 100)", "minimum": 1, "maximum": 100 },
                "start_cursor": { "type": "string", "description": "Pagination cursor" },
                "workspace": self.ctx.workspace_prop(),
            },
            "required": ["id"]
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        run(&self.ctx, input, |client, input| async move {
            let id = coerce_id(&input["id"])?;
            let page_size = clamp_limit(coerce_to_u64(&input["page_size"]), client.max_results);
            let mut body = json!({ "page_size": page_size });
            if let Some(f) = input.get("filter").filter(|v| v.is_object()) {
                body["filter"] = f.clone();
            }
            if let Some(sorts) = input.get("sorts").filter(|v| v.is_array()) {
                body["sorts"] = sorts.clone();
            }
            if let Some(cursor) = input["start_cursor"].as_str() {
                body["start_cursor"] = json!(cursor);
            }
            let resp = client.post(&format!("/databases/{id}/query"), body).await?;
            let results = resp["results"].as_array().cloned().unwrap_or_default();
            let wrapped = wrap_results(results, "notion_database_query");
            let paging = extract_paging(&resp);
            info!(count = wrapped.len(), database_id = %id, "Queried Notion database");
            Ok(json!({ "count": wrapped.len(), "results": wrapped, "paging": paging }))
        })
        .await
    }
}

// ── notion.database_create ──────────────────────────────────────────

notion_tool!(DatabaseCreateTool);

#[async_trait]
impl Tool for DatabaseCreateTool {
    fn name(&self) -> &str {
        "notion.database_create"
    }
    fn description(&self) -> &str {
        "MUTATING: create an inline database as a child of an existing page."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "parent": {
                    "type": "object",
                    "description": "Parent page: {\"page_id\": \"...\"}"
                },
                "title": {
                    "type": "array",
                    "description": "Database title as rich text: [{\"text\": {\"content\": \"...\"}}]",
                    "items": { "type": "object" }
                },
                "properties": {
                    "type": "object",
                    "description": "Database property schema (column definitions)"
                },
                "workspace": self.ctx.workspace_prop(),
            },
            "required": ["parent", "title", "properties"]
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        run(&self.ctx, input, |client, input| async move {
            let parent = input
                .get("parent")
                .filter(|v| v.is_object())
                .cloned()
                .ok_or_else(|| eyre::eyre!("Missing 'parent' object"))?;
            let title = input
                .get("title")
                .filter(|v| v.is_array())
                .cloned()
                .ok_or_else(|| eyre::eyre!("Missing 'title' array"))?;
            let properties = input
                .get("properties")
                .filter(|v| v.is_object())
                .cloned()
                .ok_or_else(|| eyre::eyre!("Missing 'properties' object"))?;
            let body = json!({ "parent": parent, "title": title, "properties": properties });
            let result = client.post("/databases", body).await?;
            info!("Created Notion database");
            Ok(result)
        })
        .await
    }
}

// ── notion.block_get ────────────────────────────────────────────────

notion_tool!(BlockGetTool);

#[async_trait]
impl Tool for BlockGetTool {
    fn name(&self) -> &str {
        "notion.block_get"
    }
    fn description(&self) -> &str {
        "Get a single block by ID."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "description": "Block ID (UUID)" },
                "workspace": self.ctx.workspace_prop(),
            },
            "required": ["id"]
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        run(&self.ctx, input, |client, input| async move {
            let id = coerce_id(&input["id"])?;
            let block = client.get(&format!("/blocks/{id}"), &[]).await?;
            Ok(wrap_rich_text(block, "notion_block"))
        })
        .await
    }
}

// ── notion.block_children ───────────────────────────────────────────

notion_tool!(BlockChildrenTool);

#[async_trait]
impl Tool for BlockChildrenTool {
    fn name(&self) -> &str {
        "notion.block_children"
    }
    fn description(&self) -> &str {
        "Get child blocks of a block or page. Use this to read page content."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "description": "Block or page ID" },
                "page_size": { "type": "integer", "description": "Max blocks (default/max 100)", "minimum": 1, "maximum": 100 },
                "start_cursor": { "type": "string", "description": "Pagination cursor" },
                "workspace": self.ctx.workspace_prop(),
            },
            "required": ["id"]
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        run(&self.ctx, input, |client, input| async move {
            let id = coerce_id(&input["id"])?;
            let page_size = clamp_limit(coerce_to_u64(&input["page_size"]), client.max_results);
            let mut query = vec![("page_size", page_size.to_string())];
            if let Some(cursor) = input["start_cursor"].as_str() {
                query.push(("start_cursor", cursor.to_string()));
            }
            let resp = client
                .get(&format!("/blocks/{id}/children"), &query)
                .await?;
            let results = resp["results"].as_array().cloned().unwrap_or_default();
            let wrapped = wrap_results(results, "notion_block");
            let paging = extract_paging(&resp);
            info!(count = wrapped.len(), block_id = %id, "Listed Notion block children");
            Ok(json!({ "count": wrapped.len(), "blocks": wrapped, "paging": paging }))
        })
        .await
    }
}

// ── notion.block_append ─────────────────────────────────────────────

notion_tool!(BlockAppendTool);

#[async_trait]
impl Tool for BlockAppendTool {
    fn name(&self) -> &str {
        "notion.block_append"
    }
    fn description(&self) -> &str {
        "MUTATING: append child blocks to a block or page. Use this to add content."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "description": "Block or page ID to append to" },
                "children": {
                    "type": "array",
                    "description": "Block objects to append (max 100)",
                    "items": { "type": "object" }
                },
                "workspace": self.ctx.workspace_prop(),
            },
            "required": ["id", "children"]
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        run(&self.ctx, input, |client, input| async move {
            let id = coerce_id(&input["id"])?;
            let children = input
                .get("children")
                .filter(|v| v.is_array())
                .cloned()
                .ok_or_else(|| eyre::eyre!("Missing 'children' array"))?;
            let body = json!({ "children": children });
            let result = client
                .patch(&format!("/blocks/{id}/children"), body)
                .await?;
            info!(block_id = %id, "Appended blocks to Notion");
            Ok(result)
        })
        .await
    }
}

// ── notion.block_update ─────────────────────────────────────────────

notion_tool!(BlockUpdateTool);

#[async_trait]
impl Tool for BlockUpdateTool {
    fn name(&self) -> &str {
        "notion.block_update"
    }
    fn description(&self) -> &str {
        "MUTATING: update a block. The payload must include the block type key with new content."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "description": "Block ID" },
                "block": {
                    "type": "object",
                    "description": "Block update payload (e.g. {\"paragraph\": {\"rich_text\": [...]}})"
                },
                "workspace": self.ctx.workspace_prop(),
            },
            "required": ["id", "block"]
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        run(&self.ctx, input, |client, input| async move {
            let id = coerce_id(&input["id"])?;
            let block = input
                .get("block")
                .filter(|v| v.is_object())
                .cloned()
                .ok_or_else(|| eyre::eyre!("Missing 'block' object"))?;
            let result = client.patch(&format!("/blocks/{id}"), block).await?;
            info!(block_id = %id, "Updated Notion block");
            Ok(result)
        })
        .await
    }
}

// ── notion.block_delete ─────────────────────────────────────────────

notion_tool!(BlockDeleteTool);

#[async_trait]
impl Tool for BlockDeleteTool {
    fn name(&self) -> &str {
        "notion.block_delete"
    }
    fn description(&self) -> &str {
        "MUTATING: delete (archive) a block."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "description": "Block ID" },
                "workspace": self.ctx.workspace_prop(),
            },
            "required": ["id"]
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        run(&self.ctx, input, |client, input| async move {
            let id = coerce_id(&input["id"])?;
            let result = client.delete(&format!("/blocks/{id}")).await?;
            info!(block_id = %id, "Deleted Notion block");
            Ok(result)
        })
        .await
    }
}

// ── notion.user_list ────────────────────────────────────────────────

notion_tool!(UserListTool);

#[async_trait]
impl Tool for UserListTool {
    fn name(&self) -> &str {
        "notion.user_list"
    }
    fn description(&self) -> &str {
        "List all users in the workspace."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "page_size": { "type": "integer", "description": "Max users (default/max 100)", "minimum": 1, "maximum": 100 },
                "start_cursor": { "type": "string", "description": "Pagination cursor" },
                "workspace": self.ctx.workspace_prop(),
            }
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        run(&self.ctx, input, |client, input| async move {
            let page_size = clamp_limit(coerce_to_u64(&input["page_size"]), client.max_results);
            let mut query = vec![("page_size", page_size.to_string())];
            if let Some(cursor) = input["start_cursor"].as_str() {
                query.push(("start_cursor", cursor.to_string()));
            }
            let resp = client.get("/users", &query).await?;
            let results = resp["results"].as_array().cloned().unwrap_or_default();
            let paging = extract_paging(&resp);
            info!(count = results.len(), "Listed Notion users");
            Ok(json!({ "count": results.len(), "users": results, "paging": paging }))
        })
        .await
    }
}

// ── notion.user_get ─────────────────────────────────────────────────

notion_tool!(UserGetTool);

#[async_trait]
impl Tool for UserGetTool {
    fn name(&self) -> &str {
        "notion.user_get"
    }
    fn description(&self) -> &str {
        "Get a user by ID."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "description": "User ID (UUID) or \"me\" for the bot user" },
                "workspace": self.ctx.workspace_prop(),
            },
            "required": ["id"]
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        run(&self.ctx, input, |client, input| async move {
            let id = coerce_id(&input["id"])?;
            let user = client.get(&format!("/users/{id}"), &[]).await?;
            Ok(user)
        })
        .await
    }
}

// ── notion.comment_list ─────────────────────────────────────────────

notion_tool!(CommentListTool);

#[async_trait]
impl Tool for CommentListTool {
    fn name(&self) -> &str {
        "notion.comment_list"
    }
    fn description(&self) -> &str {
        "List comments on a page or block."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "block_id": { "type": "string", "description": "Page or block ID to list comments for" },
                "page_size": { "type": "integer", "description": "Max comments (default/max 100)", "minimum": 1, "maximum": 100 },
                "start_cursor": { "type": "string", "description": "Pagination cursor" },
                "workspace": self.ctx.workspace_prop(),
            },
            "required": ["block_id"]
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        run(&self.ctx, input, |client, input| async move {
            let block_id = coerce_id(&input["block_id"])?;
            let page_size = clamp_limit(coerce_to_u64(&input["page_size"]), client.max_results);
            let mut query = vec![
                ("block_id", block_id.clone()),
                ("page_size", page_size.to_string()),
            ];
            if let Some(cursor) = input["start_cursor"].as_str() {
                query.push(("start_cursor", cursor.to_string()));
            }
            let resp = client.get("/comments", &query).await?;
            let results = resp["results"].as_array().cloned().unwrap_or_default();
            let wrapped = wrap_results(results, "notion_comment");
            let paging = extract_paging(&resp);
            info!(count = wrapped.len(), "Listed Notion comments");
            Ok(json!({ "count": wrapped.len(), "comments": wrapped, "paging": paging }))
        })
        .await
    }
}

// ── notion.comment_create ───────────────────────────────────────────

notion_tool!(CommentCreateTool);

#[async_trait]
impl Tool for CommentCreateTool {
    fn name(&self) -> &str {
        "notion.comment_create"
    }
    fn description(&self) -> &str {
        "MUTATING: create a comment on a page or in an existing discussion thread."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "parent": {
                    "type": "object",
                    "description": "Parent page: {\"page_id\": \"...\"}"
                },
                "discussion_id": {
                    "type": "string",
                    "description": "Existing discussion thread ID (alternative to parent)"
                },
                "rich_text": {
                    "type": "array",
                    "description": "Comment body as rich text: [{\"text\": {\"content\": \"...\"}}]",
                    "items": { "type": "object" }
                },
                "workspace": self.ctx.workspace_prop(),
            },
            "required": ["rich_text"]
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        run(&self.ctx, input, |client, input| async move {
            let rich_text = input
                .get("rich_text")
                .filter(|v| v.is_array())
                .cloned()
                .ok_or_else(|| eyre::eyre!("Missing 'rich_text' array"))?;
            let mut body = json!({ "rich_text": rich_text });
            if let Some(parent) = input.get("parent").filter(|v| v.is_object()) {
                body["parent"] = parent.clone();
            }
            if let Some(disc_id) = input["discussion_id"].as_str() {
                body["discussion_id"] = json!(disc_id);
            }
            let result = client.post("/comments", body).await?;
            info!("Created Notion comment");
            Ok(result)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_description_single() {
        let mut clients = HashMap::new();
        clients.insert(
            "default".to_string(),
            Arc::new(NotionClient::new("ntn_test", 100).unwrap()),
        );
        let desc = workspace_description(&clients, "default");
        assert!(desc.contains("multi-workspace"));
    }

    #[test]
    fn workspace_description_multi() {
        let mut clients = HashMap::new();
        clients.insert(
            "prod".to_string(),
            Arc::new(NotionClient::new("ntn_prod", 100).unwrap()),
        );
        clients.insert(
            "dev".to_string(),
            Arc::new(NotionClient::new("ntn_dev", 100).unwrap()),
        );
        let desc = workspace_description(&clients, "prod");
        assert!(desc.contains("prod"));
        assert!(desc.contains("dev"));
        assert!(desc.contains("Default: prod"));
        assert!(workspace_description(&clients, "gone").contains("Default: none — pass workspace"));
    }

    #[test]
    fn resolve_client_explicit_unknown() {
        let mut clients = HashMap::new();
        clients.insert(
            "prod".to_string(),
            Arc::new(NotionClient::new("ntn_prod", 100).unwrap()),
        );
        let err = resolve_client(&clients, "prod", &json!({"workspace": "nope"}))
            .err()
            .unwrap();
        assert!(err.to_string().contains("Unknown Notion workspace"));
    }

    #[test]
    fn wrap_rich_text_wraps_plain_text_fields() {
        let obj = json!({
            "type": "paragraph",
            "paragraph": {
                "rich_text": [
                    { "type": "text", "plain_text": "Hello world", "text": { "content": "Hello world" } },
                    { "type": "text", "plain_text": "", "text": { "content": "" } }
                ]
            }
        });
        let wrapped = wrap_rich_text(obj, "test");
        let rt = &wrapped["paragraph"]["rich_text"];
        assert_ne!(rt[0]["plain_text"].as_str().unwrap(), "Hello world");
        assert_eq!(rt[1]["plain_text"].as_str().unwrap(), "");
    }

    #[test]
    fn extract_paging_with_more() {
        let resp = json!({ "has_more": true, "next_cursor": "abc123" });
        let p = extract_paging(&resp);
        assert_eq!(p["has_more"], true);
        assert_eq!(p["next_cursor"], "abc123");
    }

    #[test]
    fn extract_paging_without_more() {
        let resp = json!({ "has_more": false });
        let p = extract_paging(&resp);
        assert_eq!(p["has_more"], false);
    }

    #[test]
    fn coerce_id_works() {
        assert_eq!(coerce_id(&json!("abc-123")).unwrap(), "abc-123");
        assert!(coerce_id(&json!(null)).is_err());
    }
}
