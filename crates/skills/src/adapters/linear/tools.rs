//! Linear agent-callable tools.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use eyre::Result;
use serde_json::Value;
use tracing::{info, warn};

use workforce_agent::security::external_content::wrap_external_content;
use workforce_agent::tools::{Tool, ToolResult};

use super::client::LinearClient;

use crate::adapters::utils::{coerce_to_i64, coerce_to_u64, instance_choices};

/// A description that arrives as one line with its newlines written out
/// (`\n` as two characters, as a model sometimes sends them) is turned
/// into real line breaks; text with real ones is left alone.
fn unescape_newlines(text: &str) -> String {
    if text.contains('\n') || !text.contains("\\n") {
        return text.to_string();
    }
    text.replace("\\r\\n", "\n").replace("\\n", "\n")
}

/// Wrap the `description` field of a Linear issue/comment JSON value with security markers.
fn wrap_description_field(value: &Value, source: &str) -> Value {
    let mut v = value.clone();
    if let Some(obj) = v.as_object_mut() {
        if let Some(desc) = obj.get("description").and_then(|d| d.as_str()) {
            if !desc.is_empty() {
                let wrapped = wrap_external_content(desc, source).wrapped_content;
                obj.insert("description".to_string(), Value::String(wrapped));
            }
        }
    }
    v
}

/// Wrap the `body` field of a Linear comment JSON value with security markers.
fn wrap_body_field(value: &Value, source: &str) -> Value {
    let mut v = value.clone();
    if let Some(obj) = v.as_object_mut() {
        if let Some(body) = obj.get("body").and_then(|b| b.as_str()) {
            if !body.is_empty() {
                let wrapped = wrap_external_content(body, source).wrapped_content;
                obj.insert("body".to_string(), Value::String(wrapped));
            }
        }
    }
    v
}

/// Resolve the team_id: use the explicit param, fall back to default_team_id, or error.
fn resolve_team_id<'a>(
    input: &'a Value,
    default_team_id: &'a Option<String>,
) -> std::result::Result<&'a str, String> {
    input["team_id"]
        .as_str()
        .or(default_team_id.as_deref())
        .ok_or_else(|| "Missing 'team_id' parameter and no default_team_id configured".to_string())
}

/// Resolve the Linear client for a given input.
///
/// Priority: explicit `workspace` param → ticket prefix auto-detection → default workspace.
fn resolve_client(
    clients: &HashMap<String, Arc<LinearClient>>,
    default_workspace: &str,
    prefix_map: &HashMap<String, String>,
    input: &Value,
) -> eyre::Result<Arc<LinearClient>> {
    let requested = input["workspace"]
        .as_str()
        .map(str::to_string)
        .or_else(|| routed_workspace(clients, prefix_map, input));
    let name = workforce_agent::llm::environment::resolve_instance(
        "linear",
        requested.as_deref(),
        default_workspace,
        clients,
    )
    .map_err(|d| eyre::eyre!("{}", d.message))?;
    clients.get(&name).cloned().ok_or_else(|| {
        eyre::eyre!(
            "Unknown Linear workspace: '{name}'. Available: {:?}",
            clients.keys().collect::<Vec<_>>()
        )
    })
}

/// Workspace auto-routed from a ticket identifier prefix (`identifier`,
/// `related_identifier`, or the first of `identifiers`, e.g. "STA" from
/// "OPS-243"), only when the mapped workspace is configured — else `None`.
fn routed_workspace(
    clients: &HashMap<String, Arc<LinearClient>>,
    prefix_map: &HashMap<String, String>,
    input: &Value,
) -> Option<String> {
    if prefix_map.is_empty() {
        return None;
    }
    let from_id = |id: &str| {
        id.split('-')
            .next()
            .and_then(|prefix| prefix_map.get(prefix))
            .filter(|name| clients.contains_key(*name))
            .cloned()
    };
    for field in ["identifier", "related_identifier"] {
        if let Some(name) = input[field].as_str().and_then(from_id) {
            return Some(name);
        }
    }
    input["identifiers"]
        .as_array()
        .and_then(|ids| ids.first())
        .and_then(|v| v.as_str())
        .and_then(from_id)
}

/// Build a dynamic description for the `workspace` parameter that includes available names.
fn workspace_description(
    clients: &HashMap<String, Arc<LinearClient>>,
    default_workspace: &str,
) -> String {
    if clients.len() <= 1 {
        return "Workspace name (for multi-workspace setups)".to_string();
    }
    format!(
        "Workspace name. {}",
        instance_choices(clients, default_workspace, "workspace")
    )
}

// ── linear.issue_list ────────────────────────────────────────────────

pub struct LinearIssueListTool {
    clients: Arc<HashMap<String, Arc<LinearClient>>>,
    default_workspace: String,
    default_team_id: Option<String>,
    prefix_map: Arc<HashMap<String, String>>,
}

impl LinearIssueListTool {
    pub fn new(
        clients: Arc<HashMap<String, Arc<LinearClient>>>,
        default_workspace: String,
        default_team_id: Option<String>,
        prefix_map: Arc<HashMap<String, String>>,
    ) -> Self {
        Self {
            clients,
            default_workspace,
            default_team_id,
            prefix_map,
        }
    }
}

#[async_trait]
impl Tool for LinearIssueListTool {
    fn name(&self) -> &str {
        "linear.issue_list"
    }

