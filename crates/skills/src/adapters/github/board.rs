//! GitHub tools behind Desk's board sync: Projects (v2) read and arranged
//! through GraphQL, and the issues, labels and milestones a board shows.
//!
//! The answer shapes are a contract: Desk's board sync parses them, so keys
//! are never renamed here without a change there.

use std::sync::Arc;

use async_trait::async_trait;
use eyre::{eyre, Result};
use serde_json::{json, Map, Value};
use tracing::info;

use workforce_agent::tools::{Tool, ToolResult};

use crate::adapters::utils::{coerce_to_u64, LiveInstances};

use super::client::GithubClient;
use super::tools::{caller_owner, owner_description, resolve_client, resolve_owner, wrap_field};

/// The most items asked of a project in one page.
const ITEMS_PAGE: usize = 100;
/// The most ids `project_items` reads by id.
const MOST_IDS: usize = 50;

/// What GraphQL says of one issue (the `ISSUE` shape of the contract).
const ISSUE_FIELDS: &str = "id number url title body state stateReason createdAt updatedAt \
    author { login } \
    labels(first: 20) { nodes { name } } \
    assignees(first: 10) { nodes { login name } } \
    milestone { title number dueOn } \
    issueType { name } \
    parent { number title repository { nameWithOwner } } \
    repository { nameWithOwner }";

/// What a lean project item says of its content.
const LEAN_CONTENT: &str = "... on Issue { number updatedAt repository { nameWithOwner } } \
    ... on PullRequest { number updatedAt repository { nameWithOwner } }";

fn item_fields(full: bool) -> String {
    if full {
        format!(
            "id type updatedAt \
             fieldValueByName(name: $field) {{ ... on ProjectV2ItemFieldSingleSelectValue {{ name }} }} \
             content {{ ... on Issue {{ {ISSUE_FIELDS} }} \
             ... on PullRequest {{ number updatedAt repository {{ nameWithOwner }} }} }}"
        )
    } else {
        format!("id type updatedAt content {{ {LEAN_CONTENT} }}")
    }
}

// ── shapes ───────────────────────────────────────────────────────

fn names(nodes: &Value, key: &str) -> Vec<Value> {
    nodes
        .as_array()
        .map(|a| a.iter().filter_map(|n| n.get(key).cloned()).collect())
        .unwrap_or_default()
}

/// The `ISSUE` shape from GraphQL's `Issue`.
fn issue_from_graphql(n: &Value) -> Value {
    let milestone = match &n["milestone"] {
        m if m.is_object() => json!({
            "title": m["title"], "number": m["number"], "due_on": m["dueOn"],
        }),
        _ => Value::Null,
    };
    let parent = match &n["parent"] {
        p if p.is_object() => json!({
            "repo": p["repository"]["nameWithOwner"],
            "number": p["number"],
            "title": p["title"],
        }),
        _ => Value::Null,
    };
    let assignees: Vec<Value> = n["assignees"]["nodes"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|u| json!({ "login": u["login"], "name": u["name"] }))
                .collect()
        })
        .unwrap_or_default();
    json!({
        "repo": n["repository"]["nameWithOwner"],
        "number": n["number"],
        "issue_id": n["id"],
        "url": n["url"],
        "title": n["title"],
        "body": wrap_field(&n["body"], "GitHub issue body"),
        "state": n["state"],
        "state_reason": n["stateReason"],
        "labels": names(&n["labels"]["nodes"], "name"),
        "assignees": assignees,
        "milestone": milestone,
        "issue_type": n["issueType"]["name"],
        "parent": parent,
        "author": n["author"]["login"],
        "created_at": n["createdAt"],
        "updated_at": n["updatedAt"],
    })
}

fn upper(v: &Value) -> Value {
    v.as_str()
        .map_or(Value::Null, |s| Value::String(s.to_uppercase()))
}

/// The `ISSUE` shape from REST's issue. REST names no repository on an
/// issue it lists, so `repo` is the one asked of.
fn issue_from_rest(v: &Value, repo: &str) -> Value {
    let labels: Vec<Value> = v["labels"]
        .as_array()
        .map(|a| {
            a.iter()
                .take(20)
                .filter_map(|l| l.get("name").or(Some(l)).filter(|n| n.is_string()).cloned())
                .collect()
        })
        .unwrap_or_default();
    let assignees: Vec<Value> = v["assignees"]
        .as_array()
        .map(|a| {
            a.iter()
                .take(10)
                .map(|u| json!({ "login": u["login"], "name": u["name"] }))
                .collect()
        })
        .unwrap_or_default();
    let milestone = match &v["milestone"] {
        m if m.is_object() => json!({
            "title": m["title"], "number": m["number"], "due_on": m["due_on"],
        }),
        _ => Value::Null,
    };
    json!({
        "repo": repo,
        "number": v["number"],
        "issue_id": v["node_id"],
        "url": v["html_url"],
        "title": v["title"],
        "body": wrap_field(&v["body"], "GitHub issue body"),
        "state": upper(&v["state"]),
        "state_reason": upper(&v["state_reason"]),
        "labels": labels,
        "assignees": assignees,
        "milestone": milestone,
        "issue_type": v["type"]["name"],
        "parent": Value::Null,
        "author": v["user"]["login"],
        "created_at": v["created_at"],
        "updated_at": v["updated_at"],
    })
}

/// The later of two RFC 3339 times, as the text GitHub gave.
fn later(a: &Value, b: &Value) -> Value {
    let (Some(x), Some(y)) = (a.as_str(), b.as_str()) else {
        return if a.is_string() { a.clone() } else { b.clone() };
    };
    let parse = |s: &str| chrono::DateTime::parse_from_rfc3339(s).ok();
    let y_is_later = match (parse(x), parse(y)) {
        (Some(px), Some(py)) => py > px,
        _ => y > x,
    };
    Value::String(if y_is_later { y } else { x }.to_string())
}

/// The `ITEM` shape; `None` for a node that isn't an item (an id that no
/// longer is one).
fn item_from_graphql(node: &Value, full: bool) -> Option<Value> {
    let item_id = node["id"].as_str()?;
    let kind = node["type"].as_str().unwrap_or("REDACTED");
    let content = &node["content"];
    let updated = later(&node["updatedAt"], &content["updatedAt"]);
    let mut item = if full && kind == "ISSUE" && content["id"].is_string() {
        let mut issue = issue_from_graphql(content);
        issue["status"] = node["fieldValueByName"]["name"].clone();
        issue
    } else {
        json!({
            "repo": content["repository"]["nameWithOwner"],
            "number": content["number"],
        })
    };
    let map = item.as_object_mut()?;
    map.insert("item_id".into(), json!(item_id));
    map.insert("type".into(), json!(kind));
    map.insert("updated_at".into(), updated);
    Some(item)
}

fn project_summary(p: &Value) -> Value {
    json!({
        "id": p["id"], "number": p["number"], "title": p["title"],
        "url": p["url"], "closed": p["closed"],
    })
}

fn field_shape(f: &Value) -> Option<Value> {
    f["id"].as_str()?;
    let options = match f["options"].as_array() {
        Some(o) => Value::Array(
            o.iter()
                .map(|o| json!({ "id": o["id"], "name": o["name"] }))
                .collect(),
        ),
        None => Value::Null,
    };
    Some(json!({ "id": f["id"], "name": f["name"], "options": options }))
}

const FIELDS: &str = "fields(first: 50) { nodes { \
    ... on ProjectV2FieldCommon { id name } \
    ... on ProjectV2SingleSelectField { options { id name } } } }";

/// The single-select field called `name` (case-insensitively) among a
/// project's field shapes.
fn single_select<'a>(fields: &'a [Value], name: &str) -> Option<&'a Value> {
    fields.iter().find(|f| {
        f["options"].is_array()
            && f["name"]
                .as_str()
                .is_some_and(|n| n.eq_ignore_ascii_case(name))
    })
}

