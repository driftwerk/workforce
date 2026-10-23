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

        let filter_clause = if filters.is_empty() {
            String::new()
        } else {
            format!("filter: {{ {} }}", filters.join(", "))
        };

        let query = format!(
            r#"
            query {{
                issues(first: {}, {}) {{
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
                        createdAt
                        updatedAt
                        url
                    }}
                }}
            }}
            "#,
            limit, filter_clause
        );

        let duration_ms = start.elapsed().as_millis() as u64;

        match client.query(&query, serde_json::json!({})).await {
            Ok(data) => {
                let issues = data
                    .get("issues")
                    .and_then(|i| i.get("nodes"))
                    .cloned()
                    .unwrap_or(serde_json::json!([]));
                let count = issues.as_array().map(|a| a.len()).unwrap_or(0);
                info!(count, "Listed Linear issues");
                Ok(ToolResult::success(
                    serde_json::json!({ "count": count, "issues": issues }),
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
            issue_input["description"] = serde_json::json!(desc);
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
            update_input.insert("description".to_string(), serde_json::json!(desc));
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
                users(first: $first) {
                    nodes {
                        id
                        name
                        email
                        displayName
                        active
                    }
                }
            }
        "#;

        let variables = serde_json::json!({ "first": limit });
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