    fn description(&self) -> &str {
        "List issues with optional filters (state, priority, team, assignee, project,"
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "team_id": {
                    "type": "string",
                    "description": "Team UUID string to filter by"
                },
                "state": {
                    "type": "string",
                    "description": "Filter by state name (e.g., 'In Progress', 'Done')"
                },
                "priority": {
                    "type": "integer",
                    "description": "Filter by priority (integer)",
                    "enum": [0, 1, 2, 3, 4]
                },
                "assignee_id": {
                    "type": "string",
                    "description": "Filter by assignee user UUID string"
                },
                "project_id": {
                    "type": "string",
                    "description": "Filter by project UUID string"
                },
                "parent_id": {
                    "type": "string",
                    "description": "Filter by parent issue identifier string (e.g., 'DW-289')"
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum number of issues to return (integer, default",
                    "minimum": 1,
                    "maximum": 100
                },
                "after": {
                    "type": "string",
                    "description": "The next page: page_info.end_cursor from the page before"
                },
                "updated_since": {
                    "type": "string",
                    "description": "Only issues updated at or after this ISO 8601 time"
                },
                "open_only": {
                    "type": "boolean",
                    "description": "Leave out completed and canceled issues"
                },
                "completed_since": {
                    "type": "string",
                    "description": "With open_only: also issues completed at or after this ISO 8601 time"
                },
                "workspace": {
                    "type": "string",
                    "description": workspace_description(&self.clients, &self.default_workspace)
                }
            },
            "required": []
        })
    }

    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();

        let client = match resolve_client(
            &self.clients,
            &self.default_workspace,
            &self.prefix_map,
            &input,
        ) {
            Ok(c) => c,
            Err(e) => {
                return Ok(ToolResult::failure(
                    format!("{e}"),
                    start.elapsed().as_millis() as u64,
                ))
            }
        };

        let limit = coerce_to_u64(&input["limit"]).unwrap_or(50).min(100);

        // Build filter conditions
        let mut filters = Vec::new();

        if let Some(team_id) = input["team_id"]
            .as_str()
            .or(self.default_team_id.as_deref())
        {
            filters.push(format!(r#"team: {{ id: {{ eq: "{}" }} }}"#, team_id));
        }

        if let Some(state) = input["state"].as_str() {
            filters.push(format!(r#"state: {{ name: {{ eq: "{}" }} }}"#, state));
        }

        if let Some(priority) = coerce_to_i64(&input["priority"]) {
            filters.push(format!("priority: {{ eq: {} }}", priority));
        }

        if let Some(assignee_id) = input["assignee_id"].as_str() {
            filters.push(format!(
                r#"assignee: {{ id: {{ eq: "{}" }} }}"#,
                assignee_id
            ));
        }

        if let Some(project_id) = input["project_id"].as_str() {
            filters.push(format!(r#"project: {{ id: {{ eq: "{}" }} }}"#, project_id));
        }
        if let Some(parent_id) = input["parent_id"].as_str() {
            filters.push(format!(r#"parent: {{ id: {{ eq: "{}" }} }}"#, parent_id));
        }
        // Times and the cursor go in as variables, never into the query.
        let mut variables = serde_json::Map::new();
        let mut params = vec![];
        if let Some(since) = input["updated_since"].as_str().filter(|s| !s.is_empty()) {
            params.push("$updatedSince: DateTimeOrDuration");
            variables.insert("updatedSince".into(), serde_json::json!(since));
            filters.push("updatedAt: { gte: $updatedSince }".to_string());
        }
        if input["open_only"].as_bool() == Some(true) {
            let open = r#"state: { type: { nin: ["completed", "canceled"] } }"#;
            match input["completed_since"].as_str().filter(|s| !s.is_empty()) {
                Some(done) => {
                    params.push("$completedSince: DateTimeOrDuration");
                    variables.insert("completedSince".into(), serde_json::json!(done));
                    filters.push(format!(
                        "or: [{{ {open} }}, {{ completedAt: {{ gte: $completedSince }} }}]"
                    ));
                }
                None => filters.push(open.to_string()),
            }
        }
        let mut page = String::new();
        if let Some(after) = input["after"].as_str().filter(|s| !s.is_empty()) {
            params.push("$after: String");
            variables.insert("after".into(), serde_json::json!(after));
            page = ", after: $after".to_string();
        }

        let filter_clause = if filters.is_empty() {
            String::new()
        } else {
            format!("filter: {{ {} }}", filters.join(", "))
        };
        let params = match params.is_empty() {
            true => String::new(),
            false => format!("({})", params.join(", ")),
        };

        let query = format!(
            r#"
            query{} {{
                issues(first: {}{}, {}) {{
                    pageInfo {{
                        hasNextPage
                        endCursor
                    }}
                    nodes {{
                        id
                        identifier
                        title
                        description
                        priority
                        priorityLabel
                        state {{
                            id
                            name
                            type
                            color
                        }}
                        assignee {{
                            id
                            name
                            email
                        }}
                        team {{
                            id
                            name
                            key
                        }}
                        labels {{
                            nodes {{
                                id
                                name
                                color
                            }}
                        }}
                        project {{
                            id
                            name
                        }}
                        parent {{
                            id
                            identifier
                            title
                        }}
                        dueDate
                        attachments(first: 25) {{
                            nodes {{
                                id
                                title
                                subtitle
                                url
                                sourceType
                                createdAt
                            }}
                        }}
                        createdAt
                        updatedAt
                        url
                    }}
                }}
            }}
            "#,
            params, limit, page, filter_clause
        );

        let duration_ms = start.elapsed().as_millis() as u64;

        match client.query(&query, Value::Object(variables)).await {
            Ok(data) => {
                let issues = data
                    .get("issues")
                    .and_then(|i| i.get("nodes"))
                    .cloned()
                    .unwrap_or(serde_json::json!([]));
                let count = issues.as_array().map(|a| a.len()).unwrap_or(0);
                let info = &data["issues"]["pageInfo"];
                let page_info = serde_json::json!({
                    "has_next_page": info["hasNextPage"].as_bool().unwrap_or(false),
                    "end_cursor": info["endCursor"],
                });
                info!(count, "Listed Linear issues");
                Ok(ToolResult::success(
                    serde_json::json!({ "count": count, "issues": issues, "page_info": page_info }),
                    duration_ms,
                ))
            }
            Err(e) => {
                warn!(error = %e, "Failed to list issues");
                Ok(ToolResult::from_report(
                    &e.wrap_err("Failed to list issues"),
                    duration_ms,
                ))
            }
        }
    }
}

// ── linear.issue_get ────────────────────────────────────────────────

pub struct LinearIssueGetTool {
    clients: Arc<HashMap<String, Arc<LinearClient>>>,
    default_workspace: String,
    prefix_map: Arc<HashMap<String, String>>,
}

impl LinearIssueGetTool {
    pub fn new(
        clients: Arc<HashMap<String, Arc<LinearClient>>>,
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

#[async_trait]
impl Tool for LinearIssueGetTool {
    fn name(&self) -> &str {
        "linear.issue_get"
    }

    fn description(&self) -> &str {
        "Get a specific issue by its identifier (e.g., 'DW-107')."
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "identifier": {
                    "type": "string",
                    "description": "Issue identifier string like 'DW-107'"
                },
                "workspace": {
                    "type": "string",
                    "description": workspace_description(&self.clients, &self.default_workspace)
                }
            },
            "required": ["identifier"]
        })
    }

    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();

        let client = match resolve_client(
            &self.clients,
            &self.default_workspace,
            &self.prefix_map,
            &input,
        ) {
            Ok(c) => c,
            Err(e) => {
                return Ok(ToolResult::failure(
                    format!("{e}"),
                    start.elapsed().as_millis() as u64,
                ))
            }
        };

        let identifier = match input["identifier"].as_str() {
            Some(id) => id,
            None => {
                return Ok(ToolResult::failure(
                    "Missing required 'identifier' parameter",
                    start.elapsed().as_millis() as u64,
                ))
            }
        };

        let query = r#"
            query($id: String!) {
                issue(id: $id) {
                    id
                    identifier
                    title
                    description
                    priority
                    priorityLabel
                    state {
                        id
                        name
                        type
                        color
                    }
                    assignee {
                        id
                        name
                        email
                    }
                    team {
                        id
                        name
                        key
                    }
                    project {
                        id
                        name
                        description
                    }
                    parent {
                        id
                        identifier
                        title
                    }
                    children(first: 50) {
                        nodes {
                            id
                            identifier
                            title
                            state {
                                name
                            }
                            priority
                        }
                    }
                    labels {
                        nodes {
                            id
                            name
                            color
                        }
                    }
                    dueDate
                    attachments(first: 25) {
                        nodes {
                            id
                            title
                            subtitle
                            url
                            sourceType
                            createdAt
                        }
                    }
                    createdAt
                    updatedAt
                    url
                }
            }
        "#;

        let variables = serde_json::json!({ "id": identifier });
        let duration_ms = start.elapsed().as_millis() as u64;

        match client.query(query, variables).await {
            Ok(data) => {
                if let Some(issue) = data.get("issue") {
                    if issue.is_null() {
                        return Ok(ToolResult::failure(
                            format!("Issue '{}' not found", identifier),
                            duration_ms,
                        ));
                    }
                    info!(identifier, "Retrieved Linear issue");
                    let wrapped_issue = wrap_description_field(issue, "Linear issue description");
                    Ok(ToolResult::success(
                        serde_json::json!({ "issue": wrapped_issue }),
                        duration_ms,
                    ))
                } else {
                    Ok(ToolResult::failure(
                        format!("Issue '{}' not found", identifier),
                        duration_ms,
                    ))
                }
            }
            Err(e) => {
                warn!(error = %e, identifier, "Failed to get issue");
                Ok(ToolResult::from_report(
                    &e.wrap_err("Failed to get issue"),
                    duration_ms,
                ))
            }
        }
    }
}

// ── linear.issue_search ────────────────────────────────────────────────

pub struct LinearIssueSearchTool {
    clients: Arc<HashMap<String, Arc<LinearClient>>>,
    default_workspace: String,
    prefix_map: Arc<HashMap<String, String>>,
}

impl LinearIssueSearchTool {
    pub fn new(
        clients: Arc<HashMap<String, Arc<LinearClient>>>,
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

#[async_trait]
impl Tool for LinearIssueSearchTool {
    fn name(&self) -> &str {
        "linear.issue_search"
    }

    fn description(&self) -> &str {
        "Full-text search for issues."
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Search query string"
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum number of results (integer, default: 25, max: 100)",
                    "minimum": 1,
                    "maximum": 100
                },
                "workspace": {
                    "type": "string",
                    "description": workspace_description(&self.clients, &self.default_workspace)
                }
            },
            "required": ["query"]
        })
    }

    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();

        let client = match resolve_client(
            &self.clients,
            &self.default_workspace,
            &self.prefix_map,
            &input,
        ) {
            Ok(c) => c,
            Err(e) => {
                return Ok(ToolResult::failure(
                    format!("{e}"),
                    start.elapsed().as_millis() as u64,
                ))
            }
        };

        let search_query = match input["query"].as_str() {
            Some(q) => q,
            None => {
                return Ok(ToolResult::failure(
                    "Missing required 'query' parameter",
                    start.elapsed().as_millis() as u64,
                ))
            }
        };

        let limit = coerce_to_u64(&input["limit"]).unwrap_or(25).min(100);

        let query = r#"
            query($term: String!, $first: Int!) {
                searchIssues(term: $term, first: $first) {
                    nodes {
                        id
                        identifier
                        title
                        description
                        priority
                        priorityLabel
                        state {
                            id
                            name
                            type
                        }
                        assignee {
                            id
                            name
                        }
                        team {
                            id
                            name
                            key
                        }
                        url
                    }
                }
            }
        "#;

        let variables = serde_json::json!({
            "term": search_query,
            "first": limit
        });

        let duration_ms = start.elapsed().as_millis() as u64;

        match client.query(query, variables).await {
            Ok(data) => {
                let issues = data
                    .get("searchIssues")
                    .and_then(|i| i.get("nodes"))
                    .cloned()
                    .unwrap_or(serde_json::json!([]));
                let count = issues.as_array().map(|a| a.len()).unwrap_or(0);
                // Wrap description fields in search results
                let wrapped_issues: Vec<Value> = issues
                    .as_array()
                    .map(|arr| {
                        arr.iter()
                            .map(|i| wrap_description_field(i, "Linear issue description"))
                            .collect()
                    })
                    .unwrap_or_default();
                info!(query = %search_query, count, "Searched Linear issues");
                Ok(ToolResult::success(
                    serde_json::json!({ "query": search_query, "count": count, "issues": wrapped_issues }),
                    duration_ms,
                ))
            }
            Err(e) => {
                warn!(error = %e, query = %search_query, "Failed to search issues");
                Ok(ToolResult::from_report(
                    &e.wrap_err("Failed to search issues"),
                    duration_ms,
                ))
            }
        }
    }
}

// ── linear.issue_create ────────────────────────────────────────────────

pub struct LinearIssueCreateTool {
    clients: Arc<HashMap<String, Arc<LinearClient>>>,
    default_workspace: String,
    default_team_id: Option<String>,
    prefix_map: Arc<HashMap<String, String>>,
}

impl LinearIssueCreateTool {
    pub fn new(
        clients: Arc<HashMap<String, Arc<LinearClient>>>,
        default_workspace: String,
        default_team_id: Option<String>,
        prefix_map: Arc<HashMap<String, String>>,
    ) -> Self {
        Self {
            clients,
            default_workspace,
            default_team_id,
            prefix_map,
        }
    }
}

#[async_trait]
impl Tool for LinearIssueCreateTool {
    fn name(&self) -> &str {
        "linear.issue_create"
    }

    fn description(&self) -> &str {
        "Create a new issue."
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "title": {
                    "type": "string",
                    "description": "Issue title"
                },
                "description": {
                    "type": "string",
                    "description": "Issue description (markdown supported)"
                },
                "team_id": {
                    "type": "string",
                    "description": "Team UUID string. Uses default_team_id if omitted."
                },
                "state_id": {
                    "type": "string",
                    "description": "Workflow state UUID string"
                },
                "priority": {
                    "type": "integer",
                    "description": "Priority (integer)",
                    "enum": [0, 1, 2, 3, 4]
                },
                "assignee_id": {
                    "type": "string",
                    "description": "User UUID string to assign the issue to"
                },
                "project_id": {
                    "type": "string",
                    "description": "Project UUID string to add the issue to"
                },
                "label_ids": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Array of label UUID strings to apply"
                },
                "workspace": {
                    "type": "string",
                    "description": workspace_description(&self.clients, &self.default_workspace)
                }
            },
            "required": ["title"]
        })
    }

    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();

        let client = match resolve_client(
            &self.clients,
            &self.default_workspace,
            &self.prefix_map,
            &input,
        ) {
            Ok(c) => c,
            Err(e) => {
                return Ok(ToolResult::failure(
                    format!("{e}"),
                    start.elapsed().as_millis() as u64,
                ))
            }
        };

        let title = match input["title"].as_str() {
            Some(t) => t,
            None => {
                return Ok(ToolResult::failure(
                    "Missing required 'title' parameter",
                    start.elapsed().as_millis() as u64,
                ))
            }
        };

        let team_id = match resolve_team_id(&input, &self.default_team_id) {
            Ok(t) => t,
            Err(e) => return Ok(ToolResult::failure(e, start.elapsed().as_millis() as u64)),
        };

        // Build the input object for the mutation
        let mut issue_input = serde_json::json!({
            "title": title,
            "teamId": team_id
        });

        if let Some(desc) = input["description"].as_str() {
            issue_input["description"] = serde_json::json!(unescape_newlines(desc));
        }
        if let Some(state_id) = input["state_id"].as_str() {
            issue_input["stateId"] = serde_json::json!(state_id);
        }
        if let Some(priority) = coerce_to_i64(&input["priority"]) {
            issue_input["priority"] = serde_json::json!(priority);
        }
        if let Some(assignee_id) = input["assignee_id"].as_str() {
            issue_input["assigneeId"] = serde_json::json!(assignee_id);
        }
        if let Some(project_id) = input["project_id"].as_str() {
            issue_input["projectId"] = serde_json::json!(project_id);
        }
        if let Some(label_ids) = input["label_ids"].as_array() {
            issue_input["labelIds"] = serde_json::json!(label_ids);
        }

        let mutation = r#"
            mutation($input: IssueCreateInput!) {
                issueCreate(input: $input) {
                    success
                    issue {
                        id
                        identifier
                        title
                        description
                        priority
                        state {
                            id
                            name
                        }
                        team {
                            id
                            name
                            key
                        }
                        url
                    }
                }
            }
        "#;

        let variables = serde_json::json!({ "input": issue_input });
        let duration_ms = start.elapsed().as_millis() as u64;

        match client.mutate(mutation, variables).await {
            Ok(data) => {
                if let Some(result) = data.get("issueCreate") {
                    let success = result["success"].as_bool().unwrap_or(false);
                    if success {
                        let issue = result
                            .get("issue")
                            .cloned()
                            .unwrap_or(serde_json::json!(null));
                        let identifier = issue["identifier"].as_str().unwrap_or("unknown");
                        info!(identifier, "Created Linear issue");
                        Ok(ToolResult::success(
                            serde_json::json!({ "success": true, "issue": issue }),
                            duration_ms,
                        ))
                    } else {
                        Ok(ToolResult::failure("Failed to create issue", duration_ms))
                    }
                } else {
                    Ok(ToolResult::failure(
                        "Unexpected response format",
                        duration_ms,
                    ))
                }
            }
            Err(e) => {
                warn!(error = %e, "Failed to create issue");
                Ok(ToolResult::from_report(
                    &e.wrap_err("Failed to create issue"),
                    duration_ms,
                ))
            }
        }
    }
}

// ── linear.issue_update ────────────────────────────────────────────────

pub struct LinearIssueUpdateTool {
    clients: Arc<HashMap<String, Arc<LinearClient>>>,
    default_workspace: String,
    prefix_map: Arc<HashMap<String, String>>,
}