async fn project_fields(client: &GithubClient, project_id: &str) -> Result<Vec<Value>> {
    let data = client
        .graphql(
            &format!(
                "query($project: ID!) {{ node(id: $project) {{ ... on ProjectV2 {{ {FIELDS} }} }} }}"
            ),
            json!({ "project": project_id }),
        )
        .await?;
    let nodes = data["node"]["fields"]["nodes"]
        .as_array()
        .ok_or_else(|| eyre!("No project {project_id}"))?;
    Ok(nodes.iter().filter_map(field_shape).collect())
}

/// A path segment, percent-encoded.
fn segment(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

// ── tool plumbing ────────────────────────────────────────────────

macro_rules! board_tool {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        pub struct $name {
            clients: Arc<LiveInstances<GithubClient>>,
            default_workspace: String,
            default_owner: Option<String>,
        }

        impl $name {
            pub fn new(
                clients: Arc<LiveInstances<GithubClient>>,
                default_workspace: String,
                default_owner: Option<String>,
            ) -> Self {
                Self { clients, default_workspace, default_owner }
            }

            async fn client(&self, input: &Value) -> Result<Arc<GithubClient>> {
                resolve_client(&self.clients, &self.default_workspace, input).await
            }

            #[allow(dead_code)]
            fn owner(&self, input: &Value) -> Result<String> {
                resolve_owner(input, &caller_owner(&self.clients, &self.default_owner))
                    .map_err(|e| eyre!(e))
            }

            #[allow(dead_code)]
            fn owner_prop(&self) -> Value {
                json!({ "type": "string",
                    "description": owner_description(&self.clients, &self.default_workspace) })
            }
        }
    };
}

fn workspace_prop() -> Value {
    json!({ "type": "string", "description": "GitHub connection to use; omit for the default" })
}

fn finish(start: std::time::Instant, out: Result<Value>) -> ToolResult {
    let ms = start.elapsed().as_millis() as u64;
    match out {
        Ok(v) => ToolResult::success(v, ms),
        Err(e) => ToolResult::from_report(&e, ms),
    }
}

fn req_str<'a>(input: &'a Value, key: &str) -> Result<&'a str> {
    input[key]
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| eyre!("Missing required '{key}' parameter"))
}

fn req_num(input: &Value, key: &str) -> Result<u64> {
    coerce_to_u64(&input[key]).ok_or_else(|| eyre!("Missing required '{key}' parameter"))
}

fn str_list(input: &Value, key: &str) -> Result<Vec<String>> {
    match &input[key] {
        Value::Null => Ok(Vec::new()),
        Value::Array(a) => a
            .iter()
            .map(|v| {
                v.as_str()
                    .map(str::to_string)
                    .ok_or_else(|| eyre!("'{key}' must be a list of strings"))
            })
            .collect(),
        _ => Err(eyre!("'{key}' must be a list of strings")),
    }
}

// ── github.viewer ────────────────────────────────────────────────

board_tool!(
    /// `github.viewer`: who the connection's token is.
    GithubViewerTool
);

#[async_trait]
impl Tool for GithubViewerTool {
    fn name(&self) -> &str {
        "github.viewer"
    }

    fn description(&self) -> &str {
        "Who the GitHub connection's token belongs to (login, name, node id) and whether it is the caller's own token (personal) rather than a shared one."
    }

    fn input_schema(&self) -> Value {
        json!({ "type": "object", "properties": { "workspace": workspace_prop() } })
    }

    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();
        let out = async {
            let client = self.client(&input).await?;
            let data = client
                .graphql("query { viewer { login name id } }", json!({}))
                .await?;
            let v = &data["viewer"];
            if !v.is_object() {
                return Err(eyre!("GitHub did not say who the token is"));
            }
            Ok(json!({
                "login": v["login"], "name": v["name"], "id": v["id"],
                "personal": client.token_is_personal(),
            }))
        }
        .await;
        Ok(finish(start, out))
    }
}

// ── github.projects_list ─────────────────────────────────────────

board_tool!(
    /// `github.projects_list`: an owner's Projects, newest activity first.
    GithubProjectsListTool
);

#[async_trait]
impl Tool for GithubProjectsListTool {
    fn name(&self) -> &str {
        "github.projects_list"
    }

    fn description(&self) -> &str {
        "List the GitHub Projects (v2) of a user or organisation, most recently updated first (id, number, title, url, closed)."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "owner": self.owner_prop(), "workspace": workspace_prop() }
        })
    }

    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();
        let out = async {
            let owner = self.owner(&input)?;
            let client = self.client(&input).await?;
            let data = client
                .graphql(
                    "query($owner: String!) { repositoryOwner(login: $owner) { \
                     ... on ProjectV2Owner { projectsV2(first: 50, orderBy: {field: UPDATED_AT, direction: DESC}) { \
                     nodes { id number title url closed } } } } }",
                    json!({ "owner": owner }),
                )
                .await?;
            if data["repositoryOwner"].is_null() {
                return Err(eyre!("No GitHub user or organisation \"{owner}\""));
            }
            let projects: Vec<Value> = data["repositoryOwner"]["projectsV2"]["nodes"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter(|p| p.is_object())
                        .map(project_summary)
                        .collect()
                })
                .unwrap_or_default();
            Ok(json!({ "owner": owner, "projects": projects }))
        }
        .await;
        Ok(finish(start, out))
    }
}

// ── github.project_get ───────────────────────────────────────────

board_tool!(
    /// `github.project_get`: one Project, its fields and its Status options.
    GithubProjectGetTool
);

#[async_trait]
impl Tool for GithubProjectGetTool {
    fn name(&self) -> &str {
        "github.project_get"
    }

    fn description(&self) -> &str {
        "Get one GitHub Project (v2) by owner and number: its id, fields (single-select ones with their options) and its Status field."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "owner": self.owner_prop(),
                "number": { "type": "integer", "description": "The project's number (integer)" },
                "workspace": workspace_prop()
            },
            "required": ["number"]
        })
    }

    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();
        let out = async {
            let owner = self.owner(&input)?;
            let number = req_num(&input, "number")?;
            let client = self.client(&input).await?;
            let data = client
                .graphql(
                    &format!(
                        "query($owner: String!, $number: Int!) {{ repositoryOwner(login: $owner) {{ \
                         ... on ProjectV2Owner {{ projectV2(number: $number) {{ \
                         id number title url closed {FIELDS} }} }} }} }}"
                    ),
                    json!({ "owner": owner, "number": number }),
                )
                .await?;
            let project = &data["repositoryOwner"]["projectV2"];
            if !project.is_object() {
                return Err(eyre!("No project {number} for \"{owner}\""));
            }
            let fields: Vec<Value> = project["fields"]["nodes"]
                .as_array()
                .map(|a| a.iter().filter_map(field_shape).collect())
                .unwrap_or_default();
            let status = single_select(&fields, "Status").map(|f| {
                json!({ "field_id": f["id"], "name": f["name"], "options": f["options"] })
            });
            let mut out = project_summary(project);
            out["owner"] = json!(owner);
            out["fields"] = json!(fields);
            out["status"] = status.unwrap_or(Value::Null);
            Ok(out)
        }
        .await;
        Ok(finish(start, out))
    }
}

// ── github.project_items ─────────────────────────────────────────

board_tool!(
    /// `github.project_items`: a Project's items, a page at a time or by id.
    GithubProjectItemsTool
);

#[async_trait]
impl Tool for GithubProjectItemsTool {
    fn name(&self) -> &str {
        "github.project_items"
    }

    fn description(&self) -> &str {
        "List the items of a GitHub Project (v2) in board order, 100 a page. Lean (id, type, updated_at, repo, number) unless full is true, which adds the issue and its Status. With ids, reads those items in full instead."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "project_id": { "type": "string", "description": "The project's node id (from github.project_get)" },
                "query": { "type": "string", "description": "A project filter, e.g. 'updated:>=2026-01-01'" },
                "after": { "type": "string", "description": "end_cursor of the previous page" },
                "full": { "type": "boolean", "description": "Read each issue in full, with its Status" },
                "ids": { "type": "array", "items": { "type": "string" }, "description": "Item ids to read (1 to 50); always full" },
                "field": { "type": "string", "description": "The single-select field read as status (default Status)" },
                "workspace": workspace_prop()
            },
            "required": ["project_id"]
        })
    }

    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();
        let out = async {
            let project = req_str(&input, "project_id")?;
            let field = input["field"].as_str().unwrap_or("Status");
            let client = self.client(&input).await?;
            if !input["ids"].is_null() {
                let ids = str_list(&input, "ids")?;
                if ids.is_empty() || ids.len() > MOST_IDS {
                    return Err(eyre!("'ids' takes 1 to {MOST_IDS} item ids"));
                }
                let data = client
                    .graphql(
                        &format!(
                            "query($ids: [ID!]!, $field: String!) {{ nodes(ids: $ids) {{ \
                             ... on ProjectV2Item {{ {} }} }} }}",
                            item_fields(true)
                        ),
                        json!({ "ids": ids, "field": field }),
                    )
                    .await?;
                let items: Vec<Value> = data["nodes"]
                    .as_array()
                    .map(|a| a.iter().filter_map(|n| item_from_graphql(n, true)).collect())
                    .unwrap_or_default();
                return Ok(json!({ "items": items }));
            }
            let full = input["full"].as_bool().unwrap_or(false);
            let mut variables = Map::new();
            variables.insert("project".into(), json!(project));
            if let Some(after) = input["after"].as_str().filter(|a| !a.is_empty()) {
                variables.insert("after".into(), json!(after));
            }
            if let Some(q) = input["query"].as_str().filter(|q| !q.is_empty()) {
                variables.insert("query".into(), json!(q));
            }
            let field_var = if full {
                variables.insert("field".into(), json!(field));
                ", $field: String!"
            } else {
                ""
            };
            let data = client
                .graphql(
                    &format!(
                        "query($project: ID!, $after: String, $query: String{field_var}) {{ \
                         node(id: $project) {{ ... on ProjectV2 {{ \
                         items(first: {ITEMS_PAGE}, after: $after, query: $query, orderBy: {{field: POSITION, direction: ASC}}) {{ \
                         pageInfo {{ hasNextPage endCursor }} nodes {{ {} }} }} }} }} }}",
                        item_fields(full)
                    ),
                    Value::Object(variables),
                )
                .await?;
            let items = &data["node"]["items"];
            if !items.is_object() {
                return Err(eyre!("No project {project}"));
            }
            let nodes: Vec<Value> = items["nodes"]
                .as_array()
                .map(|a| a.iter().filter_map(|n| item_from_graphql(n, full)).collect())
                .unwrap_or_default();
            Ok(json!({
                "items": nodes,
                "page_info": {
                    "has_next_page": items["pageInfo"]["hasNextPage"].as_bool().unwrap_or(false),
                    "end_cursor": items["pageInfo"]["endCursor"],
                },
            }))
        }
        .await;
        Ok(finish(start, out))
    }
}

// ── github.project_item_status ───────────────────────────────────

board_tool!(
    /// `github.project_item_status`: set an item's single-select column.
    GithubProjectItemStatusTool
);

#[async_trait]
impl Tool for GithubProjectItemStatusTool {
    fn name(&self) -> &str {
        "github.project_item_status"
    }

    fn description(&self) -> &str {
        "Move a GitHub Project (v2) item to a column: set its Status (or another single-select field) to the option with that name, matched case-insensitively."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "project_id": { "type": "string", "description": "The project's node id" },
                "item_id": { "type": "string", "description": "The item's id" },
                "status": { "type": "string", "description": "The option's name, e.g. 'In progress'" },
                "field": { "type": "string", "description": "The single-select field (default Status)" },
                "workspace": workspace_prop()
            },
            "required": ["project_id", "item_id", "status"]
        })
    }

    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();
        let out = async {
            let project = req_str(&input, "project_id")?;
            let item = req_str(&input, "item_id")?;
            let status = req_str(&input, "status")?;
            let field_name = input["field"].as_str().unwrap_or("Status");
            let client = self.client(&input).await?;
            let fields = project_fields(&client, project).await?;
            let field = single_select(&fields, field_name).ok_or_else(|| {
                eyre!("This project has no single-select field \"{field_name}\"")
            })?;
            let option = field["options"]
                .as_array()
                .and_then(|o| {
                    o.iter().find(|o| {
                        o["name"]
                            .as_str()
                            .is_some_and(|n| n.trim().eq_ignore_ascii_case(status.trim()))
                    })
                })
                .ok_or_else(|| eyre!("No {field_name} option \"{status}\" on this project"))?;
            client
                .graphql(
                    "mutation($project: ID!, $item: ID!, $field: ID!, $option: String!) { \
                     updateProjectV2ItemFieldValue(input: {projectId: $project, itemId: $item, \
                     fieldId: $field, value: {singleSelectOptionId: $option}}) { projectV2Item { id } } }",
                    json!({
                        "project": project, "item": item,
                        "field": field["id"], "option": option["id"],
                    }),
                )
                .await?;
            info!(item = %item, "Set a project item's status");
            Ok(json!({ "item_id": item, "status": option["name"] }))
        }
        .await;
        Ok(finish(start, out))
    }
}

// ── github.project_item_move ─────────────────────────────────────

board_tool!(
    /// `github.project_item_move`: reorder an item within its project.
    GithubProjectItemMoveTool
);

#[async_trait]
impl Tool for GithubProjectItemMoveTool {
    fn name(&self) -> &str {
        "github.project_item_move"
    }

    fn description(&self) -> &str {
        "Reorder a GitHub Project (v2) item: place it after another item, or at the top when after_id is omitted or null."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "project_id": { "type": "string", "description": "The project's node id" },
                "item_id": { "type": "string", "description": "The item to move" },
                "after_id": { "type": ["string", "null"], "description": "The item it goes after; null or omitted puts it at the top" },
                "workspace": workspace_prop()
            },
            "required": ["project_id", "item_id"]
        })
    }

    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();
        let out = async {
            let project = req_str(&input, "project_id")?;
            let item = req_str(&input, "item_id")?;
            let after = input["after_id"].as_str().filter(|a| !a.is_empty());
            let client = self.client(&input).await?;
            client
                .graphql(
                    "mutation($project: ID!, $item: ID!, $after: ID) { \
                     updateProjectV2ItemPosition(input: {projectId: $project, itemId: $item, afterId: $after}) { \
                     clientMutationId } }",
                    json!({ "project": project, "item": item, "after": after }),
                )
                .await?;
            Ok(json!({ "moved": true, "item_id": item }))
        }
        .await;
        Ok(finish(start, out))
    }
}

// ── github.project_item_add ──────────────────────────────────────

board_tool!(
    /// `github.project_item_add`: put an issue on a project.
    GithubProjectItemAddTool
);

#[async_trait]
impl Tool for GithubProjectItemAddTool {
    fn name(&self) -> &str {
        "github.project_item_add"
    }

    fn description(&self) -> &str {
        "Add an existing issue (or pull request) to a GitHub Project (v2); returns the item's id."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "project_id": { "type": "string", "description": "The project's node id" },
                "owner": self.owner_prop(),
                "repo": { "type": "string", "description": "Repository name" },
                "number": { "type": "integer", "description": "Issue number (integer)" },
                "workspace": workspace_prop()
            },
            "required": ["project_id", "repo", "number"]
        })
    }

    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();
        let out = async {
            let project = req_str(&input, "project_id")?;
            let owner = self.owner(&input)?;
            let repo = req_str(&input, "repo")?;
            let number = req_num(&input, "number")?;
            let client = self.client(&input).await?;
            let issue = client
                .get(&format!("/repos/{owner}/{repo}/issues/{number}"))
                .await?;
            let content = issue["node_id"]
                .as_str()
                .ok_or_else(|| eyre!("GitHub gave no node id for {owner}/{repo}#{number}"))?;
            let data = client
                .graphql(
                    "mutation($project: ID!, $content: ID!) { \
                     addProjectV2ItemById(input: {projectId: $project, contentId: $content}) { item { id } } }",
                    json!({ "project": project, "content": content }),
                )
                .await?;
            let item = data["addProjectV2ItemById"]["item"]["id"]
                .as_str()
                .ok_or_else(|| eyre!("GitHub did not say which item it added"))?;
            info!(owner = %owner, repo = %repo, number, "Added an issue to a project");
            Ok(json!({ "item_id": item }))
        }
        .await;
        Ok(finish(start, out))
    }
}