impl LinearIssueUpdateTool {
    pub fn new(
        clients: Arc<HashMap<String, Arc<LinearClient>>>,
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

#[async_trait]
impl Tool for LinearIssueUpdateTool {
    fn name(&self) -> &str {
        "linear.issue_update"
    }

    fn description(&self) -> &str {
        "Update an existing issue."
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "identifier": {
                    "type": "string",
                    "description": "Issue identifier string like 'DW-107'"
                },
                "title": {
                    "type": "string",
                    "description": "New title"
                },
                "description": {
                    "type": "string",
                    "description": "New description (markdown supported)"
                },
                "state_id": {
                    "type": "string",
                    "description": "New workflow state UUID string"
                },
                "priority": {
                    "type": "integer",
                    "description": "New priority (integer)",
                    "enum": [0, 1, 2, 3, 4]
                },
                "assignee_id": {
                    "type": "string",
                    "description": "New assignee user UUID string (empty string to unassign)"
                },
                "project_id": {
                    "type": "string",
                    "description": "New project UUID string"
                },
                "label_ids": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "New label UUID strings (replaces existing)"
                },
                "workspace": {
                    "type": "string",
                    "description": workspace_description(&self.clients, &self.default_workspace)
                }
            },
            "required": ["identifier"]
        })
    }

    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();

        let client = match resolve_client(
            &self.clients,
            &self.default_workspace,
            &self.prefix_map,
            &input,
        ) {
            Ok(c) => c,
            Err(e) => {
                return Ok(ToolResult::failure(
                    format!("{e}"),
                    start.elapsed().as_millis() as u64,
                ))
            }
        };

        let identifier = match input["identifier"].as_str() {
            Some(id) => id,
            None => {
                return Ok(ToolResult::failure(
                    "Missing required 'identifier' parameter",
                    start.elapsed().as_millis() as u64,
                ))
            }
        };

        // First, we need to get the issue's UUID from its identifier
        let get_query = r#"
            query($id: String!) {
                issue(id: $id) {
                    id
                }
            }
        "#;

        let get_result = client
            .query(get_query, serde_json::json!({ "id": identifier }))
            .await;

        let issue_uuid = match get_result {
            Ok(data) => match data.get("issue").and_then(|i| i["id"].as_str()) {
                Some(uuid) => uuid.to_string(),
                None => {
                    return Ok(ToolResult::failure(
                        format!("Issue '{}' not found", identifier),
                        start.elapsed().as_millis() as u64,
                    ))
                }
            },
            Err(e) => {
                return Ok(ToolResult::from_report(
                    &e.wrap_err("Failed to find issue"),
                    start.elapsed().as_millis() as u64,
                ))
            }
        };

        // Build update input
        let mut update_input = serde_json::Map::new();

        if let Some(title) = input["title"].as_str() {
            update_input.insert("title".to_string(), serde_json::json!(title));
        }
        if let Some(desc) = input["description"].as_str() {
            update_input.insert(
                "description".to_string(),
                serde_json::json!(unescape_newlines(desc)),
            );
        }
        if let Some(state_id) = input["state_id"].as_str() {
            update_input.insert("stateId".to_string(), serde_json::json!(state_id));
        }
        if let Some(priority) = coerce_to_i64(&input["priority"]) {
            update_input.insert("priority".to_string(), serde_json::json!(priority));
        }
        if let Some(assignee_id) = input["assignee_id"].as_str() {
            if assignee_id.is_empty() {
                update_input.insert("assigneeId".to_string(), serde_json::json!(null));
            } else {
                update_input.insert("assigneeId".to_string(), serde_json::json!(assignee_id));
            }
        }
        if let Some(project_id) = input["project_id"].as_str() {
            update_input.insert("projectId".to_string(), serde_json::json!(project_id));
        }
        if let Some(label_ids) = input["label_ids"].as_array() {
            update_input.insert("labelIds".to_string(), serde_json::json!(label_ids));
        }

        if update_input.is_empty() {
            return Ok(ToolResult::failure(
                "No fields to update",
                start.elapsed().as_millis() as u64,
            ));
        }

        let mutation = r#"
            mutation($id: String!, $input: IssueUpdateInput!) {
                issueUpdate(id: $id, input: $input) {
                    success
                    issue {
                        id
                        identifier
                        title
                        description
                        priority
                        state {
                            id
                            name
                        }
                        assignee {
                            id
                            name
                        }
                        url
                    }
                }
            }
        "#;

        let variables = serde_json::json!({
            "id": issue_uuid,
            "input": Value::Object(update_input)
        });

        let duration_ms = start.elapsed().as_millis() as u64;

        match client.mutate(mutation, variables).await {
            Ok(data) => {
                if let Some(result) = data.get("issueUpdate") {
                    let success = result["success"].as_bool().unwrap_or(false);
                    if success {
                        let issue = result
                            .get("issue")
                            .cloned()
                            .unwrap_or(serde_json::json!(null));
                        info!(identifier, "Updated Linear issue");
                        Ok(ToolResult::success(
                            serde_json::json!({ "success": true, "issue": issue }),
                            duration_ms,
                        ))
                    } else {
                        Ok(ToolResult::failure("Failed to update issue", duration_ms))
                    }
                } else {
                    Ok(ToolResult::failure(
                        "Unexpected response format",
                        duration_ms,
                    ))
                }
            }
            Err(e) => {
                warn!(error = %e, identifier, "Failed to update issue");
                Ok(ToolResult::from_report(
                    &e.wrap_err("Failed to update issue"),
                    duration_ms,
                ))
            }
        }
    }
}

// ── linear.comment_list ────────────────────────────────────────────────

pub struct LinearCommentListTool {
    clients: Arc<HashMap<String, Arc<LinearClient>>>,
    default_workspace: String,
    prefix_map: Arc<HashMap<String, String>>,
}

impl LinearCommentListTool {
    pub fn new(
        clients: Arc<HashMap<String, Arc<LinearClient>>>,
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

#[async_trait]
impl Tool for LinearCommentListTool {
    fn name(&self) -> &str {
        "linear.comment_list"
    }

    fn description(&self) -> &str {
        "List comments on an issue."
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "identifier": {
                    "type": "string",
                    "description": "Issue identifier string like 'DW-107'"
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum number of comments (integer, default: 50, max: 100)",
                    "minimum": 1,
                    "maximum": 100
                },
                "workspace": {
                    "type": "string",
                    "description": workspace_description(&self.clients, &self.default_workspace)
                }
            },
            "required": ["identifier"]
        })
    }

    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();

        let client = match resolve_client(
            &self.clients,
            &self.default_workspace,
            &self.prefix_map,
            &input,
        ) {
            Ok(c) => c,
            Err(e) => {
                return Ok(ToolResult::failure(
                    format!("{e}"),
                    start.elapsed().as_millis() as u64,
                ))
            }
        };

        let identifier = match input["identifier"].as_str() {
            Some(id) => id,
            None => {
                return Ok(ToolResult::failure(
                    "Missing required 'identifier' parameter",
                    start.elapsed().as_millis() as u64,
                ))
            }
        };

        let limit = coerce_to_u64(&input["limit"]).unwrap_or(50).min(100);

        let query = r#"
            query($id: String!, $first: Int!) {
                issue(id: $id) {
                    id
                    identifier
                    comments(first: $first) {
                        nodes {
                            id
                            body
                            user {
                                id
                                name
                                email
                            }
                            botActor {
                                name
                                type
                            }
                            createdAt
                            updatedAt
                            editedAt
                        }
                    }
                }
            }
        "#;

        let variables = serde_json::json!({
            "id": identifier,
            "first": limit
        });

        let duration_ms = start.elapsed().as_millis() as u64;

        match client.query(query, variables).await {
            Ok(data) => {
                if let Some(issue) = data.get("issue") {
                    if issue.is_null() {
                        return Ok(ToolResult::failure(
                            format!("Issue '{}' not found", identifier),
                            duration_ms,
                        ));
                    }
                    let comments = issue
                        .get("comments")
                        .and_then(|c| c.get("nodes"))
                        .cloned()
                        .unwrap_or(serde_json::json!([]));
                    let count = comments.as_array().map(|a| a.len()).unwrap_or(0);
                    // Wrap comment body fields
                    let wrapped_comments: Vec<Value> = comments
                        .as_array()
                        .map(|arr| {
                            arr.iter()
                                .map(|c| wrap_body_field(c, "Linear comment"))
                                .collect()
                        })
                        .unwrap_or_default();
                    info!(identifier, count, "Listed comments for issue");
                    Ok(ToolResult::success(
                        serde_json::json!({ "identifier": identifier, "count": count, "comments": wrapped_comments }),
                        duration_ms,
                    ))
                } else {
                    Ok(ToolResult::failure(
                        format!("Issue '{}' not found", identifier),
                        duration_ms,
                    ))
                }
            }
            Err(e) => {
                warn!(error = %e, identifier, "Failed to list comments");
                Ok(ToolResult::from_report(
                    &e.wrap_err("Failed to list comments"),
                    duration_ms,
                ))
            }
        }
    }
}

// ── linear.comment_add ────────────────────────────────────────────────

pub struct LinearCommentAddTool {
    clients: Arc<HashMap<String, Arc<LinearClient>>>,
    default_workspace: String,
    prefix_map: Arc<HashMap<String, String>>,
}

impl LinearCommentAddTool {
    pub fn new(
        clients: Arc<HashMap<String, Arc<LinearClient>>>,
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

#[async_trait]
impl Tool for LinearCommentAddTool {
    fn name(&self) -> &str {
        "linear.comment_add"
    }

    fn description(&self) -> &str {
        "Add a comment to an issue."
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "identifier": {
                    "type": "string",
                    "description": "Issue identifier string like 'DW-107'"
                },
                "body": {
                    "type": "string",
                    "description": "Comment body (markdown supported)"
                },
                "workspace": {
                    "type": "string",
                    "description": workspace_description(&self.clients, &self.default_workspace)
                }
            },
            "required": ["identifier", "body"]
        })
    }

    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();

        let client = match resolve_client(
            &self.clients,
            &self.default_workspace,
            &self.prefix_map,
            &input,
        ) {
            Ok(c) => c,
            Err(e) => {
                return Ok(ToolResult::failure(
                    format!("{e}"),
                    start.elapsed().as_millis() as u64,
                ))
            }
        };

        let identifier = match input["identifier"].as_str() {
            Some(id) => id,
            None => {
                return Ok(ToolResult::failure(
                    "Missing required 'identifier' parameter",
                    start.elapsed().as_millis() as u64,
                ))
            }
        };

        let body = match input["body"].as_str() {
            Some(b) => b,
            None => {
                return Ok(ToolResult::failure(
                    "Missing required 'body' parameter",
                    start.elapsed().as_millis() as u64,
                ))
            }
        };

        // First, get the issue UUID
        let get_query = r#"
            query($id: String!) {
                issue(id: $id) {
                    id
                }
            }
        "#;

        let get_result = client
            .query(get_query, serde_json::json!({ "id": identifier }))
            .await;

        let issue_uuid = match get_result {
            Ok(data) => match data.get("issue").and_then(|i| i["id"].as_str()) {
                Some(uuid) => uuid.to_string(),
                None => {
                    return Ok(ToolResult::failure(
                        format!("Issue '{}' not found", identifier),
                        start.elapsed().as_millis() as u64,
                    ))
                }
            },
            Err(e) => {
                return Ok(ToolResult::from_report(
                    &e.wrap_err("Failed to find issue"),
                    start.elapsed().as_millis() as u64,
                ))
            }
        };

        let mutation = r#"
            mutation($input: CommentCreateInput!) {
                commentCreate(input: $input) {
                    success
                    comment {
                        id
                        body
                        user {
                            id
                            name
                        }
                        createdAt
                    }
                }
            }
        "#;

        let variables = serde_json::json!({
            "input": {
                "issueId": issue_uuid,
                "body": body
            }
        });

        let duration_ms = start.elapsed().as_millis() as u64;

        match client.mutate(mutation, variables).await {
            Ok(data) => {
                if let Some(result) = data.get("commentCreate") {
                    let success = result["success"].as_bool().unwrap_or(false);
                    if success {
                        let comment = result
                            .get("comment")
                            .cloned()
                            .unwrap_or(serde_json::json!(null));
                        info!(identifier, "Added comment to issue");
                        Ok(ToolResult::success(
                            serde_json::json!({ "success": true, "comment": comment }),
                            duration_ms,
                        ))
                    } else {
                        Ok(ToolResult::failure("Failed to add comment", duration_ms))
                    }
                } else {
                    Ok(ToolResult::failure(
                        "Unexpected response format",
                        duration_ms,
                    ))
                }
            }
            Err(e) => {
                warn!(error = %e, identifier, "Failed to add comment");
                Ok(ToolResult::from_report(
                    &e.wrap_err("Failed to add comment"),
                    duration_ms,
                ))
            }
        }
    }
}

// ── linear.states_list ────────────────────────────────────────────────

pub struct LinearStatesListTool {
    clients: Arc<HashMap<String, Arc<LinearClient>>>,
    default_workspace: String,
    default_team_id: Option<String>,
    prefix_map: Arc<HashMap<String, String>>,
}

impl LinearStatesListTool {
    pub fn new(
        clients: Arc<HashMap<String, Arc<LinearClient>>>,
        default_workspace: String,
        default_team_id: Option<String>,
        prefix_map: Arc<HashMap<String, String>>,
    ) -> Self {
        Self {
            clients,
            default_workspace,
            default_team_id,
            prefix_map,
        }
    }
}

#[async_trait]
impl Tool for LinearStatesListTool {
    fn name(&self) -> &str {
        "linear.states_list"
    }

    fn description(&self) -> &str {
        "List workflow states for a team."
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "team_id": {
                    "type": "string",
                    "description": "Team UUID string. Uses default_team_id if omitted."
                },
                "workspace": {
                    "type": "string",
                    "description": workspace_description(&self.clients, &self.default_workspace)
                }
            },
            "required": []
        })
    }

    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();

        let client = match resolve_client(
            &self.clients,
            &self.default_workspace,
            &self.prefix_map,
            &input,
        ) {
            Ok(c) => c,
            Err(e) => {
                return Ok(ToolResult::failure(
                    format!("{e}"),
                    start.elapsed().as_millis() as u64,
                ))
            }
        };

        let team_id = match resolve_team_id(&input, &self.default_team_id) {
            Ok(t) => t,
            Err(e) => return Ok(ToolResult::failure(e, start.elapsed().as_millis() as u64)),
        };

        let query = r#"
            query($teamId: String!) {
                team(id: $teamId) {
                    id
                    name
                    states {
                        nodes {
                            id
                            name
                            type
                            color
                        }
                    }
                }
            }
        "#;

        let variables = serde_json::json!({ "teamId": team_id });
        let duration_ms = start.elapsed().as_millis() as u64;

        match client.query(query, variables).await {
            Ok(data) => {
                if let Some(team) = data.get("team") {
                    if team.is_null() {
                        return Ok(ToolResult::failure(
                            format!("Team '{}' not found", team_id),
                            duration_ms,
                        ));
                    }
                    let team_name = team["name"].as_str().unwrap_or("unknown");
                    let states = team
                        .get("states")
                        .and_then(|s| s.get("nodes"))
                        .cloned()
                        .unwrap_or(serde_json::json!([]));
                    let count = states.as_array().map(|a| a.len()).unwrap_or(0);
                    info!(team_id, team_name, count, "Listed workflow states");
                    Ok(ToolResult::success(
                        serde_json::json!({
                            "team_id": team_id,
                            "team_name": team_name,
                            "count": count,
                            "states": states
                        }),
                        duration_ms,
                    ))
                } else {
                    Ok(ToolResult::failure(
                        format!("Team '{}' not found", team_id),
                        duration_ms,
                    ))
                }
            }
            Err(e) => {
                warn!(error = %e, team_id, "Failed to list states");
                Ok(ToolResult::from_report(
                    &e.wrap_err("Failed to list states"),
                    duration_ms,
                ))
            }
        }
    }
}

// ── linear.teams_list ────────────────────────────────────────────────

pub struct LinearTeamsListTool {
    clients: Arc<HashMap<String, Arc<LinearClient>>>,
    default_workspace: String,
    prefix_map: Arc<HashMap<String, String>>,
}

impl LinearTeamsListTool {
    pub fn new(
        clients: Arc<HashMap<String, Arc<LinearClient>>>,
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

#[async_trait]
impl Tool for LinearTeamsListTool {
    fn name(&self) -> &str {
        "linear.teams_list"
    }

    fn description(&self) -> &str {
        "List all teams in the workspace."
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "workspace": {
                    "type": "string",
                    "description": workspace_description(&self.clients, &self.default_workspace)
                }
            },
            "required": []
        })
    }

    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();

        let client = match resolve_client(
            &self.clients,
            &self.default_workspace,
            &self.prefix_map,
            &input,
        ) {
            Ok(c) => c,
            Err(e) => {
                return Ok(ToolResult::failure(
                    format!("{e}"),
                    start.elapsed().as_millis() as u64,
                ))
            }
        };

        let query = r#"
            query {
                teams {
                    nodes {
                        id
                        name
                        key
                        description
                    }
                }
            }
        "#;

        let duration_ms = start.elapsed().as_millis() as u64;

        match client.query(query, serde_json::json!({})).await {
            Ok(data) => {
                let teams = data
                    .get("teams")
                    .and_then(|t| t.get("nodes"))
                    .cloned()
                    .unwrap_or(serde_json::json!([]));
                let count = teams.as_array().map(|a| a.len()).unwrap_or(0);
                info!(count, "Listed Linear teams");
                Ok(ToolResult::success(
                    serde_json::json!({ "count": count, "teams": teams }),
                    duration_ms,
                ))
            }
            Err(e) => {
                warn!(error = %e, "Failed to list teams");
                Ok(ToolResult::from_report(
                    &e.wrap_err("Failed to list teams"),
                    duration_ms,
                ))
            }
        }
    }
}

// ── linear.project_list ────────────────────────────────────────────────

pub struct LinearProjectListTool {
    clients: Arc<HashMap<String, Arc<LinearClient>>>,
    default_workspace: String,
    prefix_map: Arc<HashMap<String, String>>,
}

impl LinearProjectListTool {
    pub fn new(
        clients: Arc<HashMap<String, Arc<LinearClient>>>,
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

#[async_trait]
impl Tool for LinearProjectListTool {
    fn name(&self) -> &str {
        "linear.project_list"
    }

    fn description(&self) -> &str {
        "List all projects in the workspace."
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "limit": {
                    "type": "integer",
                    "description": "Maximum number of projects (integer, default: 50, max: 100)",
                    "minimum": 1,
                    "maximum": 100
                },
                "include_archived": {
                    "type": "boolean",
                    "description": "Include archived projects (default: false)"
                },
                "workspace": {
                    "type": "string",
                    "description": workspace_description(&self.clients, &self.default_workspace)
                }
            },
            "required": []
        })
    }

    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();

        let client = match resolve_client(
            &self.clients,
            &self.default_workspace,
            &self.prefix_map,
            &input,
        ) {
            Ok(c) => c,
            Err(e) => {
                return Ok(ToolResult::failure(
                    format!("{e}"),
                    start.elapsed().as_millis() as u64,
                ))
            }
        };

        let limit = coerce_to_u64(&input["limit"]).unwrap_or(50).min(100);
        let include_archived = input["include_archived"].as_bool().unwrap_or(false);

        let query = r#"
            query($first: Int!, $includeArchived: Boolean) {
                projects(first: $first, includeArchived: $includeArchived) {
                    nodes {
                        id
                        name
                        description
                        slugId
                        icon
                        color
                        state
                        status {
                            id
                            name
                        }
                        lead {
                            id
                            name
                        }
                        startDate
                        targetDate
                        url
                        progress
                        createdAt
                        updatedAt
                    }
                }
            }
        "#;

        let variables = serde_json::json!({
            "first": limit,
            "includeArchived": include_archived
        });
        let duration_ms = start.elapsed().as_millis() as u64;

        match client.query(query, variables).await {
            Ok(data) => {
                let projects = data
                    .get("projects")
                    .and_then(|p| p.get("nodes"))
                    .cloned()
                    .unwrap_or(serde_json::json!([]));
                let count = projects.as_array().map(|a| a.len()).unwrap_or(0);
                info!(count, "Listed Linear projects");
                Ok(ToolResult::success(
                    serde_json::json!({ "count": count, "projects": projects }),
                    duration_ms,
                ))
            }
            Err(e) => {
                warn!(error = %e, "Failed to list projects");
                Ok(ToolResult::from_report(
                    &e.wrap_err("Failed to list projects"),
                    duration_ms,
                ))
            }
        }
    }
}

// ── linear.project_get ────────────────────────────────────────────────

pub struct LinearProjectGetTool {
    clients: Arc<HashMap<String, Arc<LinearClient>>>,
    default_workspace: String,
    prefix_map: Arc<HashMap<String, String>>,
}

impl LinearProjectGetTool {
    pub fn new(
        clients: Arc<HashMap<String, Arc<LinearClient>>>,
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

#[async_trait]
impl Tool for LinearProjectGetTool {
    fn name(&self) -> &str {
        "linear.project_get"
    }

    fn description(&self) -> &str {
        "Get a specific project by its ID."
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "id": {
                    "type": "string",
                    "description": "Project UUID string"
                },
                "workspace": {
                    "type": "string",
                    "description": workspace_description(&self.clients, &self.default_workspace)
                }
            },
            "required": ["id"]
        })
    }

    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();

        let client = match resolve_client(
            &self.clients,
            &self.default_workspace,
            &self.prefix_map,
            &input,
        ) {
            Ok(c) => c,
            Err(e) => {
                return Ok(ToolResult::failure(
                    format!("{e}"),
                    start.elapsed().as_millis() as u64,
                ))
            }
        };

        let id = match input["id"].as_str() {
            Some(id) => id,
            None => {
                return Ok(ToolResult::failure(
                    "Missing required 'id' parameter",
                    start.elapsed().as_millis() as u64,
                ))
            }
        };

        let query = r#"
            query($id: String!) {
                project(id: $id) {
                    id
                    name
                    description
                    slugId
                    icon
                    color
                    state
                    status {
                        id
                        name
                    }
                    lead {
                        id
                        name
                        email
                    }
                    startDate
                    targetDate
                    url
                    progress
                    createdAt
                    updatedAt
                    completedAt
                    canceledAt
                }
            }
        "#;

        let variables = serde_json::json!({ "id": id });
        let duration_ms = start.elapsed().as_millis() as u64;

        match client.query(query, variables).await {
            Ok(data) => {
                if let Some(project) = data.get("project") {
                    if project.is_null() {
                        return Ok(ToolResult::failure(
                            format!("Project '{}' not found", id),
                            duration_ms,
                        ));
                    }
                    info!(id, "Retrieved Linear project");
                    let wrapped = wrap_description_field(project, "Linear project description");
                    Ok(ToolResult::success(
                        serde_json::json!({ "project": wrapped }),
                        duration_ms,
                    ))
                } else {
                    Ok(ToolResult::failure(
                        format!("Project '{}' not found", id),
                        duration_ms,
                    ))
                }
            }
            Err(e) => {
                warn!(error = %e, id, "Failed to get project");
                Ok(ToolResult::from_report(
                    &e.wrap_err("Failed to get project"),
                    duration_ms,
                ))
            }
        }
    }
}

// ── linear.labels_list ────────────────────────────────────────────────

pub struct LinearLabelsListTool {
    clients: Arc<HashMap<String, Arc<LinearClient>>>,
    default_workspace: String,
    prefix_map: Arc<HashMap<String, String>>,
}

impl LinearLabelsListTool {
    pub fn new(
        clients: Arc<HashMap<String, Arc<LinearClient>>>,
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

#[async_trait]
impl Tool for LinearLabelsListTool {
    fn name(&self) -> &str {
        "linear.labels_list"
    }

    fn description(&self) -> &str {
        "List all issue labels in the workspace."
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "limit": {
                    "type": "integer",
                    "description": "Maximum number of labels (integer, default: 100, max: 250)",
                    "minimum": 1,
                    "maximum": 250
                },
                "workspace": {
                    "type": "string",
                    "description": workspace_description(&self.clients, &self.default_workspace)
                }
            },
            "required": []
        })
    }

    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();

        let client = match resolve_client(
            &self.clients,
            &self.default_workspace,
            &self.prefix_map,
            &input,
        ) {
            Ok(c) => c,
            Err(e) => {
                return Ok(ToolResult::failure(
                    format!("{e}"),
                    start.elapsed().as_millis() as u64,
                ))
            }
        };

        let limit = coerce_to_u64(&input["limit"]).unwrap_or(100).min(250);

        let query = r#"
            query($first: Int!) {
                issueLabels(first: $first) {
                    nodes {
                        id
                        name
                        color
                    }
                }
            }
        "#;

        let variables = serde_json::json!({ "first": limit });
        let duration_ms = start.elapsed().as_millis() as u64;

        match client.query(query, variables).await {
            Ok(data) => {
                let labels = data
                    .get("issueLabels")
                    .and_then(|l| l.get("nodes"))
                    .cloned()
                    .unwrap_or(serde_json::json!([]));
                let count = labels.as_array().map(|a| a.len()).unwrap_or(0);
                info!(count, "Listed Linear labels");
                Ok(ToolResult::success(
                    serde_json::json!({ "count": count, "labels": labels }),
                    duration_ms,
                ))
            }
            Err(e) => {
                warn!(error = %e, "Failed to list labels");
                Ok(ToolResult::from_report(
                    &e.wrap_err("Failed to list labels"),
                    duration_ms,
                ))
            }
        }
    }
}

// ── linear.viewer ──────────────────────────────────────────────────────

/// Who the workspace's key belongs to: the person's own Linear user.
pub struct LinearViewerTool {
    clients: Arc<HashMap<String, Arc<LinearClient>>>,
    default_workspace: String,
    prefix_map: Arc<HashMap<String, String>>,
}

impl LinearViewerTool {
    pub fn new(
        clients: Arc<HashMap<String, Arc<LinearClient>>>,
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

#[async_trait]
impl Tool for LinearViewerTool {
    fn name(&self) -> &str {
        "linear.viewer"
    }

    fn description(&self) -> &str {
        "Who the Linear key in use belongs to: its user's id, name and email, and whether it is your own key (personal) or one shared with others."
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "workspace": {
                    "type": "string",
                    "description": workspace_description(&self.clients, &self.default_workspace)
                }
            },
            "required": []
        })
    }

    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();
        let client = match resolve_client(
            &self.clients,
            &self.default_workspace,
            &self.prefix_map,
            &input,
        ) {
            Ok(c) => c,
            Err(e) => {
                return Ok(ToolResult::failure(
                    format!("{e}"),
                    start.elapsed().as_millis() as u64,
                ))
            }
        };
        let query = "query { viewer { id name email displayName organization { urlKey } } }";
        // A shared key's viewer is whoever owns the key, not the caller.
        let personal = client.key_is_personal();
        match client.query(query, serde_json::json!({})).await {
            Ok(data) => {
                let v = &data["viewer"];
                Ok(ToolResult::success(
                    serde_json::json!({
                        "id": v["id"],
                        "name": v["name"],
                        "display_name": v["displayName"],
                        "email": v["email"],
                        "personal": personal,
                        // Which Linear workspace this is.
                        "workspace": v["organization"]["urlKey"],
                    }),
                    start.elapsed().as_millis() as u64,
                ))
            }
            Err(e) => {
                warn!(error = %e, "Failed to read the Linear viewer");
                Ok(ToolResult::from_report(
                    &e.wrap_err("Failed to read who you are on Linear"),
                    start.elapsed().as_millis() as u64,
                ))
            }
        }
    }
}