// ── github.issues_changed ────────────────────────────────────────

board_tool!(
    /// `github.issues_changed`: issues in the order they were last touched.
    GithubIssuesChangedTool
);

/// Whether `milestone` is passed to GitHub as it is (a number, `*`, `none`)
/// rather than looked up by title.
fn milestone_as_given(milestone: &str) -> bool {
    milestone == "*"
        || milestone == "none"
        || (!milestone.is_empty() && milestone.bytes().all(|b| b.is_ascii_digit()))
}

#[async_trait]
impl Tool for GithubIssuesChangedTool {
    fn name(&self) -> &str {
        "github.issues_changed"
    }

    fn description(&self) -> &str {
        "Read a repository's issues oldest-update first (pull requests left out), optionally only those updated since a time, in full (labels, assignees, milestone, type, parent). With number, reads that one issue."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "owner": self.owner_prop(),
                "repo": { "type": "string", "description": "Repository name" },
                "number": { "type": "integer", "description": "Read just this issue" },
                "since": { "type": "string", "description": "Only issues updated at or after this RFC 3339 time" },
                "state": { "type": "string", "enum": ["open", "closed", "all"], "description": "Default all" },
                "labels": { "type": "string", "description": "Comma-separated label names" },
                "milestone": { "type": "string", "description": "A milestone's title or number, '*' or 'none'" },
                "assignee": { "type": "string", "description": "An assignee's login" },
                "page": { "type": "integer", "description": "Page, from 1" },
                "per_page": { "type": "integer", "description": "1 to 100 (default 100)" },
                "workspace": workspace_prop()
            },
            "required": ["repo"]
        })
    }

    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();
        let out = async {
            let owner = self.owner(&input)?;
            let repo = req_str(&input, "repo")?;
            let client = self.client(&input).await?;
            let full_name = format!("{owner}/{repo}");
            if !input["number"].is_null() {
                let number = req_num(&input, "number")?;
                let issue = client
                    .get(&format!("/repos/{owner}/{repo}/issues/{number}"))
                    .await?;
                if issue.get("pull_request").is_some_and(|p| !p.is_null()) {
                    return Ok(json!({ "issues": [] }));
                }
                return Ok(json!({ "issues": [issue_from_rest(&issue, &full_name)] }));
            }
            let state = input["state"].as_str().unwrap_or("all");
            if !["open", "closed", "all"].contains(&state) {
                return Err(eyre!("'state' is open, closed or all"));
            }
            let per_page = match &input["per_page"] {
                Value::Null => 100,
                v => coerce_to_u64(v)
                    .filter(|n| (1..=100).contains(n))
                    .ok_or_else(|| eyre!("'per_page' is 1 to 100"))?,
            };
            let page = match &input["page"] {
                Value::Null => 1,
                v => coerce_to_u64(v)
                    .filter(|n| *n >= 1)
                    .ok_or_else(|| eyre!("'page' starts at 1"))?,
            };
            let mut query: Vec<(&str, String)> = vec![
                ("state", state.to_string()),
                ("sort", "updated".to_string()),
                ("direction", "asc".to_string()),
                ("per_page", per_page.to_string()),
                ("page", page.to_string()),
            ];
            for key in ["since", "labels", "assignee"] {
                if let Some(v) = input[key].as_str().filter(|v| !v.is_empty()) {
                    query.push((key, v.to_string()));
                }
            }
            if let Some(m) = input["milestone"].as_str().filter(|m| !m.is_empty()) {
                let number = if milestone_as_given(m) {
                    m.to_string()
                } else {
                    let all = client
                        .get_with_query(
                            &format!("/repos/{owner}/{repo}/milestones"),
                            &[("state", "all"), ("per_page", "100")],
                        )
                        .await?;
                    all.as_array()
                        .and_then(|a| a.iter().find(|x| x["title"].as_str() == Some(m)))
                        .and_then(|x| x["number"].as_u64())
                        .ok_or_else(|| eyre!("No milestone \"{m}\" in {full_name}"))?
                        .to_string()
                };
                query.push(("milestone", number));
            }
            let pairs: Vec<(&str, &str)> = query.iter().map(|(k, v)| (*k, v.as_str())).collect();
            let rows = client
                .get_with_query(&format!("/repos/{owner}/{repo}/issues"), &pairs)
                .await?;
            let rows = rows.as_array().cloned().unwrap_or_default();
            let next_page = (rows.len() as u64 == per_page).then_some(page + 1);
            let issues: Vec<Value> = rows
                .iter()
                .filter(|r| r.get("pull_request").is_none_or(Value::is_null))
                .map(|r| issue_from_rest(r, &full_name))
                .collect();
            Ok(json!({ "issues": issues, "next_page": next_page }))
        }
        .await;
        Ok(finish(start, out))
    }
}

// ── github.issue_labels ──────────────────────────────────────────

board_tool!(
    /// `github.issue_labels`: add and remove labels, never replace them.
    GithubIssueLabelsTool
);

#[async_trait]
impl Tool for GithubIssueLabelsTool {
    fn name(&self) -> &str {
        "github.issue_labels"
    }

    fn description(&self) -> &str {
        "Add and/or remove labels on an issue without touching its other labels. Returns the labels it has after."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "owner": self.owner_prop(),
                "repo": { "type": "string", "description": "Repository name" },
                "number": { "type": "integer", "description": "Issue number (integer)" },
                "add": { "type": "array", "items": { "type": "string" }, "description": "Labels to add" },
                "remove": { "type": "array", "items": { "type": "string" }, "description": "Labels to remove (one it doesn't have is ignored)" },
                "workspace": workspace_prop()
            },
            "required": ["repo", "number"]
        })
    }

    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();
        let out = async {
            let owner = self.owner(&input)?;
            let repo = req_str(&input, "repo")?;
            let number = req_num(&input, "number")?;
            let add = str_list(&input, "add")?;
            let remove = str_list(&input, "remove")?;
            if add.is_empty() && remove.is_empty() {
                return Err(eyre!("Nothing to do. Provide labels in 'add' or 'remove'"));
            }
            let client = self.client(&input).await?;
            let base = format!("/repos/{owner}/{repo}/issues/{number}/labels");
            for name in &remove {
                match client.delete(&format!("{base}/{}", segment(name))).await {
                    Ok(_) => {}
                    // It wasn't on the issue.
                    Err(e) if e.to_string().starts_with("Not found") => {}
                    Err(e) => return Err(e),
                }
            }
            let after = if add.is_empty() {
                client.get_with_query(&base, &[("per_page", "100")]).await?
            } else {
                client.post(&base, &json!({ "labels": add })).await?
            };
            let labels: Vec<Value> = after
                .as_array()
                .map(|a| a.iter().filter_map(|l| l.get("name").cloned()).collect())
                .unwrap_or_default();
            info!(owner = %owner, repo = %repo, number, "Changed an issue's labels");
            Ok(json!({ "labels": labels }))
        }
        .await;
        Ok(finish(start, out))
    }
}

// ── github.repo_labels ───────────────────────────────────────────

board_tool!(
    /// `github.repo_labels`: the labels and open milestones a repository has.
    GithubRepoLabelsTool
);

#[async_trait]
impl Tool for GithubRepoLabelsTool {
    fn name(&self) -> &str {
        "github.repo_labels"
    }

    fn description(&self) -> &str {
        "List a repository's labels (name, colour) and its open milestones (number, title)."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "owner": self.owner_prop(),
                "repo": { "type": "string", "description": "Repository name" },
                "workspace": workspace_prop()
            },
            "required": ["repo"]
        })
    }

    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();
        let out = async {
            let owner = self.owner(&input)?;
            let repo = req_str(&input, "repo")?;
            let client = self.client(&input).await?;
            let labels = client
                .get_with_query(
                    &format!("/repos/{owner}/{repo}/labels"),
                    &[("per_page", "100")],
                )
                .await?;
            let milestones = client
                .get_with_query(
                    &format!("/repos/{owner}/{repo}/milestones"),
                    &[("state", "open"), ("per_page", "100")],
                )
                .await?;
            let pick = |rows: &Value, keys: &[&str]| -> Vec<Value> {
                rows.as_array()
                    .map(|a| {
                        a.iter()
                            .map(|r| {
                                Value::Object(
                                    keys.iter()
                                        .map(|k| (k.to_string(), r[*k].clone()))
                                        .collect(),
                                )
                            })
                            .collect()
                    })
                    .unwrap_or_default()
            };
            Ok(json!({
                "labels": pick(&labels, &["name", "color"]),
                "milestones": pick(&milestones, &["number", "title"]),
            }))
        }
        .await;
        Ok(finish(start, out))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use wiremock::matchers::{body_partial_json, header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    use workforce_agent::tools::RateLimitedError;

    use super::super::client::graphql_url;
    use super::*;
    use crate::adapters::test_support::{InMem, LayeredInMem};

    // Fixtures are hand-built from GitHub's documented GraphQL and REST
    // shapes, not recorded from the live API.

    fn clients_for(server: &MockServer) -> Arc<LiveInstances<GithubClient>> {
        let client = Arc::new(GithubClient::new("ghp_test", &server.uri()).unwrap());
        Arc::new(LiveInstances::from(HashMap::from([(
            "default".to_string(),
            client,
        )])))
    }

    fn tool<T>(
        make: fn(Arc<LiveInstances<GithubClient>>, String, Option<String>) -> T,
        server: &MockServer,
    ) -> T {
        make(
            clients_for(server),
            "default".to_string(),
            Some("acme".to_string()),
        )
    }

    async fn graphql_answers(server: &MockServer, partial: Value, body: Value) {
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_partial_json(partial))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .expect(1)
            .mount(server)
            .await;
    }

    fn issue_node(number: u64) -> Value {
        json!({
            "id": format!("I_kw{number}"),
            "number": number,
            "url": format!("https://github.com/acme/app/issues/{number}"),
            "title": "Fix the thing",
            "body": "It breaks.",
            "state": "OPEN",
            "stateReason": null,
            "createdAt": "2026-11-01T10:00:00Z",
            "updatedAt": "2026-11-05T10:00:00Z",
            "author": {"login": "sam"},
            "labels": {"nodes": [{"name": "bug"}, {"name": "p1"}]},
            "assignees": {"nodes": [{"login": "sam", "name": "Sam Lee"}, {"login": "bot", "name": null}]},
            "milestone": {"title": "v2", "number": 3, "dueOn": "2026-12-01T00:00:00Z"},
            "issueType": {"name": "Bug"},
            "parent": {"number": 4, "title": "Epic", "repository": {"nameWithOwner": "acme/app"}},
            "repository": {"nameWithOwner": "acme/app"}
        })
    }

    #[test]
    fn graphql_lives_at_the_right_url() {
        assert_eq!(
            graphql_url("https://api.github.com"),
            "https://api.github.com/graphql"
        );
        assert_eq!(
            graphql_url("https://api.github.com/"),
            "https://api.github.com/graphql"
        );
        assert_eq!(
            graphql_url("https://git.example.com/api/v3"),
            "https://git.example.com/api/graphql"
        );
        assert_eq!(
            graphql_url("https://git.example.com/api/v3/"),
            "https://git.example.com/api/graphql"
        );
    }

    #[tokio::test]
    async fn a_graphql_error_says_its_type() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(header("authorization", "Bearer ghp_test"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": null,
                "errors": [{
                    "type": "INSUFFICIENT_SCOPES",
                    "message": "Your token has not been granted the required scopes to execute this query."
                }]
            })))
            .mount(&server)
            .await;
        let out = tool(GithubViewerTool::new, &server)
            .execute(json!({}))
            .await
            .unwrap();
        assert!(!out.success);
        assert!(
            out.error.as_deref().unwrap().starts_with(
                "GitHub GraphQL INSUFFICIENT_SCOPES: Your token has not been granted the required scopes"
            ),
            "{:?}",
            out.error
        );
    }

    #[tokio::test]
    async fn partial_data_with_a_not_found_still_fails() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {"nodes": []},
                "errors": [{"type": "NOT_FOUND", "message": "Could not resolve to a node"}]
            })))
            .mount(&server)
            .await;
        let client = GithubClient::new("ghp_test", &server.uri()).unwrap();
        let err = client.graphql("query { x }", json!({})).await.unwrap_err();
        assert_eq!(
            err.to_string(),
            "GitHub GraphQL NOT_FOUND: Could not resolve to a node"
        );
    }

    #[tokio::test]
    async fn a_graphql_rate_limit_is_a_rate_limit() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("retry-after", "42")
                    .set_body_json(json!({
                        "errors": [{"type": "RATE_LIMITED", "message": "API rate limit exceeded"}]
                    })),
            )
            .mount(&server)
            .await;
        let client = GithubClient::new("ghp_test", &server.uri()).unwrap();
        let err = client.graphql("query { x }", json!({})).await.unwrap_err();
        let limited = err.downcast_ref::<RateLimitedError>().expect("rate limit");
        assert_eq!(limited.retry_after_secs, Some(42));
    }

    #[tokio::test]
    async fn an_exhausted_quota_with_errors_is_a_rate_limit_waiting_for_the_reset() {
        let server = MockServer::start().await;
        let reset = chrono::Utc::now().timestamp() + 600;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("x-ratelimit-remaining", "0")
                    .insert_header("x-ratelimit-reset", reset.to_string().as_str())
                    .set_body_json(json!({"errors": [{"message": "Something went wrong"}]})),
            )
            .mount(&server)
            .await;
        let client = GithubClient::new("ghp_test", &server.uri()).unwrap();
        let err = client.graphql("query { x }", json!({})).await.unwrap_err();
        let limited = err.downcast_ref::<RateLimitedError>().expect("rate limit");
        let secs = limited.retry_after_secs.unwrap();
        assert!((590..=600).contains(&secs), "{secs}");
    }

    #[tokio::test]
    async fn the_viewer_says_whether_the_token_is_personal() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {"viewer": {"login": "sam", "name": "Sam Lee", "id": "U_kgDO1"}}
            })))
            .mount(&server)
            .await;
        let solo = |client: GithubClient| {
            Arc::new(LiveInstances::from(HashMap::from([(
                "default".to_string(),
                Arc::new(client),
            )])))
        };
        // A static token is never the caller's own.
        let out = GithubViewerTool::new(
            solo(GithubClient::new("t", &server.uri()).unwrap()),
            "default".into(),
            None,
        )
        .execute(json!({}))
        .await
        .unwrap();
        assert!(out.success, "{:?}", out.error);
        assert_eq!(
            out.output,
            json!({"login": "sam", "name": "Sam Lee", "id": "U_kgDO1", "personal": false})
        );
        // A shared vault token is not personal; one in a per-user tier is.
        let shared = InMem::from(&[("skills/github/token", "ghp_shared")]);
        let client = GithubClient::with_store(shared, "acme", &server.uri()).unwrap();
        assert!(!client.token_is_personal());
        let mine = LayeredInMem::new(
            &[("skills/github/token", "ghp_shared")],
            &[("skills/github/token", "ghp_mine")],
        );
        let client = GithubClient::with_store(mine, "acme", &server.uri()).unwrap();
        assert!(client.token_is_personal());
        let out = GithubViewerTool::new(solo(client), "default".into(), None)
            .execute(json!({}))
            .await
            .unwrap();
        assert_eq!(out.output["personal"], true);
    }

    #[tokio::test]
    async fn projects_are_listed_for_an_owner() {
        let server = MockServer::start().await;
        graphql_answers(
            &server,
            json!({"variables": {"owner": "acme"}}),
            json!({"data": {"repositoryOwner": {"projectsV2": {"nodes": [
                {"id": "PVT_1", "number": 7, "title": "Roadmap", "url": "https://github.com/orgs/acme/projects/7", "closed": false}
            ]}}}}),
        )
        .await;
        let out = tool(GithubProjectsListTool::new, &server)
            .execute(json!({}))
            .await
            .unwrap();
        assert!(out.success, "{:?}", out.error);
        assert_eq!(
            out.output,
            json!({"owner": "acme", "projects": [
                {"id": "PVT_1", "number": 7, "title": "Roadmap",
                 "url": "https://github.com/orgs/acme/projects/7", "closed": false}
            ]})
        );
    }

    #[tokio::test]
    async fn a_project_comes_with_its_fields_and_status() {
        let server = MockServer::start().await;
        graphql_answers(
            &server,
            json!({"variables": {"owner": "acme", "number": 7}}),
            json!({"data": {"repositoryOwner": {"projectV2": {
                "id": "PVT_1", "number": 7, "title": "Roadmap",
                "url": "https://github.com/orgs/acme/projects/7", "closed": false,
                "fields": {"nodes": [
                    {"id": "PVTF_t", "name": "Title"},
                    {"id": "PVTSSF_s", "name": "Status", "options": [
                        {"id": "o1", "name": "Todo"}, {"id": "o2", "name": "Done"}]},
                    {}
                ]}
            }}}}),
        )
        .await;
        let out = tool(GithubProjectGetTool::new, &server)
            .execute(json!({"number": 7}))
            .await
            .unwrap();
        assert!(out.success, "{:?}", out.error);
        let o = out.output;
        assert_eq!(o["id"], "PVT_1");
        assert_eq!(o["owner"], "acme");
        assert_eq!(
            o["fields"],
            json!([
                {"id": "PVTF_t", "name": "Title", "options": null},
                {"id": "PVTSSF_s", "name": "Status", "options": [
                    {"id": "o1", "name": "Todo"}, {"id": "o2", "name": "Done"}]},
            ])
        );
        assert_eq!(
            o["status"],
            json!({"field_id": "PVTSSF_s", "name": "Status", "options": [
                {"id": "o1", "name": "Todo"}, {"id": "o2", "name": "Done"}]})
        );
    }

    #[tokio::test]
    async fn lean_items_name_prs_drafts_and_redacted_ones() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_partial_json(json!({"variables": {"project": "PVT_1"}})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": {"node": {"items": {
                "pageInfo": {"hasNextPage": true, "endCursor": "Y3Vyc29y"},
                "nodes": [
                    {"id": "PVTI_a", "type": "ISSUE", "updatedAt": "2026-11-01T00:00:00Z",
                     "content": {"number": 12, "updatedAt": "2026-11-03T00:00:00Z",
                                 "repository": {"nameWithOwner": "acme/app"}}},
                    {"id": "PVTI_b", "type": "PULL_REQUEST", "updatedAt": "2026-11-04T00:00:00Z",
                     "content": {"number": 30, "updatedAt": "2026-11-02T00:00:00Z",
                                 "repository": {"nameWithOwner": "acme/app"}}},
                    {"id": "PVTI_c", "type": "DRAFT_ISSUE", "updatedAt": "2026-11-05T00:00:00Z",
                     "content": {}},
                    {"id": "PVTI_d", "type": "REDACTED", "updatedAt": "2026-11-06T00:00:00Z",
                     "content": null}
                ]
            }}}})))
            .mount(&server)
            .await;
        let out = tool(GithubProjectItemsTool::new, &server)
            .execute(json!({"project_id": "PVT_1"}))
            .await
            .unwrap();
        assert!(out.success, "{:?}", out.error);
        assert_eq!(
            out.output,
            json!({
                "items": [
                    {"item_id": "PVTI_a", "type": "ISSUE", "updated_at": "2026-11-03T00:00:00Z",
                     "repo": "acme/app", "number": 12},
                    {"item_id": "PVTI_b", "type": "PULL_REQUEST", "updated_at": "2026-11-04T00:00:00Z",
                     "repo": "acme/app", "number": 30},
                    {"item_id": "PVTI_c", "type": "DRAFT_ISSUE", "updated_at": "2026-11-05T00:00:00Z",
                     "repo": null, "number": null},
                    {"item_id": "PVTI_d", "type": "REDACTED", "updated_at": "2026-11-06T00:00:00Z",
                     "repo": null, "number": null},
                ],
                "page_info": {"has_next_page": true, "end_cursor": "Y3Vyc29y"}
            })
        );
        // A lean read asks for no status, no query and no cursor.
        let sent: Value = server.received_requests().await.unwrap()[0]
            .body_json()
            .unwrap();
        assert_eq!(sent["variables"], json!({"project": "PVT_1"}));
        assert!(!sent["query"].as_str().unwrap().contains("fieldValueByName"));
    }

    #[tokio::test]
    async fn full_items_carry_the_issue_and_its_status() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_partial_json(json!({"variables": {
                "project": "PVT_1", "after": "cur", "query": "label:bug", "field": "Status"}})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": {"node": {"items": {
                "pageInfo": {"hasNextPage": false, "endCursor": null},
                "nodes": [
                    {"id": "PVTI_a", "type": "ISSUE", "updatedAt": "2026-11-09T00:00:00Z",
                     "fieldValueByName": {"name": "In progress"}, "content": issue_node(12)},
                    {"id": "PVTI_b", "type": "PULL_REQUEST", "updatedAt": "2026-11-04T00:00:00Z",
                     "fieldValueByName": {"name": "Done"},
                     "content": {"number": 30, "updatedAt": "2026-11-02T00:00:00Z",
                                 "repository": {"nameWithOwner": "acme/app"}}},
                    {"id": "PVTI_c", "type": "ISSUE", "updatedAt": "2026-11-01T00:00:00Z",
                     "fieldValueByName": null, "content": issue_node(13)}
                ]
            }}}})))
            .mount(&server)
            .await;
        let out = tool(GithubProjectItemsTool::new, &server)
            .execute(
                json!({"project_id": "PVT_1", "full": true, "after": "cur", "query": "label:bug"}),
            )
            .await
            .unwrap();
        assert!(out.success, "{:?}", out.error);
        let items = out.output["items"].as_array().unwrap();
        let a = &items[0];
        assert_eq!(a["item_id"], "PVTI_a");
        assert_eq!(a["type"], "ISSUE");
        assert_eq!(a["status"], "In progress");
        assert_eq!(
            a["updated_at"], "2026-11-09T00:00:00Z",
            "the later of item and issue"
        );
        assert_eq!(a["repo"], "acme/app");
        assert_eq!(a["number"], 12);
        assert_eq!(a["issue_id"], "I_kw12");
        assert_eq!(a["url"], "https://github.com/acme/app/issues/12");
        assert_eq!(a["title"], "Fix the thing");
        assert!(a["body"].as_str().unwrap().contains("It breaks."));
        assert_ne!(a["body"], "It breaks.", "the body is wrapped");
        assert_eq!(a["state"], "OPEN");
        assert_eq!(a["state_reason"], Value::Null);
        assert_eq!(a["labels"], json!(["bug", "p1"]));
        assert_eq!(
            a["assignees"],
            json!([{"login": "sam", "name": "Sam Lee"}, {"login": "bot", "name": null}])
        );
        assert_eq!(
            a["milestone"],
            json!({"title": "v2", "number": 3, "due_on": "2026-12-01T00:00:00Z"})
        );
        assert_eq!(a["issue_type"], "Bug");
        assert_eq!(
            a["parent"],
            json!({"repo": "acme/app", "number": 4, "title": "Epic"})
        );
        assert_eq!(a["author"], "sam");
        assert_eq!(a["created_at"], "2026-11-01T10:00:00Z");
        // A pull request is the lean shape even in a full read.
        assert_eq!(
            items[1],
            json!({"item_id": "PVTI_b", "type": "PULL_REQUEST",
                   "updated_at": "2026-11-04T00:00:00Z", "repo": "acme/app", "number": 30})
        );
        assert_eq!(items[2]["status"], Value::Null);
        assert_eq!(out.output["page_info"]["has_next_page"], false);
        assert_eq!(out.output["page_info"]["end_cursor"], Value::Null);
    }

    #[tokio::test]
    async fn items_by_id_drop_the_ones_that_are_gone() {
        let server = MockServer::start().await;
        graphql_answers(
            &server,
            json!({"variables": {"ids": ["PVTI_a", "PVTI_gone"], "field": "Phase"}}),
            json!({"data": {"nodes": [
                {"id": "PVTI_a", "type": "ISSUE", "updatedAt": "2026-11-01T00:00:00Z",
                 "fieldValueByName": {"name": "Build"}, "content": issue_node(12)},
                null
            ]}}),
        )
        .await;
        let out = tool(GithubProjectItemsTool::new, &server)
            .execute(
                json!({"project_id": "PVT_1", "ids": ["PVTI_a", "PVTI_gone"], "field": "Phase"}),
            )
            .await
            .unwrap();
        assert!(out.success, "{:?}", out.error);
        let items = out.output["items"].as_array().unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["status"], "Build");
        assert!(out.output.get("page_info").is_none());
    }

    #[tokio::test]
    async fn too_many_ids_are_refused() {
        let server = MockServer::start().await;
        let ids: Vec<String> = (0..51).map(|i| format!("PVTI_{i}")).collect();
        let out = tool(GithubProjectItemsTool::new, &server)
            .execute(json!({"project_id": "PVT_1", "ids": ids}))
            .await
            .unwrap();
        assert!(!out.success);
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    fn fields_answer() -> Value {
        json!({"data": {"node": {"fields": {"nodes": [
            {"id": "PVTF_t", "name": "Title"},
            {"id": "PVTSSF_s", "name": "Status", "options": [
                {"id": "o1", "name": "Todo"}, {"id": "o2", "name": "In Progress"}]}
        ]}}}})
    }

    #[tokio::test]
    async fn a_status_is_set_by_the_options_id_found_by_name() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_partial_json(
                json!({"variables": {"project": "PVT_1"}}),
            ))
            .and(wiremock::matchers::body_string_contains(
                "fields(first: 50)",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(fields_answer()))
            .expect(1)
            .mount(&server)
            .await;
        graphql_answers(
            &server,
            json!({"variables": {
                "project": "PVT_1", "item": "PVTI_a", "field": "PVTSSF_s", "option": "o2"}}),
            json!({"data": {"updateProjectV2ItemFieldValue": {"projectV2Item": {"id": "PVTI_a"}}}}),
        )
        .await;
        let out = tool(GithubProjectItemStatusTool::new, &server)
            .execute(json!({"project_id": "PVT_1", "item_id": "PVTI_a", "status": "in progress"}))
            .await
            .unwrap();
        assert!(out.success, "{:?}", out.error);
        assert_eq!(
            out.output,
            json!({"item_id": "PVTI_a", "status": "In Progress"})
        );
    }

    #[tokio::test]
    async fn a_status_the_project_lacks_is_said_so_and_nothing_is_written() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(fields_answer()))
            .expect(2)
            .mount(&server)
            .await;
        let t = tool(GithubProjectItemStatusTool::new, &server);
        let out = t
            .execute(json!({"project_id": "PVT_1", "item_id": "PVTI_a", "status": "Blocked"}))
            .await
            .unwrap();
        assert!(!out.success);
        assert_eq!(
            out.error.as_deref(),
            Some("No Status option \"Blocked\" on this project")
        );
        let out = t
            .execute(json!({"project_id": "PVT_1", "item_id": "PVTI_a", "status": "Todo", "field": "Phase"}))
            .await
            .unwrap();
        assert_eq!(
            out.error.as_deref(),
            Some("This project has no single-select field \"Phase\"")
        );
        // Only the two field reads were made.
        assert_eq!(server.received_requests().await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn moving_to_the_top_sends_a_null_after() {
        let server = MockServer::start().await;
        graphql_answers(
            &server,
            json!({"variables": {"project": "PVT_1", "item": "PVTI_a", "after": null}}),
            json!({"data": {"updateProjectV2ItemPosition": {"clientMutationId": null}}}),
        )
        .await;
        let out = tool(GithubProjectItemMoveTool::new, &server)
            .execute(json!({"project_id": "PVT_1", "item_id": "PVTI_a"}))
            .await
            .unwrap();
        assert!(out.success, "{:?}", out.error);
        assert_eq!(out.output, json!({"moved": true, "item_id": "PVTI_a"}));
        let sent: Value = server.received_requests().await.unwrap()[0]
            .body_json()
            .unwrap();
        assert!(sent["variables"]["after"].is_null());
        assert!(sent["variables"].as_object().unwrap().contains_key("after"));
    }

    #[tokio::test]
    async fn moving_after_an_item_names_it() {
        let server = MockServer::start().await;
        graphql_answers(
            &server,
            json!({"variables": {"item": "PVTI_a", "after": "PVTI_b"}}),
            json!({"data": {"updateProjectV2ItemPosition": {}}}),
        )
        .await;
        let out = tool(GithubProjectItemMoveTool::new, &server)
            .execute(json!({"project_id": "PVT_1", "item_id": "PVTI_a", "after_id": "PVTI_b"}))
            .await
            .unwrap();
        assert!(out.success, "{:?}", out.error);
    }

    #[tokio::test]
    async fn an_issue_is_added_to_a_project_by_its_node_id() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/acme/app/issues/12"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "number": 12, "node_id": "I_kw12"
            })))
            .expect(1)
            .mount(&server)
            .await;
        graphql_answers(
            &server,
            json!({"variables": {"project": "PVT_1", "content": "I_kw12"}}),
            json!({"data": {"addProjectV2ItemById": {"item": {"id": "PVTI_new"}}}}),
        )
        .await;
        let out = tool(GithubProjectItemAddTool::new, &server)
            .execute(json!({"project_id": "PVT_1", "repo": "app", "number": 12}))
            .await
            .unwrap();
        assert!(out.success, "{:?}", out.error);
        assert_eq!(out.output, json!({"item_id": "PVTI_new"}));
    }

    fn rest_issue(number: u64) -> Value {
        json!({
            "number": number,
            "node_id": format!("I_kw{number}"),
            "html_url": format!("https://github.com/acme/app/issues/{number}"),
            "title": "Fix the thing",
            "body": "It breaks.",
            "state": "closed",
            "state_reason": "not_planned",
            "labels": [{"id": 1, "name": "bug", "color": "d73a4a"}],
            "assignees": [{"login": "sam"}],
            "milestone": {"title": "v2", "number": 3, "due_on": null},
            "type": {"name": "Task"},
            "user": {"login": "sam"},
            "created_at": "2026-11-01T10:00:00Z",
            "updated_at": "2026-11-05T10:00:00Z"
        })
    }

    #[tokio::test]
    async fn changed_issues_leave_out_prs_and_say_whether_there_is_more() {
        let server = MockServer::start().await;
        let mut pr = rest_issue(2);
        pr["pull_request"] = json!({"url": "https://api.github.com/repos/acme/app/pulls/2"});
        Mock::given(method("GET"))
            .and(path("/repos/acme/app/issues"))
            .and(query_param("state", "all"))
            .and(query_param("sort", "updated"))
            .and(query_param("direction", "asc"))
            .and(query_param("per_page", "2"))
            .and(query_param("page", "1"))
            .and(query_param("since", "2026-11-01T00:00:00Z"))
            .and(query_param("labels", "bug,p1"))
            .and(query_param("assignee", "sam"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([rest_issue(1), pr])))
            .expect(1)
            .mount(&server)
            .await;
        let t = tool(GithubIssuesChangedTool::new, &server);
        let out = t
            .execute(json!({
                "repo": "app", "per_page": 2, "since": "2026-11-01T00:00:00Z",
                "labels": "bug,p1", "assignee": "sam"
            }))
            .await
            .unwrap();
        assert!(out.success, "{:?}", out.error);
        let issues = out.output["issues"].as_array().unwrap();
        assert_eq!(issues.len(), 1);
        assert_eq!(out.output["next_page"], 2, "the raw page was full");
        let i = &issues[0];
        assert_eq!(i["repo"], "acme/app");
        assert_eq!(i["number"], 1);
        assert_eq!(i["issue_id"], "I_kw1");
        assert_eq!(i["url"], "https://github.com/acme/app/issues/1");
        assert_eq!(i["state"], "CLOSED");
        assert_eq!(i["state_reason"], "NOT_PLANNED");
        assert_eq!(i["labels"], json!(["bug"]));
        assert_eq!(i["assignees"], json!([{"login": "sam", "name": null}]));
        assert_eq!(
            i["milestone"],
            json!({"title": "v2", "number": 3, "due_on": null})
        );
        assert_eq!(i["issue_type"], "Task");
        assert_eq!(i["parent"], Value::Null);
        assert_eq!(i["author"], "sam");
        assert_ne!(i["body"], "It breaks.", "the body is wrapped");
    }

    #[tokio::test]
    async fn a_short_page_has_no_next_page() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/acme/app/issues"))
            .and(query_param("page", "3"))
            .and(query_param("per_page", "100"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([rest_issue(1)])))
            .mount(&server)
            .await;
        let out = tool(GithubIssuesChangedTool::new, &server)
            .execute(json!({"repo": "app", "page": 3}))
            .await
            .unwrap();
        assert!(out.success, "{:?}", out.error);
        assert_eq!(out.output["next_page"], Value::Null);
    }

    #[tokio::test]
    async fn a_milestone_is_looked_up_by_its_title() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/acme/app/milestones"))
            .and(query_param("state", "all"))
            .and(query_param("per_page", "100"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([
                {"number": 2, "title": "v1"}, {"number": 3, "title": "v2"}
            ])))
            .expect(2)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/repos/acme/app/issues"))
            .and(query_param("milestone", "3"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
            .expect(1)
            .mount(&server)
            .await;
        let t = tool(GithubIssuesChangedTool::new, &server);
        let out = t
            .execute(json!({"repo": "app", "milestone": "v2"}))
            .await
            .unwrap();
        assert!(out.success, "{:?}", out.error);
        let out = t
            .execute(json!({"repo": "app", "milestone": "v9"}))
            .await
            .unwrap();
        assert!(!out.success);
        assert_eq!(
            out.error.as_deref(),
            Some("No milestone \"v9\" in acme/app")
        );
    }

    #[tokio::test]
    async fn a_numeric_milestone_goes_as_it_is() {
        let server = MockServer::start().await;
        for given in ["4", "*", "none"] {
            Mock::given(method("GET"))
                .and(path("/repos/acme/app/issues"))
                .and(query_param("milestone", given))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
                .expect(1)
                .mount(&server)
                .await;
            let out = tool(GithubIssuesChangedTool::new, &server)
                .execute(json!({"repo": "app", "milestone": given}))
                .await
                .unwrap();
            assert!(out.success, "{given}: {:?}", out.error);
        }
        assert!(milestone_as_given("12"));
        assert!(!milestone_as_given("12b"));
        assert!(!milestone_as_given(""));
    }

    #[tokio::test]
    async fn one_issue_is_read_by_number_and_a_pr_is_not_an_issue() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/acme/app/issues/1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(rest_issue(1)))
            .mount(&server)
            .await;
        let mut pr = rest_issue(2);
        pr["pull_request"] = json!({});
        Mock::given(method("GET"))
            .and(path("/repos/acme/app/issues/2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(pr))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/repos/acme/app/issues/3"))
            .respond_with(ResponseTemplate::new(404).set_body_json(json!({"message": "Not Found"})))
            .mount(&server)
            .await;
        let t = tool(GithubIssuesChangedTool::new, &server);
        let out = t
            .execute(json!({"repo": "app", "number": 1}))
            .await
            .unwrap();
        assert_eq!(out.output["issues"].as_array().unwrap().len(), 1);
        assert!(out.output.get("next_page").is_none());
        let out = t
            .execute(json!({"repo": "app", "number": 2}))
            .await
            .unwrap();
        assert!(out.success);
        assert_eq!(out.output, json!({"issues": []}));
        let out = t
            .execute(json!({"repo": "app", "number": 3}))
            .await
            .unwrap();
        assert!(!out.success, "a 404 stays an error");
    }

    #[tokio::test]
    async fn labels_are_added_with_a_post_and_removed_with_deletes_never_replaced() {
        let server = MockServer::start().await;
        Mock::given(method("DELETE"))
            .and(path("/repos/acme/app/issues/1/labels/needs%20review"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("DELETE"))
            .and(path("/repos/acme/app/issues/1/labels/gone"))
            .respond_with(
                ResponseTemplate::new(404)
                    .set_body_json(json!({"message": "Label does not exist"})),
            )
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/repos/acme/app/issues/1/labels"))
            .and(body_partial_json(json!({"labels": ["bug", "p1"]})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([
                {"name": "bug", "color": "d73a4a"}, {"name": "p1", "color": "ff0000"},
                {"name": "docs", "color": "0075ca"}
            ])))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(&server)
            .await;
        let out = tool(GithubIssueLabelsTool::new, &server)
            .execute(json!({
                "repo": "app", "number": 1,
                "add": ["bug", "p1"], "remove": ["needs review", "gone"]
            }))
            .await
            .unwrap();
        assert!(out.success, "{:?}", out.error);
        assert_eq!(out.output, json!({"labels": ["bug", "p1", "docs"]}));
    }

    #[tokio::test]
    async fn only_removing_reads_the_labels_after() {
        let server = MockServer::start().await;
        Mock::given(method("DELETE"))
            .and(path("/repos/acme/app/issues/1/labels/p1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/repos/acme/app/issues/1/labels"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([{"name": "bug"}])))
            .expect(1)
            .mount(&server)
            .await;
        let t = tool(GithubIssueLabelsTool::new, &server);
        let out = t
            .execute(json!({"repo": "app", "number": 1, "remove": ["p1"]}))
            .await
            .unwrap();
        assert_eq!(out.output, json!({"labels": ["bug"]}));
        let out = t
            .execute(json!({"repo": "app", "number": 1}))
            .await
            .unwrap();
        assert!(!out.success, "nothing to do is no call");
    }

    #[tokio::test]
    async fn a_repository_lists_its_labels_and_open_milestones() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/acme/app/labels"))
            .and(query_param("per_page", "100"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([
                {"id": 1, "name": "bug", "color": "d73a4a", "description": "x"}
            ])))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/repos/acme/app/milestones"))
            .and(query_param("state", "open"))
            .and(query_param("per_page", "100"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([
                {"number": 3, "title": "v2", "state": "open"}
            ])))
            .mount(&server)
            .await;
        let out = tool(GithubRepoLabelsTool::new, &server)
            .execute(json!({"repo": "app"}))
            .await
            .unwrap();
        assert!(out.success, "{:?}", out.error);
        assert_eq!(
            out.output,
            json!({
                "labels": [{"name": "bug", "color": "d73a4a"}],
                "milestones": [{"number": 3, "title": "v2"}]
            })
        );
    }

    #[tokio::test]
    async fn an_issue_closes_with_its_reason() {
        use super::super::tools::GithubIssueUpdateTool;
        let server = MockServer::start().await;
        Mock::given(method("PATCH"))
            .and(path("/repos/acme/app/issues/1"))
            .and(body_partial_json(
                json!({"state": "closed", "state_reason": "not_planned"}),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "number": 1, "state": "closed", "title": "t",
                "html_url": "https://github.com/acme/app/issues/1",
                "updated_at": "2026-11-06T00:00:00Z"
            })))
            .expect(1)
            .mount(&server)
            .await;
        let t = tool(GithubIssueUpdateTool::new, &server);
        let out = t
            .execute(json!({"repo": "app", "number": 1, "state": "closed", "state_reason": "not_planned"}))
            .await
            .unwrap();
        assert!(out.success, "{:?}", out.error);
        assert_eq!(out.output["updated_at"], "2026-11-06T00:00:00Z");
        let out = t
            .execute(
                json!({"repo": "app", "number": 1, "state": "closed", "state_reason": "whatever"}),
            )
            .await
            .unwrap();
        assert!(!out.success);
    }

    #[test]
    fn the_later_time_wins() {
        let a = json!("2026-11-01T00:00:00Z");
        let b = json!("2026-11-02T00:00:00Z");
        assert_eq!(later(&a, &b), b);
        assert_eq!(later(&b, &a), b);
        assert_eq!(later(&a, &Value::Null), a);
        assert_eq!(later(&Value::Null, &b), b);
    }

    #[test]
    fn a_label_is_one_path_segment() {
        assert_eq!(segment("good first/issue"), "good%20first%2Fissue");
        assert_eq!(segment("p1"), "p1");
    }
}