// ── linear.upload_read ─────────────────────────────────────────────────

/// A chunk of a file uploaded to Linear (a picture in an issue's
/// description), for Desk's board sync: read with the workspace's key, from
/// `https://uploads.linear.app/` only.
pub struct LinearUploadReadTool {
    clients: Arc<HashMap<String, Arc<LinearClient>>>,
    default_workspace: String,
    prefix_map: Arc<HashMap<String, String>>,
}

impl LinearUploadReadTool {
    pub fn new(
        clients: Arc<HashMap<String, Arc<LinearClient>>>,
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

/// Whether `id` can be a Linear issue's identifier (`ENG-123`).
fn identifier_ok(id: &str) -> bool {
    let Some((team, n)) = id.rsplit_once('-') else {
        return false;
    };
    let mut t = team.chars();
    t.next().is_some_and(|c| c.is_ascii_uppercase())
        && t.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
        && team.len() <= 10
        && !n.is_empty()
        && n.len() <= 12
        && n.chars().all(|c| c.is_ascii_digit())
}

/// The addresses a Markdown description links to or shows (`[..](url)`,
/// `![..](url)`), as written.
fn linked_urls(description: &str) -> Vec<&str> {
    description
        .match_indices("](")
        .map(|(i, _)| {
            let rest = &description[i + 2..];
            let end = rest
                .find(|c: char| c == ')' || c.is_whitespace())
                .unwrap_or(rest.len());
            &rest[..end]
        })
        .collect()
}

/// Whether `url` is one of issue `issue`'s own files: an attachment it
/// lists, or an upload its description shows or links to.
fn issue_lists(issue: &Value, url: &str) -> bool {
    let attached = issue["attachments"]["nodes"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|a| a["url"].as_str() == Some(url));
    attached
        || issue["description"]
            .as_str()
            .is_some_and(|d| linked_urls(d).contains(&url))
}

/// The type a Linear upload's address says, for the answer; the reader
/// sniffs the bytes whatever this says.
fn upload_type(url: &str) -> &'static str {
    let path = url
        .split(['?', '#'])
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    match path.rsplit_once('.').map(|(_, ext)| ext) {
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("pdf") => "application/pdf",
        _ => "application/octet-stream",
    }
}

#[async_trait]
impl Tool for LinearUploadReadTool {
    fn name(&self) -> &str {
        "linear.upload_read"
    }

    fn description(&self) -> &str {
        "Desk's board sync reads a file uploaded to Linear (https://uploads.linear.app/...) with this, a base64 chunk at a time."
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "url": { "type": "string", "description": "The upload's https://uploads.linear.app/ address" },
                "identifier": { "type": "string", "description": "The issue it is on, like 'DW-107'. Only a file that issue shows or lists is read." },
                "offset": { "type": "integer", "minimum": 0 },
                "length": { "type": "integer", "minimum": 1, "description": "At most 524288" },
                "workspace": {
                    "type": "string",
                    "description": workspace_description(&self.clients, &self.default_workspace)
                }
            },
            "required": ["identifier", "url"]
        })
    }

    async fn execute(&self, input: Value) -> Result<ToolResult> {
        use crate::adapters::file_read::{self, memo};
        let start = std::time::Instant::now();
        let ms = || start.elapsed().as_millis() as u64;
        let client = match resolve_client(
            &self.clients,
            &self.default_workspace,
            &self.prefix_map,
            &input,
        ) {
            Ok(c) => c,
            Err(e) => return Ok(ToolResult::failure(format!("{e}"), ms())),
        };
        let url = match input["url"].as_str() {
            Some(u) if file_read::is_linear_upload(u) => u.to_string(),
            _ => {
                return Ok(ToolResult::failure(
                    "url is a file uploaded to Linear, at https://uploads.linear.app/",
                    ms(),
                ))
            }
        };
        let range = match file_read::range_of(&input) {
            Ok(r) => r,
            Err(e) => return Ok(ToolResult::failure(e, ms())),
        };
        // Only a file its issue shows or lists is read: any upload's address
        // alone would reach every file the key can see.
        let identifier = match input["identifier"].as_str() {
            Some(i) if identifier_ok(i) => i.to_string(),
            _ => {
                return Ok(ToolResult::failure(
                    "identifier is the issue the file is on, like ENG-123",
                    ms(),
                ))
            }
        };
        // Kept by the issue too: a kept file was checked against that issue.
        let key = match client.credential_key() {
            Ok(k) => format!("linear:{k}:{identifier}:{url}"),
            Err(e) => return Ok(ToolResult::from_report(&e, ms())),
        };
        if let Some((bytes, media_type)) = memo().get(&key) {
            return Ok(ToolResult::success(
                file_read::chunk(&bytes, &media_type, range),
                ms(),
            ));
        }
        let query = r#"
            query($id: String!) {
                issue(id: $id) {
                    description
                    attachments(first: 50) { nodes { url } }
                }
            }
        "#;
        match client
            .query(query, serde_json::json!({ "id": identifier }))
            .await
        {
            Ok(data) if issue_lists(&data["issue"], &url) => {}
            Ok(_) => {
                return Ok(ToolResult::failure(
                    format!("{identifier} doesn't show or list that file"),
                    ms(),
                ))
            }
            Err(e) => {
                return Ok(ToolResult::from_report(
                    &e.wrap_err(format!("Failed to read {identifier}'s files")),
                    ms(),
                ))
            }
        }
        match client.get_file(&url).await {
            Ok(bytes) => {
                let bytes = Arc::new(bytes);
                let media_type = upload_type(&url).to_string();
                let answer = file_read::chunk(&bytes, &media_type, range);
                memo().put(key, bytes, media_type);
                Ok(ToolResult::success(answer, ms()))
            }
            Err(e) => {
                warn!(error = %e, "A Linear upload wasn't read");
                Ok(ToolResult::from_report(
                    &e.wrap_err("Failed to read the Linear upload"),
                    ms(),
                ))
            }
        }
    }
}

// ── linear.members_list ────────────────────────────────────────────────

pub struct LinearMembersListTool {
    clients: Arc<HashMap<String, Arc<LinearClient>>>,
    default_workspace: String,
    prefix_map: Arc<HashMap<String, String>>,
}

impl LinearMembersListTool {
    pub fn new(
        clients: Arc<HashMap<String, Arc<LinearClient>>>,
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

#[async_trait]
impl Tool for LinearMembersListTool {
    fn name(&self) -> &str {
        "linear.members_list"
    }

    fn description(&self) -> &str {
        "List all members in the workspace."
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "limit": {
                    "type": "integer",
                    "description": "Maximum number of members (integer, default: 100, max: 250)",
                    "minimum": 1,
                    "maximum": 250
                },
                "email": {
                    "type": "string",
                    "description": "Only the members with exactly this email (any case)"
                },
                "name": {
                    "type": "string",
                    "description": "Only the members whose name or display name contains this (any case)"
                },
                "workspace": {
                    "type": "string",
                    "description": workspace_description(&self.clients, &self.default_workspace)
                }
            },
            "required": []
        })
    }

    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();

        let client = match resolve_client(
            &self.clients,
            &self.default_workspace,
            &self.prefix_map,
            &input,
        ) {
            Ok(c) => c,
            Err(e) => {
                return Ok(ToolResult::failure(
                    format!("{e}"),
                    start.elapsed().as_millis() as u64,
                ))
            }
        };

        let limit = coerce_to_u64(&input["limit"]).unwrap_or(100).min(250);

        // An email or a name, when given, is a variable: never written into
        // the query.
        let given = |key: &str| {
            input[key]
                .as_str()
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .map(str::to_string)
        };
        let filter = match (given("email"), given("name")) {
            (Some(email), _) => serde_json::json!({ "email": { "eqIgnoreCase": email } }),
            (None, Some(name)) => serde_json::json!({ "or": [
                { "name": { "containsIgnoreCase": name } },
                { "displayName": { "containsIgnoreCase": name } },
            ] }),
            (None, None) => Value::Null,
        };
        let query = r#"
            query($first: Int!, $filter: UserFilter) {
                users(first: $first, filter: $filter) {
                    nodes {
                        id
                        name
                        email
                        displayName
                        active
                        avatarUrl
                    }
                }
            }
        "#;
        let variables = serde_json::json!({ "first": limit, "filter": filter });
        let duration_ms = start.elapsed().as_millis() as u64;

        match client.query(query, variables).await {
            Ok(data) => {
                let members = data
                    .get("users")
                    .and_then(|u| u.get("nodes"))
                    .cloned()
                    .unwrap_or(serde_json::json!([]));
                let count = members.as_array().map(|a| a.len()).unwrap_or(0);
                info!(count, "Listed Linear members");
                Ok(ToolResult::success(
                    serde_json::json!({ "count": count, "members": members }),
                    duration_ms,
                ))
            }
            Err(e) => {
                warn!(error = %e, "Failed to list members");
                Ok(ToolResult::from_report(
                    &e.wrap_err("Failed to list members"),
                    duration_ms,
                ))
            }
        }
    }
}

// ── linear.issue_bulk_update ────────────────────────────────────────────────

pub struct LinearIssueBulkUpdateTool {
    clients: Arc<HashMap<String, Arc<LinearClient>>>,
    default_workspace: String,
    prefix_map: Arc<HashMap<String, String>>,
}

impl LinearIssueBulkUpdateTool {
    pub fn new(
        clients: Arc<HashMap<String, Arc<LinearClient>>>,
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

#[async_trait]
impl Tool for LinearIssueBulkUpdateTool {
    fn name(&self) -> &str {
        "linear.issue_bulk_update"
    }

    fn description(&self) -> &str {
        "Bulk update multiple issues at once (state, priority, assignee, labels)."
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "identifiers": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Array of issue identifier strings like ['DW-101', 'DW-102']"
                },
                "state_id": {
                    "type": "string",
                    "description": "New workflow state UUID string"
                },
                "priority": {
                    "type": "integer",
                    "description": "New priority (integer)",
                    "enum": [0, 1, 2, 3, 4]
                },
                "assignee_id": {
                    "type": "string",
                    "description": "New assignee user UUID string"
                },
                "label_ids": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "New label UUID strings (replaces existing)"
                },
                "workspace": {
                    "type": "string",
                    "description": workspace_description(&self.clients, &self.default_workspace)
                }
            },
            "required": ["identifiers"]
        })
    }

    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();

        let client = match resolve_client(
            &self.clients,
            &self.default_workspace,
            &self.prefix_map,
            &input,
        ) {
            Ok(c) => c,
            Err(e) => {
                return Ok(ToolResult::failure(
                    format!("{e}"),
                    start.elapsed().as_millis() as u64,
                ))
            }
        };

        let identifiers = match input["identifiers"].as_array() {
            Some(arr) => arr
                .iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect::<Vec<_>>(),
            None => {
                return Ok(ToolResult::failure(
                    "Missing required 'identifiers' parameter (array of strings)",
                    start.elapsed().as_millis() as u64,
                ))
            }
        };

        if identifiers.is_empty() {
            return Ok(ToolResult::failure(
                "'identifiers' array must not be empty",
                start.elapsed().as_millis() as u64,
            ));
        }

        // Resolve identifiers to UUIDs by looking them up one by one
        // (Linear's issue(id:) accepts human-readable identifiers)
        let mut uuids = Vec::new();
        for ident in &identifiers {
            let get_query = r#"
                query($id: String!) {
                    issue(id: $id) {
                        id
                        identifier
                    }
                }
            "#;
            match client
                .query(get_query, serde_json::json!({ "id": ident }))
                .await
            {
                Ok(data) => {
                    if let Some(uuid) = data.get("issue").and_then(|i| i["id"].as_str()) {
                        uuids.push(uuid.to_string());
                    } else {
                        return Ok(ToolResult::failure(
                            format!("Issue '{}' not found", ident),
                            start.elapsed().as_millis() as u64,
                        ));
                    }
                }
                Err(e) => {
                    return Ok(ToolResult::from_report(
                        &e.wrap_err(format!("Failed to resolve issue '{}'", ident)),
                        start.elapsed().as_millis() as u64,
                    ));
                }
            }
        }

        // Build the update input
        let mut update_input = serde_json::Map::new();

        if let Some(state_id) = input["state_id"].as_str() {
            update_input.insert("stateId".to_string(), serde_json::json!(state_id));
        }
        if let Some(priority) = coerce_to_i64(&input["priority"]) {
            update_input.insert("priority".to_string(), serde_json::json!(priority));
        }
        if let Some(assignee_id) = input["assignee_id"].as_str() {
            if assignee_id.is_empty() {
                update_input.insert("assigneeId".to_string(), serde_json::json!(null));
            } else {
                update_input.insert("assigneeId".to_string(), serde_json::json!(assignee_id));
            }
        }
        if let Some(label_ids) = input["label_ids"].as_array() {
            update_input.insert("labelIds".to_string(), serde_json::json!(label_ids));
        }

        if update_input.is_empty() {
            return Ok(ToolResult::failure(
                "No fields to update. Provide at least one of: state_id, priority, assignee_id, label_ids",
                start.elapsed().as_millis() as u64,
            ));
        }

        let mutation = r#"
            mutation($ids: [UUID!]!, $input: IssueUpdateInput!) {
                issueBatchUpdate(ids: $ids, input: $input) {
                    success
                    issues {
                        id
                        identifier
                        title
                        state {
                            name
                        }
                        priority
                    }
                }
            }
        "#;

        let variables = serde_json::json!({
            "ids": uuids,
            "input": Value::Object(update_input)
        });

        let duration_ms = start.elapsed().as_millis() as u64;

        match client.mutate(mutation, variables).await {
            Ok(data) => {
                if let Some(result) = data.get("issueBatchUpdate") {
                    let success = result["success"].as_bool().unwrap_or(false);
                    if success {
                        let issues = result
                            .get("issues")
                            .cloned()
                            .unwrap_or(serde_json::json!([]));
                        let count = issues.as_array().map(|a| a.len()).unwrap_or(0);
                        info!(count, "Bulk updated Linear issues");
                        Ok(ToolResult::success(
                            serde_json::json!({ "success": true, "count": count, "issues": issues }),
                            duration_ms,
                        ))
                    } else {
                        Ok(ToolResult::failure("Bulk update failed", duration_ms))
                    }
                } else {
                    Ok(ToolResult::failure(
                        "Unexpected response format",
                        duration_ms,
                    ))
                }
            }
            Err(e) => {
                warn!(error = %e, "Failed to bulk update issues");
                Ok(ToolResult::from_report(
                    &e.wrap_err("Failed to bulk update issues"),
                    duration_ms,
                ))
            }
        }
    }
}

// ── linear.issue_link ────────────────────────────────────────────────

pub struct LinearIssueLinkTool {
    clients: Arc<HashMap<String, Arc<LinearClient>>>,
    default_workspace: String,
    prefix_map: Arc<HashMap<String, String>>,
}

impl LinearIssueLinkTool {
    pub fn new(
        clients: Arc<HashMap<String, Arc<LinearClient>>>,
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

#[async_trait]
impl Tool for LinearIssueLinkTool {
    fn name(&self) -> &str {
        "linear.issue_link"
    }

    fn description(&self) -> &str {
        "Create a relation between two issues (blocks, blocked_by, related, duplicate)."
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "identifier": {
                    "type": "string",
                    "description": "Source issue identifier string like 'DW-101'"
                },
                "related_identifier": {
                    "type": "string",
                    "description": "Target/related issue identifier string like 'DW-102'"
                },
                "type": {
                    "type": "string",
                    "description": "Relation type between the two issues",
                    "enum": ["blocks", "blocked_by", "related", "duplicate"]
                },
                "workspace": {
                    "type": "string",
                    "description": workspace_description(&self.clients, &self.default_workspace)
                }
            },
            "required": ["identifier", "related_identifier", "type"]
        })
    }

    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();

        let client = match resolve_client(
            &self.clients,
            &self.default_workspace,
            &self.prefix_map,
            &input,
        ) {
            Ok(c) => c,
            Err(e) => {
                return Ok(ToolResult::failure(
                    format!("{e}"),
                    start.elapsed().as_millis() as u64,
                ))
            }
        };

        let identifier = match input["identifier"].as_str() {
            Some(id) => id,
            None => {
                return Ok(ToolResult::failure(
                    "Missing required 'identifier' parameter",
                    start.elapsed().as_millis() as u64,
                ))
            }
        };

        let related_identifier = match input["related_identifier"].as_str() {
            Some(id) => id,
            None => {
                return Ok(ToolResult::failure(
                    "Missing required 'related_identifier' parameter",
                    start.elapsed().as_millis() as u64,
                ))
            }
        };

        let relation_type =
            match input["type"].as_str() {
                Some(t) => t,
                None => return Ok(ToolResult::failure(
                    "Missing required 'type' parameter (blocks, blocked_by, related, duplicate)",
                    start.elapsed().as_millis() as u64,
                )),
            };

        // Resolve both identifiers to UUIDs
        let get_query = r#"
            query($id: String!) {
                issue(id: $id) {
                    id
                    identifier
                }
            }
        "#;

        let issue_uuid = match client
            .query(get_query, serde_json::json!({ "id": identifier }))
            .await
        {
            Ok(data) => match data.get("issue").and_then(|i| i["id"].as_str()) {
                Some(uuid) => uuid.to_string(),
                None => {
                    return Ok(ToolResult::failure(
                        format!("Issue '{}' not found", identifier),
                        start.elapsed().as_millis() as u64,
                    ))
                }
            },
            Err(e) => {
                return Ok(ToolResult::from_report(
                    &e.wrap_err(format!("Failed to resolve issue '{}'", identifier)),
                    start.elapsed().as_millis() as u64,
                ))
            }
        };

        let related_uuid = match client
            .query(get_query, serde_json::json!({ "id": related_identifier }))
            .await
        {
            Ok(data) => match data.get("issue").and_then(|i| i["id"].as_str()) {
                Some(uuid) => uuid.to_string(),
                None => {
                    return Ok(ToolResult::failure(
                        format!("Issue '{}' not found", related_identifier),
                        start.elapsed().as_millis() as u64,
                    ))
                }
            },
            Err(e) => {
                return Ok(ToolResult::from_report(
                    &e.wrap_err(format!("Failed to resolve issue '{}'", related_identifier)),
                    start.elapsed().as_millis() as u64,
                ))
            }
        };

        // Map type string to Linear enum and determine issue/related ordering.
        // For "blocked_by", we swap issue and related so the Linear "blocks" relation is correct.
        let (linear_type, final_issue_id, final_related_id) = match relation_type {
            "blocks" => ("blocks", issue_uuid.as_str(), related_uuid.as_str()),
            "blocked_by" => ("blocks", related_uuid.as_str(), issue_uuid.as_str()),
            "related" => ("related", issue_uuid.as_str(), related_uuid.as_str()),
            "duplicate" => ("duplicate", issue_uuid.as_str(), related_uuid.as_str()),
            other => {
                return Ok(ToolResult::failure(
                    format!(
                        "Invalid relation type: '{}'. Must be one of: blocks, blocked_by, related, duplicate",
                        other
                    ),
                    start.elapsed().as_millis() as u64,
                ))
            }
        };

        let mutation = r#"
            mutation($input: IssueRelationCreateInput!) {
                issueRelationCreate(input: $input) {
                    success
                    issueRelation {
                        id
                        type
                        issue {
                            id
                            identifier
                            title
                        }
                        relatedIssue {
                            id
                            identifier
                            title
                        }
                    }
                }
            }
        "#;

        let variables = serde_json::json!({
            "input": {
                "issueId": final_issue_id,
                "relatedIssueId": final_related_id,
                "type": linear_type
            }
        });

        let duration_ms = start.elapsed().as_millis() as u64;

        match client.mutate(mutation, variables).await {
            Ok(data) => {
                if let Some(result) = data.get("issueRelationCreate") {
                    let success = result["success"].as_bool().unwrap_or(false);
                    if success {
                        let relation = result
                            .get("issueRelation")
                            .cloned()
                            .unwrap_or(serde_json::json!(null));
                        info!(
                            identifier,
                            related_identifier, relation_type, "Created issue relation"
                        );
                        Ok(ToolResult::success(
                            serde_json::json!({ "success": true, "relation": relation }),
                            duration_ms,
                        ))
                    } else {
                        Ok(ToolResult::failure(
                            "Failed to create issue relation",
                            duration_ms,
                        ))
                    }
                } else {
                    Ok(ToolResult::failure(
                        "Unexpected response format",
                        duration_ms,
                    ))
                }
            }
            Err(e) => {
                warn!(error = %e, identifier, related_identifier, "Failed to create issue relation");
                Ok(ToolResult::from_report(
                    &e.wrap_err("Failed to create issue relation"),
                    duration_ms,
                ))
            }
        }
    }
}

// ── linear.document_get ────────────────────────────────────────────────

pub struct LinearDocumentGetTool {
    clients: Arc<HashMap<String, Arc<LinearClient>>>,
    default_workspace: String,
    prefix_map: Arc<HashMap<String, String>>,
}

impl LinearDocumentGetTool {
    pub fn new(
        clients: Arc<HashMap<String, Arc<LinearClient>>>,
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

/// Extract a document ID from a Linear URL or return the input as-is.
/// Linear document URLs end with `/<title>-<uuid>` so we take the last path segment.
fn extract_document_id(input: &str) -> &str {
    if input.starts_with("http") {
        input
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .unwrap_or(input)
    } else {
        input
    }
}

#[async_trait]
impl Tool for LinearDocumentGetTool {
    fn name(&self) -> &str {
        "linear.document_get"
    }

    fn description(&self) -> &str {
        "Get a specific Linear document by its UUID or URL."
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "id": {
                    "type": "string",
                    "description": "Document UUID or Linear document URL"
                },
                "workspace": {
                    "type": "string",
                    "description": workspace_description(&self.clients, &self.default_workspace)
                }
            },
            "required": ["id"]
        })
    }

    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();

        let client = match resolve_client(
            &self.clients,
            &self.default_workspace,
            &self.prefix_map,
            &input,
        ) {
            Ok(c) => c,
            Err(e) => {
                return Ok(ToolResult::failure(
                    format!("{e}"),
                    start.elapsed().as_millis() as u64,
                ))
            }
        };

        let raw_id = match input["id"].as_str() {
            Some(id) => id,
            None => {
                return Ok(ToolResult::failure(
                    "Missing required 'id' parameter",
                    start.elapsed().as_millis() as u64,
                ))
            }
        };

        let doc_id = extract_document_id(raw_id);

        let query = r#"
            query($id: String!) {
                document(id: $id) {
                    id
                    title
                    content
                    icon
                    color
                    project {
                        id
                        name
                    }
                    creator {
                        id
                        name
                        email
                    }
                    updatedBy {
                        id
                        name
                    }
                    createdAt
                    updatedAt
                    url
                }
            }
        "#;

        let variables = serde_json::json!({ "id": doc_id });
        let duration_ms = start.elapsed().as_millis() as u64;

        match client.query(query, variables).await {
            Ok(data) => {
                if let Some(doc) = data.get("document") {
                    if doc.is_null() {
                        return Ok(ToolResult::failure(
                            format!("Document '{}' not found", doc_id),
                            duration_ms,
                        ));
                    }
                    info!(doc_id, "Retrieved Linear document");
                    let mut wrapped = doc.clone();
                    if let Some(obj) = wrapped.as_object_mut() {
                        if let Some(content) = obj.get("content").and_then(|c| c.as_str()) {
                            if !content.is_empty() {
                                let w = wrap_external_content(content, "Linear document content")
                                    .wrapped_content;
                                obj.insert("content".to_string(), Value::String(w));
                            }
                        }
                    }
                    Ok(ToolResult::success(
                        serde_json::json!({ "document": wrapped }),
                        duration_ms,
                    ))
                } else {
                    Ok(ToolResult::failure(
                        format!("Document '{}' not found", doc_id),
                        duration_ms,
                    ))
                }
            }
            Err(e) => {
                warn!(error = %e, doc_id, "Failed to get document");
                Ok(ToolResult::from_report(
                    &e.wrap_err("Failed to get document"),
                    duration_ms,
                ))
            }
        }
    }
}

// ── linear.document_list ────────────────────────────────────────────────

pub struct LinearDocumentListTool {
    clients: Arc<HashMap<String, Arc<LinearClient>>>,
    default_workspace: String,
    prefix_map: Arc<HashMap<String, String>>,
}

impl LinearDocumentListTool {
    pub fn new(
        clients: Arc<HashMap<String, Arc<LinearClient>>>,
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

#[async_trait]
impl Tool for LinearDocumentListTool {
    fn name(&self) -> &str {
        "linear.document_list"
    }

    fn description(&self) -> &str {
        "List documents, optionally filtered by project."
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "project_id": {
                    "type": "string",
                    "description": "Filter documents by project UUID string"
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum number of documents to return (integer, default: 50, max: 100)",
                    "minimum": 1,
                    "maximum": 100
                },
                "workspace": {
                    "type": "string",
                    "description": workspace_description(&self.clients, &self.default_workspace)
                }
            },
            "required": []
        })
    }

    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();

        let client = match resolve_client(
            &self.clients,
            &self.default_workspace,
            &self.prefix_map,
            &input,
        ) {
            Ok(c) => c,
            Err(e) => {
                return Ok(ToolResult::failure(
                    format!("{e}"),
                    start.elapsed().as_millis() as u64,
                ))
            }
        };

        let limit = coerce_to_u64(&input["limit"]).unwrap_or(50).min(100);

        let filter_clause = if let Some(project_id) = input["project_id"].as_str() {
            format!(
                r#"filter: {{ project: {{ id: {{ eq: "{}" }} }} }}"#,
                project_id
            )
        } else {
            String::new()
        };

        let query = format!(
            r#"
            query {{
                documents(first: {limit}, {filter_clause}) {{
                    nodes {{
                        id
                        title
                        icon
                        color
                        project {{
                            id
                            name
                        }}
                        creator {{
                            id
                            name
                        }}
                        createdAt
                        updatedAt
                        url
                    }}
                }}
            }}
            "#
        );

        let duration_ms = start.elapsed().as_millis() as u64;

        match client.query(&query, serde_json::json!({})).await {
            Ok(data) => {
                let documents = data
                    .get("documents")
                    .and_then(|d| d.get("nodes"))
                    .cloned()
                    .unwrap_or(serde_json::json!([]));
                let count = documents.as_array().map(|a| a.len()).unwrap_or(0);
                info!(count, "Listed Linear documents");
                Ok(ToolResult::success(
                    serde_json::json!({ "count": count, "documents": documents }),
                    duration_ms,
                ))
            }
            Err(e) => {
                warn!(error = %e, "Failed to list documents");
                Ok(ToolResult::from_report(
                    &e.wrap_err("Failed to list documents"),
                    duration_ms,
                ))
            }
        }
    }
}

// ── linear.document_create ────────────────────────────────────────────────

pub struct LinearDocumentCreateTool {
    clients: Arc<HashMap<String, Arc<LinearClient>>>,
    default_workspace: String,
    prefix_map: Arc<HashMap<String, String>>,
}

impl LinearDocumentCreateTool {
    pub fn new(
        clients: Arc<HashMap<String, Arc<LinearClient>>>,
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

#[async_trait]
impl Tool for LinearDocumentCreateTool {
    fn name(&self) -> &str {
        "linear.document_create"
    }

    fn description(&self) -> &str {
        "Create a new Linear document."
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "title": {
                    "type": "string",
                    "description": "Document title"
                },
                "content": {
                    "type": "string",
                    "description": "Document content in Markdown format"
                },
                "project_id": {
                    "type": "string",
                    "description": "Project UUID string to associate the document with"
                },
                "icon": {
                    "type": "string",
                    "description": "Document icon emoji or identifier"
                },
                "color": {
                    "type": "string",
                    "description": "Document color (hex code)"
                },
                "workspace": {
                    "type": "string",
                    "description": workspace_description(&self.clients, &self.default_workspace)
                }
            },
            "required": ["title"]
        })
    }

    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();

        let client = match resolve_client(
            &self.clients,
            &self.default_workspace,
            &self.prefix_map,
            &input,
        ) {
            Ok(c) => c,
            Err(e) => {
                return Ok(ToolResult::failure(
                    format!("{e}"),
                    start.elapsed().as_millis() as u64,
                ))
            }
        };

        let title = match input["title"].as_str() {
            Some(t) => t,
            None => {
                return Ok(ToolResult::failure(
                    "Missing required 'title' parameter",
                    start.elapsed().as_millis() as u64,
                ))
            }
        };

        let mut doc_input = serde_json::json!({ "title": title });

        if let Some(content) = input["content"].as_str() {
            doc_input["content"] = serde_json::json!(content);
        }
        if let Some(project_id) = input["project_id"].as_str() {
            doc_input["projectId"] = serde_json::json!(project_id);
        }
        if let Some(icon) = input["icon"].as_str() {
            doc_input["icon"] = serde_json::json!(icon);
        }
        if let Some(color) = input["color"].as_str() {
            doc_input["color"] = serde_json::json!(color);
        }

        let mutation = r#"
            mutation($input: DocumentCreateInput!) {
                documentCreate(input: $input) {
                    success
                    document {
                        id
                        title
                        url
                        project {
                            id
                            name
                        }
                        createdAt
                    }
                }
            }
        "#;

        let variables = serde_json::json!({ "input": doc_input });
        let duration_ms = start.elapsed().as_millis() as u64;

        match client.mutate(mutation, variables).await {
            Ok(data) => {
                if let Some(result) = data.get("documentCreate") {
                    let success = result["success"].as_bool().unwrap_or(false);
                    if success {
                        let doc = result
                            .get("document")
                            .cloned()
                            .unwrap_or(serde_json::json!(null));
                        info!(title, "Created Linear document");
                        Ok(ToolResult::success(
                            serde_json::json!({ "success": true, "document": doc }),
                            duration_ms,
                        ))
                    } else {
                        Ok(ToolResult::failure(
                            "Failed to create document",
                            duration_ms,
                        ))
                    }
                } else {
                    Ok(ToolResult::failure(
                        "Unexpected response format",
                        duration_ms,
                    ))
                }
            }
            Err(e) => {
                warn!(error = %e, title, "Failed to create document");
                Ok(ToolResult::from_report(
                    &e.wrap_err("Failed to create document"),
                    duration_ms,
                ))
            }
        }
    }
}

// ── linear.document_update ────────────────────────────────────────────────

pub struct LinearDocumentUpdateTool {
    clients: Arc<HashMap<String, Arc<LinearClient>>>,
    default_workspace: String,
    prefix_map: Arc<HashMap<String, String>>,
}

impl LinearDocumentUpdateTool {
    pub fn new(
        clients: Arc<HashMap<String, Arc<LinearClient>>>,
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

#[async_trait]
impl Tool for LinearDocumentUpdateTool {
    fn name(&self) -> &str {
        "linear.document_update"
    }

    fn description(&self) -> &str {
        "Update an existing Linear document."
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "id": {
                    "type": "string",
                    "description": "Document UUID to update"
                },
                "title": {
                    "type": "string",
                    "description": "New document title"
                },
                "content": {
                    "type": "string",
                    "description": "New document content in Markdown format"
                },
                "icon": {
                    "type": "string",
                    "description": "New document icon emoji or identifier"
                },
                "color": {
                    "type": "string",
                    "description": "New document color (hex code)"
                },
                "workspace": {
                    "type": "string",
                    "description": workspace_description(&self.clients, &self.default_workspace)
                }
            },
            "required": ["id"]
        })
    }

    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();

        let client = match resolve_client(
            &self.clients,
            &self.default_workspace,
            &self.prefix_map,
            &input,
        ) {
            Ok(c) => c,
            Err(e) => {
                return Ok(ToolResult::failure(
                    format!("{e}"),
                    start.elapsed().as_millis() as u64,
                ))
            }
        };

        let doc_id = match input["id"].as_str() {
            Some(id) => id,
            None => {
                return Ok(ToolResult::failure(
                    "Missing required 'id' parameter",
                    start.elapsed().as_millis() as u64,
                ))
            }
        };

        let mut update_input = serde_json::Map::new();

        if let Some(title) = input["title"].as_str() {
            update_input.insert("title".to_string(), serde_json::json!(title));
        }
        if let Some(content) = input["content"].as_str() {
            update_input.insert("content".to_string(), serde_json::json!(content));
        }
        if let Some(icon) = input["icon"].as_str() {
            update_input.insert("icon".to_string(), serde_json::json!(icon));
        }
        if let Some(color) = input["color"].as_str() {
            update_input.insert("color".to_string(), serde_json::json!(color));
        }

        if update_input.is_empty() {
            return Ok(ToolResult::failure(
                "No fields to update. Provide at least one of: title, content, icon, color",
                start.elapsed().as_millis() as u64,
            ));
        }

        let mutation = r#"
            mutation($id: String!, $input: DocumentUpdateInput!) {
                documentUpdate(id: $id, input: $input) {
                    success
                    document {
                        id
                        title
                        url
                        updatedAt
                    }
                }
            }
        "#;

        let variables = serde_json::json!({
            "id": doc_id,
            "input": Value::Object(update_input)
        });

        let duration_ms = start.elapsed().as_millis() as u64;

        match client.mutate(mutation, variables).await {
            Ok(data) => {
                if let Some(result) = data.get("documentUpdate") {
                    let success = result["success"].as_bool().unwrap_or(false);
                    if success {
                        let doc = result
                            .get("document")
                            .cloned()
                            .unwrap_or(serde_json::json!(null));
                        info!(doc_id, "Updated Linear document");
                        Ok(ToolResult::success(
                            serde_json::json!({ "success": true, "document": doc }),
                            duration_ms,
                        ))
                    } else {
                        Ok(ToolResult::failure(
                            "Failed to update document",
                            duration_ms,
                        ))
                    }
                } else {
                    Ok(ToolResult::failure(
                        "Unexpected response format",
                        duration_ms,
                    ))
                }
            }
            Err(e) => {
                warn!(error = %e, doc_id, "Failed to update document");
                Ok(ToolResult::from_report(
                    &e.wrap_err("Failed to update document"),
                    duration_ms,
                ))
            }
        }
    }
}

// ── linear.document_delete ────────────────────────────────────────────────

pub struct LinearDocumentDeleteTool {
    clients: Arc<HashMap<String, Arc<LinearClient>>>,
    default_workspace: String,
    prefix_map: Arc<HashMap<String, String>>,
}

impl LinearDocumentDeleteTool {
    pub fn new(
        clients: Arc<HashMap<String, Arc<LinearClient>>>,
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

#[async_trait]
impl Tool for LinearDocumentDeleteTool {
    fn name(&self) -> &str {
        "linear.document_delete"
    }

    fn description(&self) -> &str {
        "Delete a Linear document by its UUID."
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "id": {
                    "type": "string",
                    "description": "Document UUID to delete"
                },
                "workspace": {
                    "type": "string",
                    "description": workspace_description(&self.clients, &self.default_workspace)
                }
            },
            "required": ["id"]
        })
    }

    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();

        let client = match resolve_client(
            &self.clients,
            &self.default_workspace,
            &self.prefix_map,
            &input,
        ) {
            Ok(c) => c,
            Err(e) => {
                return Ok(ToolResult::failure(
                    format!("{e}"),
                    start.elapsed().as_millis() as u64,
                ))
            }
        };

        let doc_id = match input["id"].as_str() {
            Some(id) => id,
            None => {
                return Ok(ToolResult::failure(
                    "Missing required 'id' parameter",
                    start.elapsed().as_millis() as u64,
                ))
            }
        };

        let mutation = r#"
            mutation($id: String!) {
                documentDelete(id: $id) {
                    success
                }
            }
        "#;

        let variables = serde_json::json!({ "id": doc_id });
        let duration_ms = start.elapsed().as_millis() as u64;

        match client.mutate(mutation, variables).await {
            Ok(data) => {
                if let Some(result) = data.get("documentDelete") {
                    let success = result["success"].as_bool().unwrap_or(false);
                    if success {
                        info!(doc_id, "Deleted Linear document");
                        Ok(ToolResult::success(
                            serde_json::json!({ "success": true, "id": doc_id }),
                            duration_ms,
                        ))
                    } else {
                        Ok(ToolResult::failure(
                            "Failed to delete document",
                            duration_ms,
                        ))
                    }
                } else {
                    Ok(ToolResult::failure(
                        "Unexpected response format",
                        duration_ms,
                    ))
                }
            }
            Err(e) => {
                warn!(error = %e, doc_id, "Failed to delete document");
                Ok(ToolResult::from_report(
                    &e.wrap_err("Failed to delete document"),
                    duration_ms,
                ))
            }
        }
    }
}

// ── linear.document_search ────────────────────────────────────────────────

pub struct LinearDocumentSearchTool {
    clients: Arc<HashMap<String, Arc<LinearClient>>>,
    default_workspace: String,
    prefix_map: Arc<HashMap<String, String>>,
}

impl LinearDocumentSearchTool {
    pub fn new(
        clients: Arc<HashMap<String, Arc<LinearClient>>>,
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

#[async_trait]
impl Tool for LinearDocumentSearchTool {
    fn name(&self) -> &str {
        "linear.document_search"
    }

    fn description(&self) -> &str {
        "Search Linear documents by title."
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Search query to match against document titles"
                },
                "project_id": {
                    "type": "string",
                    "description": "Optional project UUID string to narrow search scope"
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum number of results (integer, default: 25, max: 100)",
                    "minimum": 1,
                    "maximum": 100
                },
                "workspace": {
                    "type": "string",
                    "description": workspace_description(&self.clients, &self.default_workspace)
                }
            },
            "required": ["query"]
        })
    }

    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();

        let client = match resolve_client(
            &self.clients,
            &self.default_workspace,
            &self.prefix_map,
            &input,
        ) {
            Ok(c) => c,
            Err(e) => {
                return Ok(ToolResult::failure(
                    format!("{e}"),
                    start.elapsed().as_millis() as u64,
                ))
            }
        };

        let search_query = match input["query"].as_str() {
            Some(q) => q,
            None => {
                return Ok(ToolResult::failure(
                    "Missing required 'query' parameter",
                    start.elapsed().as_millis() as u64,
                ))
            }
        };

        let limit = coerce_to_u64(&input["limit"]).unwrap_or(25).min(100);

        let mut filter_parts = vec![format!(
            r#"title: {{ containsIgnoreCase: "{}" }}"#,
            search_query.replace('"', r#"\""#)
        )];

        if let Some(project_id) = input["project_id"].as_str() {
            filter_parts.push(format!(r#"project: {{ id: {{ eq: "{}" }} }}"#, project_id));
        }

        let filter_clause = format!("filter: {{ {} }}", filter_parts.join(", "));

        let query = format!(
            r#"
            query {{
                documents(first: {limit}, {filter_clause}) {{
                    nodes {{
                        id
                        title
                        icon
                        color
                        project {{
                            id
                            name
                        }}
                        creator {{
                            id
                            name
                        }}
                        createdAt
                        updatedAt
                        url
                    }}
                }}
            }}
            "#
        );

        let duration_ms = start.elapsed().as_millis() as u64;

        match client.query(&query, serde_json::json!({})).await {
            Ok(data) => {
                let documents = data
                    .get("documents")
                    .and_then(|d| d.get("nodes"))
                    .cloned()
                    .unwrap_or(serde_json::json!([]));
                let count = documents.as_array().map(|a| a.len()).unwrap_or(0);
                info!(query = %search_query, count, "Searched Linear documents");
                Ok(ToolResult::success(
                    serde_json::json!({ "query": search_query, "count": count, "documents": documents }),
                    duration_ms,
                ))
            }
            Err(e) => {
                warn!(error = %e, query = %search_query, "Failed to search documents");
                Ok(ToolResult::from_report(
                    &e.wrap_err("Failed to search documents"),
                    duration_ms,
                ))
            }
        }
    }
}

// ── linear.issue_children ────────────────────────────────────────────────

pub struct LinearIssueChildrenTool {
    clients: Arc<HashMap<String, Arc<LinearClient>>>,
    default_workspace: String,
    prefix_map: Arc<HashMap<String, String>>,
}

impl LinearIssueChildrenTool {
    pub fn new(
        clients: Arc<HashMap<String, Arc<LinearClient>>>,
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

#[async_trait]
impl Tool for LinearIssueChildrenTool {
    fn name(&self) -> &str {
        "linear.issue_children"
    }

    fn description(&self) -> &str {
        "List child (sub) issues of a parent issue."
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "identifier": {
                    "type": "string",
                    "description": "Parent issue identifier string like 'DW-107'"
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum number of children (integer, default: 50, max: 100)",
                    "minimum": 1,
                    "maximum": 100
                },
                "workspace": {
                    "type": "string",
                    "description": workspace_description(&self.clients, &self.default_workspace)
                }
            },
            "required": ["identifier"]
        })
    }

    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();

        let client = match resolve_client(
            &self.clients,
            &self.default_workspace,
            &self.prefix_map,
            &input,
        ) {
            Ok(c) => c,
            Err(e) => {
                return Ok(ToolResult::failure(
                    format!("{e}"),
                    start.elapsed().as_millis() as u64,
                ))
            }
        };

        let identifier = match input["identifier"].as_str() {
            Some(id) => id,
            None => {
                return Ok(ToolResult::failure(
                    "Missing required 'identifier' parameter",
                    start.elapsed().as_millis() as u64,
                ))
            }
        };

        let limit = coerce_to_u64(&input["limit"]).unwrap_or(50).min(100);

        let query = r#"
            query($id: String!, $first: Int) {
                issue(id: $id) {
                    id
                    identifier
                    title
                    children(first: $first) {
                        nodes {
                            id
                            identifier
                            title
                            description
                            state {
                                name
                            }
                            priority
                            assignee {
                                id
                                name
                            }
                            project {
                                id
                                name
                            }
                            createdAt
                            updatedAt
                        }
                    }
                }
            }
        "#;

        let variables = serde_json::json!({
            "id": identifier,
            "first": limit
        });

        let duration_ms = start.elapsed().as_millis() as u64;

        match client.query(query, variables).await {
            Ok(data) => {
                if let Some(issue) = data.get("issue") {
                    if issue.is_null() {
                        return Ok(ToolResult::failure(
                            format!("Issue '{}' not found", identifier),
                            duration_ms,
                        ));
                    }
                    let children = issue
                        .get("children")
                        .and_then(|c| c.get("nodes"))
                        .cloned()
                        .unwrap_or(serde_json::json!([]));
                    let count = children.as_array().map(|a| a.len()).unwrap_or(0);
                    // Wrap description fields in child issues
                    let wrapped_children: Vec<Value> = children
                        .as_array()
                        .map(|arr| {
                            arr.iter()
                                .map(|c| wrap_description_field(c, "Linear issue description"))
                                .collect()
                        })
                        .unwrap_or_default();
                    info!(identifier, count, "Listed children for issue");
                    Ok(ToolResult::success(
                        serde_json::json!({
                            "identifier": identifier,
                            "parent_title": issue["title"].as_str().unwrap_or(""),
                            "count": count,
                            "children": wrapped_children
                        }),
                        duration_ms,
                    ))
                } else {
                    Ok(ToolResult::failure(
                        format!("Issue '{}' not found", identifier),
                        duration_ms,
                    ))
                }
            }
            Err(e) => {
                warn!(error = %e, identifier, "Failed to list children");
                Ok(ToolResult::from_report(
                    &e.wrap_err("Failed to list children"),
                    duration_ms,
                ))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_description_with_newlines_written_out_gets_real_ones() {
        assert_eq!(
            unescape_newlines("**A:**\\nb\\n\\n* [ ] c"),
            "**A:**\nb\n\n* [ ] c"
        );
        assert_eq!(unescape_newlines("a\nb \\n c"), "a\nb \\n c");
    }

    fn fixture() -> (HashMap<String, Arc<LinearClient>>, HashMap<String, String>) {
        let clients = ["main", "staging"]
            .into_iter()
            .map(|n| {
                (
                    n.to_string(),
                    Arc::new(LinearClient::new("key", "https://api.linear.app").unwrap()),
                )
            })
            .collect();
        let prefix = [("OPS", "staging"), ("MAIN", "main")]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        (clients, prefix)
    }

    /// A file is read only when its issue shows or lists it.
    #[test]
    fn an_upload_is_its_issue_s_only_when_it_shows_or_lists_it() {
        let shown = "https://uploads.linear.app/a/b/shot.png";
        let issue = serde_json::json!({
            "description": format!("Steps:\n![the page]({shown})\nSee [spec](https://uploads.linear.app/a/c/spec.pdf) too"),
            "attachments": { "nodes": [ { "url": "https://uploads.linear.app/a/d/log.txt" } ] },
        });
        assert!(issue_lists(&issue, shown));
        assert!(issue_lists(
            &issue,
            "https://uploads.linear.app/a/c/spec.pdf"
        ));
        assert!(issue_lists(
            &issue,
            "https://uploads.linear.app/a/d/log.txt"
        ));
        for other in [
            "https://uploads.linear.app/a/b/other.png",
            "https://uploads.linear.app/a/b/shot.pn",
            "https://uploads.linear.app/a/b",
        ] {
            assert!(!issue_lists(&issue, other), "{other}");
        }
        assert!(
            !issue_lists(&serde_json::Value::Null, shown),
            "no issue, no file"
        );
        assert!(identifier_ok("ENG-12"));
        for bad in [
            "eng-12",
            "ENG-",
            "-12",
            "ENG-12/x",
            "ENG-12\"",
            "TOOLONGTEAMKEY-1",
        ] {
            assert!(!identifier_ok(bad), "{bad}");
        }
    }

    /// An upload walked past what its issue shows is refused before anything
    /// is downloaded, and none is read without its issue.
    #[tokio::test]
    async fn upload_read_reads_only_what_its_issue_shows() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": { "issue": {
                    "description": "![shot](https://uploads.linear.app/a/b/shot.png)",
                    "attachments": { "nodes": [] }
                } }
            })))
            .mount(&server)
            .await;
        let clients: HashMap<String, Arc<LinearClient>> = [(
            "main".to_string(),
            Arc::new(LinearClient::new("key", &server.uri()).unwrap()),
        )]
        .into();
        let tool = LinearUploadReadTool::new(Arc::new(clients), "main".into(), Arc::default());
        let walked = tool
            .execute(serde_json::json!({ "identifier": "ENG-1", "url": "https://uploads.linear.app/a/b/someone-elses.png" }))
            .await
            .unwrap();
        assert!(!walked.success);
        assert!(walked
            .error
            .unwrap_or_default()
            .contains("doesn't show or list that file"));
        let unnamed = tool
            .execute(serde_json::json!({ "url": "https://uploads.linear.app/a/b/shot.png" }))
            .await
            .unwrap();
        assert!(!unnamed.success, "never without its issue");
    }

    /// Linear's key goes only to Linear's own uploads: any other address is
    /// refused before a request is made.
    #[tokio::test]
    async fn upload_read_refuses_any_address_but_linear_s_uploads() {
        let (clients, prefix) = fixture();
        let tool = LinearUploadReadTool::new(Arc::new(clients), "main".into(), Arc::new(prefix));
        for url in [
            "https://evil.example/uploads.linear.app/x.png",
            "http://uploads.linear.app/x.png",
            "https://uploads.linear.app.evil.example/x.png",
            "https://key@uploads.linear.app/x.png",
        ] {
            let r = tool
                .execute(serde_json::json!({ "identifier": "MAIN-1", "url": url }))
                .await
                .unwrap();
            assert!(!r.success, "{url}");
        }
        assert_eq!(
            upload_type("https://uploads.linear.app/a/b/Shot.PNG?x=1"),
            "image/png"
        );
        assert_eq!(
            upload_type("https://uploads.linear.app/a/b/notes"),
            "application/octet-stream"
        );
    }

    #[test]
    fn resolve_client_under_environment_lock() {
        use workforce_agent::runner::{ACTIVE_ENVIRONMENT, ACTIVE_SKILL_INSTANCES};
        let (clients, prefix) = fixture();
        let pins: workforce_agent::llm::environment::SkillPins =
            [("linear".to_string(), vec!["main".to_string()])].into();
        ACTIVE_ENVIRONMENT.sync_scope("harbor-prod".to_string(), || {
            ACTIVE_SKILL_INSTANCES.sync_scope(pins, || {
                let main = clients.get("main").unwrap();
                // No identifier → the pin (main), overriding the "staging" default.
                let c =
                    resolve_client(&clients, "staging", &prefix, &serde_json::json!({})).unwrap();
                assert!(Arc::ptr_eq(&c, main));
                // An identifier routing to the SAME workspace as the pin resolves.
                let c = resolve_client(
                    &clients,
                    "staging",
                    &prefix,
                    &serde_json::json!({ "identifier": "MAIN-1" }),
                )
                .unwrap();
                assert!(Arc::ptr_eq(&c, main));
                // An identifier auto-routing to a DIFFERENT workspace is refused.
                assert!(resolve_client(
                    &clients,
                    "staging",
                    &prefix,
                    &serde_json::json!({ "identifier": "OPS-7" })
                )
                .is_err());
                // An explicit mismatching workspace is refused.
                assert!(resolve_client(
                    &clients,
                    "staging",
                    &prefix,
                    &serde_json::json!({ "workspace": "staging" })
                )
                .is_err());
            });
        });
    }

    #[test]
    fn a_bound_set_lets_an_identifier_route_to_any_member() {
        use workforce_agent::runner::{ACTIVE_ENVIRONMENT, ACTIVE_SKILL_INSTANCES};
        let (mut clients, mut prefix) = fixture();
        clients.insert(
            "fabrikam-crm".to_string(),
            Arc::new(LinearClient::new("key", "https://api.linear.app").unwrap()),
        );
        prefix.insert("FAB".to_string(), "fabrikam-crm".to_string());
        let pins: workforce_agent::llm::environment::SkillPins = [(
            "linear".to_string(),
            vec!["main".to_string(), "fabrikam-crm".to_string()],
        )]
        .into();
        ACTIVE_ENVIRONMENT.sync_scope("driftwerk".to_string(), || {
            ACTIVE_SKILL_INSTANCES.sync_scope(pins, || {
                let fabrikam = clients.get("fabrikam-crm").unwrap();
                let c = resolve_client(
                    &clients,
                    "main",
                    &prefix,
                    &serde_json::json!({ "identifier": "FAB-123" }),
                )
                .unwrap();
                assert!(Arc::ptr_eq(&c, fabrikam));
                let c =
                    resolve_client(&clients, "staging", &prefix, &serde_json::json!({})).unwrap();
                assert!(Arc::ptr_eq(&c, clients.get("main").unwrap()));
                assert!(resolve_client(
                    &clients,
                    "main",
                    &prefix,
                    &serde_json::json!({ "identifier": "OPS-7" })
                )
                .is_err());
            });
        });
    }
}
