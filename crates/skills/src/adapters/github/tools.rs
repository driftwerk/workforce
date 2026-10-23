//! GitHub agent-callable tools.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use eyre::Result;
use tracing::{info, warn};

use workforce_agent::security::external_content::wrap_external_content;
use workforce_agent::tools::{Tool, ToolResult};

use crate::adapters::utils::{coerce_to_u64, instance_choices};

use super::app_auth::AppError;
use super::client::GithubClient;

/// Wrap a JSON string field with security boundary markers if it contains content.
fn wrap_field(value: &serde_json::Value, source: &str) -> serde_json::Value {
    match value.as_str() {
        Some(s) if !s.is_empty() => {
            serde_json::Value::String(wrap_external_content(s, source).wrapped_content)
        }
        _ => value.clone(),
    }
}

/// Resolve the owner: use the explicit param, fall back to default_owner, or error.
fn resolve_owner<'a>(
    input: &'a serde_json::Value,
    default_owner: &'a Option<String>,
) -> std::result::Result<&'a str, String> {
    input["owner"]
        .as_str()
        .or(default_owner.as_deref())
        .ok_or_else(|| "Missing 'owner' parameter and no default_owner configured".to_string())
}

/// Resolve the GitHub client for a given input. If the `owner` matches a configured
/// workspace name, use that workspace's client. Otherwise use the default.
fn resolve_client(
    clients: &HashMap<String, Arc<GithubClient>>,
    default_workspace: &str,
    input: &serde_json::Value,
) -> eyre::Result<Arc<GithubClient>> {
    let owner = input["owner"].as_str();
    // Explicit `workspace` param (for github.api), else an `owner` that matches a
    // configured workspace name; both post-filtered through the environment lock.
    let requested = input["workspace"]
        .as_str()
        .or_else(|| owner.filter(|o| clients.contains_key(*o)));
    let name = workforce_agent::llm::environment::resolve_instance(
        "github",
        requested,
        default_workspace,
        clients,
    )
    .map_err(|d| eyre::eyre!("{}", d.message))?;
    clients.get(&name).cloned().ok_or_else(|| {
        eyre::eyre!(
            "Unknown GitHub workspace: '{name}'. Available: {:?}",
            clients.keys().collect::<Vec<_>>()
        )
    })
}

/// Build a dynamic description for the `owner` parameter that includes available org names.
fn owner_description(
    clients: &HashMap<String, Arc<GithubClient>>,
    default_workspace: &str,
) -> String {
    if clients.len() <= 1 {
        return "GitHub user or org name. Uses default if omitted.".to_string();
    }
    format!(
        "GitHub org/user — also selects credentials. {}",
        instance_choices(clients, default_workspace, "owner")
    )
}

// ── github.list_repos ────────────────────────────────────────────────

pub struct GithubListReposTool {
    clients: Arc<HashMap<String, Arc<GithubClient>>>,
    default_workspace: String,
    default_owner: Option<String>,
}

impl GithubListReposTool {
    pub fn new(
        clients: Arc<HashMap<String, Arc<GithubClient>>>,
        default_workspace: String,
        default_owner: Option<String>,
    ) -> Self {
        Self {
            clients,
            default_workspace,
            default_owner,
        }
    }
}

#[async_trait]
impl Tool for GithubListReposTool {
    fn name(&self) -> &str {
        "github.list_repos"
    }

    fn description(&self) -> &str {
        "List repositories for a GitHub user or organisation."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "owner": {
                    "type": "string",
                    "description": owner_description(&self.clients, &self.default_workspace)
                },
                "type": {
                    "type": "string",
                    "description": "Filter by repo type: all, owner, member (default: all)",
                    "enum": ["all", "owner", "member"]
                },
                "sort": {
                    "type": "string",
                    "description": "Sort by",
                    "enum": ["created", "updated", "pushed", "full_name"]
                },
                "per_page": {
                    "type": "integer",
                    "description": "Results per page (integer, default: 30, max: 100)",
                    "minimum": 1,
                    "maximum": 100
                }
            },
            "required": []
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();

        let owner = match resolve_owner(&input, &self.default_owner) {
            Ok(o) => o,
            Err(e) => return Ok(ToolResult::failure(e, start.elapsed().as_millis() as u64)),
        };
        let client = match resolve_client(&self.clients, &self.default_workspace, &input) {
            Ok(c) => c,
            Err(e) => {
                return Ok(ToolResult::failure(
                    format!("{e}"),
                    start.elapsed().as_millis() as u64,
                ))
            }
        };

        let mut query: Vec<(&str, &str)> = Vec::new();
        let type_val = input["type"].as_str().unwrap_or("all");
        query.push(("type", type_val));
        let sort_val = input["sort"].as_str().unwrap_or("updated");
        query.push(("sort", sort_val));
        let per_page_str = coerce_to_u64(&input["per_page"])
            .unwrap_or(30)
            .min(100)
            .to_string();
        query.push(("per_page", &per_page_str));

        let path = format!("/users/{}/repos", owner);
        let duration_ms = start.elapsed().as_millis() as u64;

        match client.get_with_query(&path, &query).await {
            Ok(repos) => {
                let count = repos.as_array().map(|a| a.len()).unwrap_or(0);
                info!(owner = %owner, count, "Listed GitHub repos");
                let summary: Vec<serde_json::Value> = repos
                    .as_array()
                    .unwrap_or(&vec![])
                    .iter()
                    .map(|r| {
                        serde_json::json!({
                            "name": r["name"],
                            "full_name": r["full_name"],
                            "description": r["description"],
                            "private": r["private"],
                            "language": r["language"],
                            "updated_at": r["updated_at"],
                            "html_url": r["html_url"],
                            "default_branch": r["default_branch"],
                        })
                    })
                    .collect();
                Ok(ToolResult::success(
                    serde_json::json!({ "owner": owner, "count": count, "repos": summary }),
                    duration_ms,
                ))
            }
            Err(e) => {
                warn!(error = %e, owner = %owner, "Failed to list repos");
                Ok(ToolResult::from_report(
                    &e.wrap_err("Failed to list repos"),
                    duration_ms,
                ))
            }
        }
    }
}

// ── github.get_repo ──────────────────────────────────────────────────

pub struct GithubGetRepoTool {
    clients: Arc<HashMap<String, Arc<GithubClient>>>,
    default_workspace: String,
    default_owner: Option<String>,
}

impl GithubGetRepoTool {
    pub fn new(
        clients: Arc<HashMap<String, Arc<GithubClient>>>,
        default_workspace: String,
        default_owner: Option<String>,
    ) -> Self {
        Self {
            clients,
            default_workspace,
            default_owner,
        }
    }
}

#[async_trait]
impl Tool for GithubGetRepoTool {
    fn name(&self) -> &str {
        "github.get_repo"
    }

    fn description(&self) -> &str {
        "Get details about a specific GitHub repository."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "owner": {
                    "type": "string",
                    "description": owner_description(&self.clients, &self.default_workspace)
                },
                "repo": {
                    "type": "string",
                    "description": "Repository name"
                }
            },
            "required": ["repo"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();

        let owner = match resolve_owner(&input, &self.default_owner) {
            Ok(o) => o,
            Err(e) => return Ok(ToolResult::failure(e, start.elapsed().as_millis() as u64)),
        };
        let client = match resolve_client(&self.clients, &self.default_workspace, &input) {
            Ok(c) => c,
            Err(e) => {
                return Ok(ToolResult::failure(
                    format!("{e}"),
                    start.elapsed().as_millis() as u64,
                ))
            }
        };
        let repo = match input["repo"].as_str() {
            Some(r) => r,
            None => {
                return Ok(ToolResult::failure(
                    "Missing 'repo' parameter",
                    start.elapsed().as_millis() as u64,
                ))
            }
        };

        let path = format!("/repos/{}/{}", owner, repo);
        let duration_ms = start.elapsed().as_millis() as u64;

        match client.get(&path).await {
            Ok(data) => {
                info!(owner = %owner, repo = %repo, "Got GitHub repo");
                Ok(ToolResult::success(
                    serde_json::json!({
                        "name": data["name"],
                        "full_name": data["full_name"],
                        "description": data["description"],
                        "private": data["private"],
                        "language": data["language"],
                        "default_branch": data["default_branch"],
                        "html_url": data["html_url"],
                        "clone_url": data["clone_url"],
                        "stargazers_count": data["stargazers_count"],
                        "forks_count": data["forks_count"],
                        "open_issues_count": data["open_issues_count"],
                        "created_at": data["created_at"],
                        "updated_at": data["updated_at"],
                        "topics": data["topics"],
                    }),
                    duration_ms,
                ))
            }
            Err(e) => {
                warn!(error = %e, owner = %owner, repo = %repo, "Failed to get repo");
                Ok(ToolResult::from_report(
                    &e.wrap_err("Failed to get repo"),
                    duration_ms,
                ))
            }
        }
    }
}

// ── github.list_issues ───────────────────────────────────────────────

pub struct GithubListIssuesTool {
    clients: Arc<HashMap<String, Arc<GithubClient>>>,
    default_workspace: String,
    default_owner: Option<String>,
}

impl GithubListIssuesTool {
    pub fn new(
        clients: Arc<HashMap<String, Arc<GithubClient>>>,
        default_workspace: String,
        default_owner: Option<String>,
    ) -> Self {
        Self {
            clients,
            default_workspace,
            default_owner,
        }
    }
}

#[async_trait]
impl Tool for GithubListIssuesTool {
    fn name(&self) -> &str {
        "github.list_issues"
    }

    fn description(&self) -> &str {
        "List issues in a GitHub repository. Can filter by state and labels."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "owner": {
                    "type": "string",
                    "description": owner_description(&self.clients, &self.default_workspace)
                },
                "repo": {
                    "type": "string",
                    "description": "Repository name"
                },
                "state": {
                    "type": "string",
                    "description": "Filter by state: open, closed, all (default: open)",
                    "enum": ["open", "closed", "all"]
                },
                "labels": {
                    "type": "string",
                    "description": "Comma-separated list of label names to filter by"
                },
                "per_page": {
                    "type": "integer",
                    "description": "Results per page (integer, default: 30, max: 100)",
                    "minimum": 1,
                    "maximum": 100
                }
            },
            "required": ["repo"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();

        let owner = match resolve_owner(&input, &self.default_owner) {
            Ok(o) => o,
            Err(e) => return Ok(ToolResult::failure(e, start.elapsed().as_millis() as u64)),
        };
        let client = match resolve_client(&self.clients, &self.default_workspace, &input) {
            Ok(c) => c,
            Err(e) => {
                return Ok(ToolResult::failure(
                    format!("{e}"),
                    start.elapsed().as_millis() as u64,
                ))
            }
        };
        let repo = match input["repo"].as_str() {
            Some(r) => r,
            None => {
                return Ok(ToolResult::failure(
                    "Missing 'repo' parameter",
                    start.elapsed().as_millis() as u64,
                ))
            }
        };

        let mut query: Vec<(&str, &str)> = Vec::new();
        let state = input["state"].as_str().unwrap_or("open");
        query.push(("state", state));
        let labels_val;
        if let Some(labels) = input["labels"].as_str() {
            labels_val = labels.to_string();
            query.push(("labels", &labels_val));
        }
        let per_page_str = coerce_to_u64(&input["per_page"])
            .unwrap_or(30)
            .min(100)
            .to_string();
        query.push(("per_page", &per_page_str));

        let path = format!("/repos/{}/{}/issues", owner, repo);
        let duration_ms = start.elapsed().as_millis() as u64;

        match client.get_with_query(&path, &query).await {
            Ok(issues) => {
                let count = issues.as_array().map(|a| a.len()).unwrap_or(0);
                info!(owner = %owner, repo = %repo, count, "Listed GitHub issues");
                let summary: Vec<serde_json::Value> = issues
                    .as_array()
                    .unwrap_or(&vec![])
                    .iter()
                    .filter(|i| i["pull_request"].is_null()) // Exclude PRs
                    .map(|i| {
                        serde_json::json!({
                            "number": i["number"],
                            "title": i["title"],
                            "state": i["state"],
                            "user": i["user"]["login"],
                            "labels": i["labels"].as_array()
                                .map(|l| l.iter().map(|lb| &lb["name"]).collect::<Vec<_>>())
                                .unwrap_or_default(),
                            "created_at": i["created_at"],
                            "updated_at": i["updated_at"],
                            "html_url": i["html_url"],
                        })
                    })
                    .collect();
                let filtered_count = summary.len();
                Ok(ToolResult::success(
                    serde_json::json!({
                        "owner": owner,
                        "repo": repo,
                        "count": filtered_count,
                        "issues": summary,
                    }),
                    duration_ms,
                ))
            }
            Err(e) => {
                warn!(error = %e, owner = %owner, repo = %repo, "Failed to list issues");
                Ok(ToolResult::from_report(
                    &e.wrap_err("Failed to list issues"),
                    duration_ms,
                ))
            }
        }
    }
}

// ── github.get_issue ─────────────────────────────────────────────────

pub struct GithubGetIssueTool {
    clients: Arc<HashMap<String, Arc<GithubClient>>>,
    default_workspace: String,
    default_owner: Option<String>,
}

impl GithubGetIssueTool {
    pub fn new(
        clients: Arc<HashMap<String, Arc<GithubClient>>>,
        default_workspace: String,
        default_owner: Option<String>,
    ) -> Self {
        Self {
            clients,
            default_workspace,
            default_owner,
        }
    }
}

#[async_trait]
impl Tool for GithubGetIssueTool {
    fn name(&self) -> &str {
        "github.get_issue"
    }

    fn description(&self) -> &str {
        "Get details and comments for a specific GitHub issue."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "owner": {
                    "type": "string",
                    "description": owner_description(&self.clients, &self.default_workspace)
                },
                "repo": {
                    "type": "string",
                    "description": "Repository name"
                },
                "number": {
                    "type": "integer",
                    "description": "Issue number (integer, required — NOT a string)"
                }
            },
            "required": ["repo", "number"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();

        let owner = match resolve_owner(&input, &self.default_owner) {
            Ok(o) => o,
            Err(e) => return Ok(ToolResult::failure(e, start.elapsed().as_millis() as u64)),
        };
        let client = match resolve_client(&self.clients, &self.default_workspace, &input) {
            Ok(c) => c,
            Err(e) => {
                return Ok(ToolResult::failure(
                    format!("{e}"),
                    start.elapsed().as_millis() as u64,
                ))
            }
        };
        let repo = match input["repo"].as_str() {
            Some(r) => r,
            None => {
                return Ok(ToolResult::failure(
                    "Missing 'repo' parameter",
                    start.elapsed().as_millis() as u64,
                ))
            }
        };
        let number = match coerce_to_u64(&input["number"]) {
            Some(n) => n,
            None => {
                return Ok(ToolResult::failure(
                    "Missing 'number' parameter",
                    start.elapsed().as_millis() as u64,
                ))
            }
        };

        let issue_path = format!("/repos/{}/{}/issues/{}", owner, repo, number);
        let comments_path = format!("/repos/{}/{}/issues/{}/comments", owner, repo, number);
        let duration_ms = start.elapsed().as_millis() as u64;

        let issue = match client.get(&issue_path).await {
            Ok(data) => data,
            Err(e) => {
                warn!(error = %e, "Failed to get issue");
                return Ok(ToolResult::from_report(
                    &e.wrap_err("Failed to get issue"),
                    duration_ms,
                ));
            }
        };

        let comments = client.get(&comments_path).await.unwrap_or_default();
        let comment_summary: Vec<serde_json::Value> = comments
            .as_array()
            .unwrap_or(&vec![])
            .iter()
            .map(|c| {
                serde_json::json!({
                    "user": c["user"]["login"],
                    "body": wrap_field(&c["body"], "GitHub comment"),
                    "created_at": c["created_at"],
                })
            })
            .collect();

        info!(owner = %owner, repo = %repo, number, "Got GitHub issue");
        Ok(ToolResult::success(
            serde_json::json!({
                "number": issue["number"],
                "title": issue["title"],
                "state": issue["state"],
                "body": wrap_field(&issue["body"], "GitHub issue body"),
                "user": issue["user"]["login"],
                "labels": issue["labels"].as_array()
                    .map(|l| l.iter().map(|lb| &lb["name"]).collect::<Vec<_>>())
                    .unwrap_or_default(),
                "assignees": issue["assignees"].as_array()
                    .map(|a| a.iter().map(|u| &u["login"]).collect::<Vec<_>>())
                    .unwrap_or_default(),
                "created_at": issue["created_at"],
                "updated_at": issue["updated_at"],
                "html_url": issue["html_url"],
                "comments": comment_summary,
            }),
            duration_ms,
        ))
    }
}

// ── github.create_issue ──────────────────────────────────────────────

pub struct GithubCreateIssueTool {
    clients: Arc<HashMap<String, Arc<GithubClient>>>,
    default_workspace: String,
    default_owner: Option<String>,
}

impl GithubCreateIssueTool {
    pub fn new(
        clients: Arc<HashMap<String, Arc<GithubClient>>>,
        default_workspace: String,
        default_owner: Option<String>,
    ) -> Self {
        Self {
            clients,
            default_workspace,
            default_owner,
        }
    }
}

#[async_trait]
impl Tool for GithubCreateIssueTool {
    fn name(&self) -> &str {
        "github.create_issue"
    }

    fn description(&self) -> &str {
        "Create a new issue in a GitHub repository."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "owner": {
                    "type": "string",
                    "description": owner_description(&self.clients, &self.default_workspace)
                },
                "repo": {
                    "type": "string",
                    "description": "Repository name"
                },
                "title": {
                    "type": "string",
                    "description": "Issue title"
                },
                "body": {
                    "type": "string",
                    "description": "Issue body (Markdown supported)"
                },
                "labels": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Labels to apply to the issue"
                },
                "assignees": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "GitHub usernames to assign"
                }
            },
            "required": ["repo", "title"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();

        let owner = match resolve_owner(&input, &self.default_owner) {
            Ok(o) => o,
            Err(e) => return Ok(ToolResult::failure(e, start.elapsed().as_millis() as u64)),
        };
        let client = match resolve_client(&self.clients, &self.default_workspace, &input) {
            Ok(c) => c,
            Err(e) => {
                return Ok(ToolResult::failure(
                    format!("{e}"),
                    start.elapsed().as_millis() as u64,
                ))
            }
        };
        let repo = match input["repo"].as_str() {
            Some(r) => r,
            None => {
                return Ok(ToolResult::failure(
                    "Missing 'repo' parameter",
                    start.elapsed().as_millis() as u64,
                ))
            }
        };
        let title = match input["title"].as_str() {
            Some(t) => t,
            None => {
                return Ok(ToolResult::failure(
                    "Missing 'title' parameter",
                    start.elapsed().as_millis() as u64,
                ))
            }
        };

        let mut body_json = serde_json::json!({ "title": title });
        if let Some(body) = input["body"].as_str() {
            body_json["body"] = serde_json::Value::String(body.to_string());
        }
        if let Some(labels) = input["labels"].as_array() {
            body_json["labels"] = serde_json::Value::Array(labels.clone());
        }
        if let Some(assignees) = input["assignees"].as_array() {
            body_json["assignees"] = serde_json::Value::Array(assignees.clone());
        }

        let path = format!("/repos/{}/{}/issues", owner, repo);
        let duration_ms = start.elapsed().as_millis() as u64;

        match client.post(&path, &body_json).await {
            Ok(created) => {
                info!(
                    owner = %owner,
                    repo = %repo,
                    number = %created["number"],
                    "Created GitHub issue"
                );
                Ok(ToolResult::success(
                    serde_json::json!({
                        "status": "created",
                        "number": created["number"],
                        "title": created["title"],
                        "html_url": created["html_url"],
                    }),
                    duration_ms,
                ))
            }
            Err(e) => {
                warn!(error = %e, owner = %owner, repo = %repo, "Failed to create issue");
                Ok(ToolResult::from_report(
                    &e.wrap_err("Failed to create issue"),
                    duration_ms,
                ))
            }
        }
    }
}

// ── github.list_pulls ────────────────────────────────────────────────

pub struct GithubListPullsTool {
    clients: Arc<HashMap<String, Arc<GithubClient>>>,
    default_workspace: String,
    default_owner: Option<String>,
}

impl GithubListPullsTool {
    pub fn new(
        clients: Arc<HashMap<String, Arc<GithubClient>>>,
        default_workspace: String,
        default_owner: Option<String>,
    ) -> Self {
        Self {
            clients,
            default_workspace,
            default_owner,
        }
    }
}

#[async_trait]
impl Tool for GithubListPullsTool {
    fn name(&self) -> &str {
        "github.list_pulls"
    }

    fn description(&self) -> &str {
        "List pull requests in a GitHub repository."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "owner": {
                    "type": "string",
                    "description": owner_description(&self.clients, &self.default_workspace)
                },
                "repo": {
                    "type": "string",
                    "description": "Repository name"
                },
                "state": {
                    "type": "string",
                    "description": "Filter by state: open, closed, all (default: open)",
                    "enum": ["open", "closed", "all"]
                },
                "per_page": {
                    "type": "integer",
                    "description": "Results per page (integer, default: 30, max: 100)",
                    "minimum": 1,
                    "maximum": 100
                }
            },
            "required": ["repo"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();

        let owner = match resolve_owner(&input, &self.default_owner) {
            Ok(o) => o,
            Err(e) => return Ok(ToolResult::failure(e, start.elapsed().as_millis() as u64)),
        };
        let client = match resolve_client(&self.clients, &self.default_workspace, &input) {
            Ok(c) => c,
            Err(e) => {
                return Ok(ToolResult::failure(
                    format!("{e}"),
                    start.elapsed().as_millis() as u64,
                ))
            }
        };
        let repo = match input["repo"].as_str() {
            Some(r) => r,
            None => {
                return Ok(ToolResult::failure(
                    "Missing 'repo' parameter",
                    start.elapsed().as_millis() as u64,
                ))
            }
        };

        let mut query: Vec<(&str, &str)> = Vec::new();
        let state = input["state"].as_str().unwrap_or("open");
        query.push(("state", state));
        let per_page_str = coerce_to_u64(&input["per_page"])
            .unwrap_or(30)
            .min(100)
            .to_string();
        query.push(("per_page", &per_page_str));

        let path = format!("/repos/{}/{}/pulls", owner, repo);
        let duration_ms = start.elapsed().as_millis() as u64;

        match client.get_with_query(&path, &query).await {
            Ok(pulls) => {
                let count = pulls.as_array().map(|a| a.len()).unwrap_or(0);
                info!(owner = %owner, repo = %repo, count, "Listed GitHub PRs");
                let summary: Vec<serde_json::Value> = pulls
                    .as_array()
                    .unwrap_or(&vec![])
                    .iter()
                    .map(|p| {
                        serde_json::json!({
                            "number": p["number"],
                            "title": p["title"],
                            "state": p["state"],
                            "user": p["user"]["login"],
                            "head": p["head"]["ref"],
                            "base": p["base"]["ref"],
                            "draft": p["draft"],
                            "created_at": p["created_at"],
                            "updated_at": p["updated_at"],
                            "html_url": p["html_url"],
                        })
                    })
                    .collect();
                Ok(ToolResult::success(
                    serde_json::json!({
                        "owner": owner,
                        "repo": repo,
                        "count": count,
                        "pulls": summary,
                    }),
                    duration_ms,
                ))
            }
            Err(e) => {
                warn!(error = %e, owner = %owner, repo = %repo, "Failed to list PRs");
                Ok(ToolResult::from_report(
                    &e.wrap_err("Failed to list pull requests"),
                    duration_ms,
                ))
            }
        }
    }
}

// ── github.get_pull ──────────────────────────────────────────────────

pub struct GithubGetPullTool {
    clients: Arc<HashMap<String, Arc<GithubClient>>>,
    default_workspace: String,
    default_owner: Option<String>,
}

impl GithubGetPullTool {
    pub fn new(
        clients: Arc<HashMap<String, Arc<GithubClient>>>,
        default_workspace: String,
        default_owner: Option<String>,
    ) -> Self {
        Self {
            clients,
            default_workspace,
            default_owner,
        }
    }
}

#[async_trait]
impl Tool for GithubGetPullTool {
    fn name(&self) -> &str {
        "github.get_pull"
    }

    fn description(&self) -> &str {
        "Get details for a specific pull request, including changed files."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "owner": {
                    "type": "string",
                    "description": owner_description(&self.clients, &self.default_workspace)
                },
                "repo": {
                    "type": "string",
                    "description": "Repository name"
                },
                "number": {
                    "type": "integer",
                    "description": "Pull request number (integer, required — NOT a string)"
                }
            },
            "required": ["repo", "number"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();

        let owner = match resolve_owner(&input, &self.default_owner) {
            Ok(o) => o,
            Err(e) => return Ok(ToolResult::failure(e, start.elapsed().as_millis() as u64)),
        };
        let client = match resolve_client(&self.clients, &self.default_workspace, &input) {
            Ok(c) => c,
            Err(e) => {
                return Ok(ToolResult::failure(
                    format!("{e}"),
                    start.elapsed().as_millis() as u64,
                ))
            }
        };
        let repo = match input["repo"].as_str() {
            Some(r) => r,
            None => {
                return Ok(ToolResult::failure(
                    "Missing 'repo' parameter",
                    start.elapsed().as_millis() as u64,
                ))
            }
        };
        let number = match coerce_to_u64(&input["number"]) {
            Some(n) => n,
            None => {
                return Ok(ToolResult::failure(
                    "Missing 'number' parameter",
                    start.elapsed().as_millis() as u64,
                ))
            }
        };

        let pr_path = format!("/repos/{}/{}/pulls/{}", owner, repo, number);
        let files_path = format!("/repos/{}/{}/pulls/{}/files", owner, repo, number);
        let duration_ms = start.elapsed().as_millis() as u64;

        let pr = match client.get(&pr_path).await {
            Ok(data) => data,
            Err(e) => {
                warn!(error = %e, "Failed to get PR");
                return Ok(ToolResult::from_report(
                    &e.wrap_err("Failed to get pull request"),
                    duration_ms,
                ));
            }
        };

        let files = client.get(&files_path).await.unwrap_or_default();
        let file_summary: Vec<serde_json::Value> = files
            .as_array()
            .unwrap_or(&vec![])
            .iter()
            .map(|f| {
                serde_json::json!({
                    "filename": f["filename"],
                    "status": f["status"],
                    "additions": f["additions"],
                    "deletions": f["deletions"],
                    "changes": f["changes"],
                })
            })
            .collect();

        // Fetch review status
        let reviews_path = format!("/repos/{}/{}/pulls/{}/reviews", owner, repo, number);
        let review_summary: Vec<serde_json::Value> = client
            .get(&reviews_path)
            .await
            .ok()
            .and_then(|v| v.as_array().cloned())
            .unwrap_or_default()
            .iter()
            .map(|r| {
                serde_json::json!({
                    "user": r["user"]["login"],
                    "state": r["state"],
                })
            })
            .collect();

        // Fetch CI check status
        let head_sha = pr["head"]["sha"].as_str().unwrap_or_default();
        let checks_summary = if !head_sha.is_empty() {
            let checks_path = format!("/repos/{}/{}/commits/{}/check-runs", owner, repo, head_sha);
            match client.get(&checks_path).await {
                Ok(data) => {
                    let runs = data["check_runs"].as_array().cloned().unwrap_or_default();
                    let total = runs.len();
                    let success = runs
                        .iter()
                        .filter(|r| r["conclusion"].as_str() == Some("success"))
                        .count();
                    let failure = runs
                        .iter()
                        .filter(|r| {
                            matches!(
                                r["conclusion"].as_str(),
                                Some("failure") | Some("timed_out")
                            )
                        })
                        .count();
                    let pending = total - success - failure;
                    serde_json::json!({ "total": total, "success": success, "failure": failure, "pending": pending })
                }
                Err(_) => serde_json::json!(null),
            }
        } else {
            serde_json::json!(null)
        };

        info!(owner = %owner, repo = %repo, number, "Got GitHub PR");
        Ok(ToolResult::success(
            serde_json::json!({
                "number": pr["number"],
                "title": pr["title"],
                "state": pr["state"],
                "body": wrap_field(&pr["body"], "GitHub PR body"),
                "user": pr["user"]["login"],
                "head": pr["head"]["ref"],
                "base": pr["base"]["ref"],
                "draft": pr["draft"],
                "mergeable": pr["mergeable"],
                "additions": pr["additions"],
                "deletions": pr["deletions"],
                "changed_files": pr["changed_files"],
                "created_at": pr["created_at"],
                "updated_at": pr["updated_at"],
                "html_url": pr["html_url"],
                "files": file_summary,
                "reviews": review_summary,
                "checks": checks_summary,
            }),
            duration_ms,
        ))
    }
}

// ── github.get_file ──────────────────────────────────────────────────

pub struct GithubGetFileTool {
    clients: Arc<HashMap<String, Arc<GithubClient>>>,
    default_workspace: String,
    default_owner: Option<String>,
}

impl GithubGetFileTool {
    pub fn new(
        clients: Arc<HashMap<String, Arc<GithubClient>>>,
        default_workspace: String,
        default_owner: Option<String>,
    ) -> Self {
        Self {
            clients,
            default_workspace,
            default_owner,
        }
    }
}

#[async_trait]
impl Tool for GithubGetFileTool {
    fn name(&self) -> &str {
        "github.get_file"
    }

    fn description(&self) -> &str {
        "Get the contents of a file from a GitHub repository."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "owner": {
                    "type": "string",
                    "description": owner_description(&self.clients, &self.default_workspace)
                },
                "repo": {
                    "type": "string",
                    "description": "Repository name"
                },
                "path": {
                    "type": "string",
                    "description": "File path within the repository (e.g. 'src/main.rs')"
                },
                "ref": {
                    "type": "string",
                    "description": "Branch, tag, or commit SHA (default: repo's default branch)"
                }
            },
            "required": ["repo", "path"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();

        let owner = match resolve_owner(&input, &self.default_owner) {
            Ok(o) => o,
            Err(e) => return Ok(ToolResult::failure(e, start.elapsed().as_millis() as u64)),
        };
        let client = match resolve_client(&self.clients, &self.default_workspace, &input) {
            Ok(c) => c,
            Err(e) => {
                return Ok(ToolResult::failure(
                    format!("{e}"),
                    start.elapsed().as_millis() as u64,
                ))
            }
        };
        let repo = match input["repo"].as_str() {
            Some(r) => r,
            None => {
                return Ok(ToolResult::failure(
                    "Missing 'repo' parameter",
                    start.elapsed().as_millis() as u64,
                ))
            }
        };
        let file_path = match input["path"].as_str() {
            Some(p) => p,
            None => {
                return Ok(ToolResult::failure(
                    "Missing 'path' parameter",
                    start.elapsed().as_millis() as u64,
                ))
            }
        };

        let api_path = format!(
            "/repos/{}/{}/contents/{}",
            owner,
            repo,
            file_path.trim_start_matches('/')
        );

        let mut query: Vec<(&str, &str)> = Vec::new();
        if let Some(git_ref) = input["ref"].as_str() {
            query.push(("ref", git_ref));
        }

        let duration_ms = start.elapsed().as_millis() as u64;

        let data = if query.is_empty() {
            client.get(&api_path).await
        } else {
            client.get_with_query(&api_path, &query).await
        };

        match data {
            Ok(content) => {
                // Decode base64 content if present
                let decoded = content["content"].as_str().and_then(|encoded| {
                    use base64::Engine;
                    let cleaned = encoded.replace('\n', "");
                    base64::engine::general_purpose::STANDARD
                        .decode(cleaned)
                        .ok()
                        .and_then(|bytes| String::from_utf8(bytes).ok())
                });

                info!(owner = %owner, repo = %repo, path = %file_path, "Got GitHub file");
                Ok(ToolResult::success(
                    serde_json::json!({
                        "name": content["name"],
                        "path": content["path"],
                        "size": content["size"],
                        "sha": content["sha"],
                        "content": decoded,
                        "encoding": content["encoding"],
                        "html_url": content["html_url"],
                    }),
                    duration_ms,
                ))
            }
            Err(e) => {
                warn!(error = %e, path = %file_path, "Failed to get file");
                Ok(ToolResult::from_report(
                    &e.wrap_err("Failed to get file"),
                    duration_ms,
                ))
            }
        }
    }
}

// ── github.pr_create ────────────────────────────────────────────────

pub struct GithubPrCreateTool {
    clients: Arc<HashMap<String, Arc<GithubClient>>>,
    default_workspace: String,
    default_owner: Option<String>,
}

impl GithubPrCreateTool {
    pub fn new(
        clients: Arc<HashMap<String, Arc<GithubClient>>>,
        default_workspace: String,
        default_owner: Option<String>,
    ) -> Self {
        Self {
            clients,
            default_workspace,
            default_owner,
        }
    }
}

#[async_trait]
impl Tool for GithubPrCreateTool {
    fn name(&self) -> &str {
        "github.pr_create"
    }

    fn description(&self) -> &str {
        "Create a new pull request in a GitHub repository."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "owner": {
                    "type": "string",
                    "description": owner_description(&self.clients, &self.default_workspace)
                },
                "repo": {
                    "type": "string",
                    "description": "Repository name"
                },
                "title": {
                    "type": "string",
                    "description": "Pull request title"
                },
                "head": {
                    "type": "string",
                    "description": "Branch containing the changes"
                },
                "base": {
                    "type": "string",
                    "description": "Branch to merge into (default: 'main')"
                },
                "body": {
                    "type": "string",
                    "description": "Pull request description"
                },
                "draft": {
                    "type": "boolean",
                    "description": "Create as draft PR (default: false)"
                },
                "reviewers": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "GitHub usernames to request review from"
                },
                "labels": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Labels to add to the PR"
                }
            },
            "required": ["repo", "title", "head"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();

        let owner = match resolve_owner(&input, &self.default_owner) {
            Ok(o) => o,
            Err(e) => return Ok(ToolResult::failure(e, start.elapsed().as_millis() as u64)),
        };
        let client = match resolve_client(&self.clients, &self.default_workspace, &input) {
            Ok(c) => c,
            Err(e) => {
                return Ok(ToolResult::failure(
                    format!("{e}"),
                    start.elapsed().as_millis() as u64,
                ))
            }
        };
        let repo = match input["repo"].as_str() {
            Some(r) => r,
            None => {
                return Ok(ToolResult::failure(
                    "Missing 'repo' parameter",
                    start.elapsed().as_millis() as u64,
                ))
            }
        };
        let title = match input["title"].as_str() {
            Some(t) => t,
            None => {
                return Ok(ToolResult::failure(
                    "Missing 'title' parameter",
                    start.elapsed().as_millis() as u64,
                ))
            }
        };
        let head = match input["head"].as_str() {
            Some(h) => h,
            None => {
                return Ok(ToolResult::failure(
                    "Missing 'head' parameter",
                    start.elapsed().as_millis() as u64,
                ))
            }
        };
        let base = input["base"].as_str().unwrap_or("main");
        let draft = input["draft"].as_bool().unwrap_or(false);

        let mut body_json = serde_json::json!({
            "title": title,
            "head": head,
            "base": base,
            "draft": draft,
        });
        if let Some(pr_body) = input["body"].as_str() {
            body_json["body"] = serde_json::json!(pr_body);
        }

        let path = format!("/repos/{}/{}/pulls", owner, repo);
        let duration_ms = start.elapsed().as_millis() as u64;

        match client.post(&path, &body_json).await {
            Ok(pr) => {
                let pr_number = coerce_to_u64(&pr["number"]).unwrap_or(0);

                // Request reviewers if specified
                if let Some(reviewers) = input["reviewers"].as_array() {
                    let usernames: Vec<&str> =
                        reviewers.iter().filter_map(|v| v.as_str()).collect();
                    if !usernames.is_empty() {
                        let reviewers_path = format!(
                            "/repos/{}/{}/pulls/{}/requested_reviewers",
                            owner, repo, pr_number
                        );
                        let _ = client
                            .post(
                                &reviewers_path,
                                &serde_json::json!({ "reviewers": usernames }),
                            )
                            .await;
                    }
                }

                // Add labels if specified
                if let Some(labels) = input["labels"].as_array() {
                    let label_names: Vec<&str> = labels.iter().filter_map(|v| v.as_str()).collect();
                    if !label_names.is_empty() {
                        let labels_path =
                            format!("/repos/{}/{}/issues/{}/labels", owner, repo, pr_number);
                        let _ = client
                            .post(&labels_path, &serde_json::json!({ "labels": label_names }))
                            .await;
                    }
                }

                info!(owner = %owner, repo = %repo, number = pr_number, "Created GitHub PR");
                Ok(ToolResult::success(
                    serde_json::json!({
                        "number": pr["number"],
                        "url": pr["html_url"],
                        "state": pr["state"],
                        "draft": pr["draft"],
                    }),
                    duration_ms,
                ))
            }
            Err(e) => {
                warn!(error = %e, "Failed to create PR");
                Ok(ToolResult::from_report(
                    &e.wrap_err("Failed to create PR"),
                    duration_ms,
                ))
            }
        }
    }
}

// ── github.pr_diff ──────────────────────────────────────────────────

pub struct GithubPrDiffTool {
    clients: Arc<HashMap<String, Arc<GithubClient>>>,
    default_workspace: String,
    default_owner: Option<String>,
}

impl GithubPrDiffTool {
    pub fn new(
        clients: Arc<HashMap<String, Arc<GithubClient>>>,
        default_workspace: String,
        default_owner: Option<String>,
    ) -> Self {
        Self {
            clients,
            default_workspace,
            default_owner,
        }
    }
}

#[async_trait]
impl Tool for GithubPrDiffTool {
    fn name(&self) -> &str {
        "github.pr_diff"
    }

    fn description(&self) -> &str {
        "Get the file diffs (patches) for a pull request."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "owner": {
                    "type": "string",
                    "description": owner_description(&self.clients, &self.default_workspace)
                },
                "repo": {
                    "type": "string",
                    "description": "Repository name"
                },
                "number": {
                    "type": "integer",
                    "description": "Pull request number (integer, required — NOT a string)"
                }
            },
            "required": ["repo", "number"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();

        let owner = match resolve_owner(&input, &self.default_owner) {
            Ok(o) => o,
            Err(e) => return Ok(ToolResult::failure(e, start.elapsed().as_millis() as u64)),
        };
        let client = match resolve_client(&self.clients, &self.default_workspace, &input) {
            Ok(c) => c,
            Err(e) => {
                return Ok(ToolResult::failure(
                    format!("{e}"),
                    start.elapsed().as_millis() as u64,
                ))
            }
        };
        let repo = match input["repo"].as_str() {
            Some(r) => r,
            None => {
                return Ok(ToolResult::failure(
                    "Missing 'repo' parameter",
                    start.elapsed().as_millis() as u64,
                ))
            }
        };
        let number = match coerce_to_u64(&input["number"]) {
            Some(n) => n,
            None => {
                return Ok(ToolResult::failure(
                    "Missing 'number' parameter",
                    start.elapsed().as_millis() as u64,
                ))
            }
        };

        let path = format!("/repos/{}/{}/pulls/{}/files", owner, repo, number);
        let duration_ms = start.elapsed().as_millis() as u64;

        match client.get(&path).await {
            Ok(data) => {
                let files: Vec<serde_json::Value> = data
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
                    .into_iter()
                    .map(|f| {
                        serde_json::json!({
                            "filename": f["filename"],
                            "status": f["status"],
                            "additions": f["additions"],
                            "deletions": f["deletions"],
                            "patch": f["patch"],
                        })
                    })
                    .collect();

                info!(owner = %owner, repo = %repo, number, files = files.len(), "Got PR diff");
                Ok(ToolResult::success(
                    serde_json::json!({ "files": files }),
                    duration_ms,
                ))
            }
            Err(e) => {
                warn!(error = %e, "Failed to get PR diff");
                Ok(ToolResult::from_report(
                    &e.wrap_err("Failed to get PR diff"),
                    duration_ms,
                ))
            }
        }
    }
}

// ── github.pr_review ────────────────────────────────────────────────

pub struct GithubPrReviewTool {
    clients: Arc<HashMap<String, Arc<GithubClient>>>,
    default_workspace: String,
    default_owner: Option<String>,
}

impl GithubPrReviewTool {
    pub fn new(
        clients: Arc<HashMap<String, Arc<GithubClient>>>,
        default_workspace: String,
        default_owner: Option<String>,
    ) -> Self {
        Self {
            clients,
            default_workspace,
            default_owner,
        }
    }
}

#[async_trait]
impl Tool for GithubPrReviewTool {
    fn name(&self) -> &str {
        "github.pr_review"
    }

    fn description(&self) -> &str {
        "Submit a review on a pull request. APPROVE and REQUEST_CHANGES are submitted as the GitHub App identity when one is configured (the token cannot review its own pull requests); without an App, or when the App is not installed on the repository, the body is posted as a plain comment and the result says so (submitted: false, fallback: comment). COMMENT is always posted as the token."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "owner": {
                    "type": "string",
                    "description": owner_description(&self.clients, &self.default_workspace)
                },
                "repo": {
                    "type": "string",
                    "description": "Repository name"
                },
                "number": {
                    "type": "integer",
                    "description": "Pull request number (integer, required — NOT a string)"
                },
                "event": {
                    "type": "string",
                    "enum": ["APPROVE", "REQUEST_CHANGES", "COMMENT"],
                    "description": "Review action"
                },
                "body": {
                    "type": "string",
                    "description": "Review body/comment text"
                },
                "commit_id": {
                    "type": "string",
                    "description": "The head sha the review applies to. Defaults to the pull request's current head; pass the sha that was reviewed so a later push cannot inherit the approval."
                },
                "comments": {
                    "type": "array",
                    "description": "Inline comments on specific lines",
                    "items": {
                        "type": "object",
                        "properties": {
                            "path": {
                                "type": "string",
                                "description": "File path relative to repo root"
                            },
                            "line": {
                                "type": "integer",
                                "description": "Line number in the diff (integer — NOT a string)"
                            },
                            "body": {
                                "type": "string",
                                "description": "Comment text"
                            }
                        },
                        "required": ["path", "line", "body"]
                    }
                }
            },
            "required": ["repo", "number", "event"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();

        let owner = match resolve_owner(&input, &self.default_owner) {
            Ok(o) => o,
            Err(e) => return Ok(ToolResult::failure(e, start.elapsed().as_millis() as u64)),
        };
        let client = match resolve_client(&self.clients, &self.default_workspace, &input) {
            Ok(c) => c,
            Err(e) => {
                return Ok(ToolResult::failure(
                    format!("{e}"),
                    start.elapsed().as_millis() as u64,
                ))
            }
        };
        let repo = match input["repo"].as_str() {
            Some(r) => r,
            None => {
                return Ok(ToolResult::failure(
                    "Missing 'repo' parameter",
                    start.elapsed().as_millis() as u64,
                ))
            }
        };
        let number = match coerce_to_u64(&input["number"]) {
            Some(n) => n,
            None => {
                return Ok(ToolResult::failure(
                    "Missing 'number' parameter",
                    start.elapsed().as_millis() as u64,
                ))
            }
        };
        let event = match input["event"].as_str() {
            Some(e) => e,
            None => {
                return Ok(ToolResult::failure(
                    "Missing 'event' parameter",
                    start.elapsed().as_millis() as u64,
                ))
            }
        };

        let mut body_json = serde_json::json!({ "event": event });
        if let Some(body) = input["body"].as_str() {
            body_json["body"] = serde_json::json!(body);
        }
        if let Some(commit_id) = input["commit_id"].as_str().filter(|c| !c.is_empty()) {
            body_json["commit_id"] = serde_json::json!(commit_id);
        }
        if let Some(comments) = input["comments"].as_array() {
            let inline: Vec<serde_json::Value> = comments
                .iter()
                .filter_map(|c| {
                    Some(serde_json::json!({
                        "path": c["path"].as_str()?,
                        "line": coerce_to_u64(&c["line"])?,
                        "body": c["body"].as_str()?,
                    }))
                })
                .collect();
            if !inline.is_empty() {
                body_json["comments"] = serde_json::json!(inline);
            }
        }

        let path = format!("/repos/{}/{}/pulls/{}/reviews", owner, repo, number);
        let duration_ms = start.elapsed().as_millis() as u64;

        let submitted = |review: serde_json::Value, identity: &str| {
            info!(owner = %owner, repo = %repo, number, event, identity, "Submitted PR review");
            ToolResult::success(
                serde_json::json!({
                    "submitted": true,
                    "identity": identity,
                    "id": review["id"],
                    "state": review["state"],
                    "commit_id": review["commit_id"],
                    "html_url": review["html_url"],
                }),
                duration_ms,
            )
        };
        let failed = |e: eyre::Report| {
            warn!(error = %e, "Failed to submit PR review");
            ToolResult::from_report(&e.wrap_err("Failed to submit PR review"), duration_ms)
        };

        if !review_needs_second_identity(event) {
            return Ok(match client.post(&path, &body_json).await {
                Ok(review) => submitted(review, "token"),
                Err(e) => failed(e),
            });
        }
        let fallback_reason = match client.post_as_app(owner, repo, &path, &body_json).await {
            Ok(Some(review)) => return Ok(submitted(review, "app")),
            Ok(None) => "no GitHub App is configured for this workspace".to_string(),
            Err(AppError::NotInstalled { repo }) => {
                format!("the GitHub App is not installed on {repo}")
            }
            Err(AppError::Other(e)) => return Ok(failed(e)),
        };
        let comment = fallback_comment(event, input["body"].as_str());
        let comments = format!("/repos/{}/{}/issues/{}/comments", owner, repo, number);
        Ok(
            match client
                .post(&comments, &serde_json::json!({ "body": comment }))
                .await
            {
                Ok(posted) => {
                    info!(owner = %owner, repo = %repo, number, event, reason = %fallback_reason, "PR review posted as a comment");
                    ToolResult::success(
                        serde_json::json!({
                            "submitted": false,
                            "fallback": "comment",
                            "reason": fallback_reason,
                            "id": posted["id"],
                            "html_url": posted["html_url"],
                        }),
                        duration_ms,
                    )
                }
                Err(e) => failed(e),
            },
        )
    }
}

/// GitHub refuses an approval or a change request from the account that
/// opened the pull request, which is the token's account for every PR the
/// pipeline opens; those two events need the App identity.
fn review_needs_second_identity(event: &str) -> bool {
    matches!(event, "APPROVE" | "REQUEST_CHANGES")
}

/// What a review becomes when it cannot be submitted as a review: the same
/// text, headed by the verdict it would have carried.
fn fallback_comment(event: &str, body: Option<&str>) -> String {
    let heading = match event {
        "APPROVE" => "Approved",
        "REQUEST_CHANGES" => "Changes requested",
        other => other,
    };
    match body.map(str::trim).filter(|b| !b.is_empty()) {
        Some(body) => {
            format!("**{heading}** (review posted as a comment: no GitHub App identity)\n\n{body}")
        }
        None => format!("**{heading}** (review posted as a comment: no GitHub App identity)"),
    }
}

// ── github.pr_comment ───────────────────────────────────────────────

pub struct GithubPrCommentTool {
    clients: Arc<HashMap<String, Arc<GithubClient>>>,
    default_workspace: String,
    default_owner: Option<String>,
}

impl GithubPrCommentTool {
    pub fn new(
        clients: Arc<HashMap<String, Arc<GithubClient>>>,
        default_workspace: String,
        default_owner: Option<String>,
    ) -> Self {
        Self {
            clients,
            default_workspace,
            default_owner,
        }
    }
}

#[async_trait]
impl Tool for GithubPrCommentTool {
    fn name(&self) -> &str {
        "github.pr_comment"
    }

    fn description(&self) -> &str {
        "Add a comment to a pull request."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "owner": {
                    "type": "string",
                    "description": owner_description(&self.clients, &self.default_workspace)
                },
                "repo": {
                    "type": "string",
                    "description": "Repository name"
                },
                "number": {
                    "type": "integer",
                    "description": "Pull request number (integer, required — NOT a string)"
                },
                "body": {
                    "type": "string",
                    "description": "Comment text (Markdown supported)"
                }
            },
            "required": ["repo", "number", "body"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();

        let owner = match resolve_owner(&input, &self.default_owner) {
            Ok(o) => o,
            Err(e) => return Ok(ToolResult::failure(e, start.elapsed().as_millis() as u64)),
        };
        let client = match resolve_client(&self.clients, &self.default_workspace, &input) {
            Ok(c) => c,
            Err(e) => {
                return Ok(ToolResult::failure(
                    format!("{e}"),
                    start.elapsed().as_millis() as u64,
                ))
            }
        };
        let repo = match input["repo"].as_str() {
            Some(r) => r,
            None => {
                return Ok(ToolResult::failure(
                    "Missing 'repo' parameter",
                    start.elapsed().as_millis() as u64,
                ))
            }
        };
        let number = match coerce_to_u64(&input["number"]) {
            Some(n) => n,
            None => {
                return Ok(ToolResult::failure(
                    "Missing 'number' parameter",
                    start.elapsed().as_millis() as u64,
                ))
            }
        };
        let body = match input["body"].as_str() {
            Some(b) => b,
            None => {
                return Ok(ToolResult::failure(
                    "Missing 'body' parameter",
                    start.elapsed().as_millis() as u64,
                ))
            }
        };

        // PRs are issues in GitHub's API
        let path = format!("/repos/{}/{}/issues/{}/comments", owner, repo, number);
        let duration_ms = start.elapsed().as_millis() as u64;

        match client
            .post(&path, &serde_json::json!({ "body": body }))
            .await
        {
            Ok(comment) => {
                info!(owner = %owner, repo = %repo, number, "Added PR comment");
                Ok(ToolResult::success(
                    serde_json::json!({
                        "id": comment["id"],
                        "html_url": comment["html_url"],
                    }),
                    duration_ms,
                ))
            }
            Err(e) => {
                warn!(error = %e, "Failed to add PR comment");
                Ok(ToolResult::from_report(
                    &e.wrap_err("Failed to add PR comment"),
                    duration_ms,
                ))
            }
        }
    }
}

// ── github.pr_merge ─────────────────────────────────────────────────

pub struct GithubPrMergeTool {
    clients: Arc<HashMap<String, Arc<GithubClient>>>,
    default_workspace: String,
    default_owner: Option<String>,
}

impl GithubPrMergeTool {
    pub fn new(
        clients: Arc<HashMap<String, Arc<GithubClient>>>,
        default_workspace: String,
        default_owner: Option<String>,
    ) -> Self {
        Self {
            clients,
            default_workspace,
            default_owner,
        }
    }
}

#[async_trait]
impl Tool for GithubPrMergeTool {
    fn name(&self) -> &str {
        "github.pr_merge"
    }

    fn description(&self) -> &str {
        "Merge a GitHub pull request"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "owner": {
                    "type": "string",
                    "description": owner_description(&self.clients, &self.default_workspace)
                },
                "repo": {
                    "type": "string",
                    "description": "Repository name"
                },
                "number": {
                    "type": "integer",
                    "description": "Pull request number (integer, required — NOT a string)"
                },
                "merge_method": {
                    "type": "string",
                    "enum": ["merge", "squash", "rebase"],
                    "description": "Merge strategy (default: 'merge')"
                },
                "commit_title": {
                    "type": "string",
                    "description": "Custom commit title for merge/squash"
                },
                "commit_message": {
                    "type": "string",
                    "description": "Custom commit message for merge/squash"
                },
                "delete_branch": {
                    "type": "boolean",
                    "description": "Delete the head branch after merging (default: false)"
                },
                "sha": {
                    "type": "string",
                    "description": "Expected head commit SHA for safe merge"
                }
            },
            "required": ["repo", "number"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();

        let owner = match resolve_owner(&input, &self.default_owner) {
            Ok(o) => o,
            Err(e) => return Ok(ToolResult::failure(e, start.elapsed().as_millis() as u64)),
        };
        let client = match resolve_client(&self.clients, &self.default_workspace, &input) {
            Ok(c) => c,
            Err(e) => {
                return Ok(ToolResult::failure(
                    format!("{e}"),
                    start.elapsed().as_millis() as u64,
                ))
            }
        };
        let repo = match input["repo"].as_str() {
            Some(r) => r,
            None => {
                return Ok(ToolResult::failure(
                    "Missing 'repo' parameter",
                    start.elapsed().as_millis() as u64,
                ))
            }
        };
        let number = match coerce_to_u64(&input["number"]) {
            Some(n) => n,
            None => {
                return Ok(ToolResult::failure(
                    "Missing 'number' parameter",
                    start.elapsed().as_millis() as u64,
                ))
            }
        };

        let merge_method = input["merge_method"].as_str().unwrap_or("merge");
        let delete_branch = input["delete_branch"].as_bool().unwrap_or(false);

        // ── CI Status Gate ──────────────────────────────────────────
        // Fetch PR to get head SHA, then check CI status before merging.
        let pr_path = format!("/repos/{}/{}/pulls/{}", owner, repo, number);
        let pr_data = match client.get(&pr_path).await {
            Ok(pr) => pr,
            Err(e) => {
                return Ok(ToolResult::from_report(
                    &e.wrap_err("Failed to fetch PR"),
                    start.elapsed().as_millis() as u64,
                ));
            }
        };

        let head_sha = pr_data["head"]["sha"]
            .as_str()
            .unwrap_or_default()
            .to_string();

        if head_sha.is_empty() {
            return Ok(ToolResult::failure(
                "Could not determine PR head SHA",
                start.elapsed().as_millis() as u64,
            ));
        }

        // Check CI status using Check Runs API (not the legacy Combined Status API,
        // which defaults to "pending" when no legacy statuses exist)
        let check_runs_path = format!("/repos/{}/{}/commits/{}/check-runs", owner, repo, head_sha);
        if let Ok(check_runs_response) = client.get(&check_runs_path).await {
            let check_runs: Vec<serde_json::Value> = check_runs_response["check_runs"]
                .as_array()
                .unwrap_or(&vec![])
                .to_vec();

            let gate = evaluate_check_runs(&check_runs);

            match gate.overall.as_str() {
                "failure" => {
                    let failed_names = gate.failed_checks.join(", ");
                    return Ok(ToolResult::failure(
                        format!(
                            "Cannot merge: CI checks have failed: [{}]. All checks must pass before merging.",
                            failed_names
                        ),
                        start.elapsed().as_millis() as u64,
                    ));
                }
                "pending" => {
                    return Ok(ToolResult::failure(
                        "Cannot merge: CI checks are still pending. Wait for all checks to complete.",
                        start.elapsed().as_millis() as u64,
                    ));
                }
                _ => {} // "success" — proceed with merge
            }
        }

        let mut body_json = serde_json::json!({ "merge_method": merge_method });
        if let Some(title) = input["commit_title"].as_str() {
            body_json["commit_title"] = serde_json::json!(title);
        }
        if let Some(message) = input["commit_message"].as_str() {
            body_json["commit_message"] = serde_json::json!(message);
        }
        // Use explicit SHA if provided, otherwise auto-populate from PR head
        // to prevent race conditions.
        if let Some(sha) = input["sha"].as_str() {
            body_json["sha"] = serde_json::json!(sha);
        } else {
            body_json["sha"] = serde_json::json!(head_sha);
        }

        let path = format!("/repos/{}/{}/pulls/{}/merge", owner, repo, number);
        let duration_ms = start.elapsed().as_millis() as u64;

        match client.put(&path, &body_json).await {
            Ok(result) => {
                // Delete the branch if requested (reuse pr_data from CI check)
                if delete_branch {
                    if let Some(branch) = pr_data["head"]["ref"].as_str() {
                        let ref_path =
                            format!("/repos/{}/{}/git/refs/heads/{}", owner, repo, branch);
                        let _ = client.delete(&ref_path).await;
                    }
                }

                info!(owner = %owner, repo = %repo, number, "Merged PR");
                Ok(ToolResult::success(
                    serde_json::json!({
                        "merged": result["merged"],
                        "sha": result["sha"],
                        "message": result["message"],
                    }),
                    duration_ms,
                ))
            }
            Err(e) => {
                warn!(error = %e, "Failed to merge PR");
                Ok(ToolResult::from_report(
                    &e.wrap_err("Failed to merge PR"),
                    duration_ms,
                ))
            }
        }
    }
}

// ── github.issue_comment ──────────────────────────────────────────

pub struct GithubIssueCommentTool {
    clients: Arc<HashMap<String, Arc<GithubClient>>>,
    default_workspace: String,
    default_owner: Option<String>,
}

impl GithubIssueCommentTool {
    pub fn new(
        clients: Arc<HashMap<String, Arc<GithubClient>>>,
        default_workspace: String,
        default_owner: Option<String>,
    ) -> Self {
        Self {
            clients,
            default_workspace,
            default_owner,
        }
    }
}

#[async_trait]
impl Tool for GithubIssueCommentTool {
    fn name(&self) -> &str {
        "github.issue_comment"
    }

    fn description(&self) -> &str {
        "Add a comment to a GitHub issue or pull request."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "owner": { "type": "string", "description": owner_description(&self.clients, &self.default_workspace) },
                "repo": { "type": "string", "description": "Repository name" },
                "number": { "type": "integer", "description": "Issue or PR number (integer, required — NOT a string)" },
                "body": { "type": "string", "description": "Comment text (Markdown supported)" }
            },
            "required": ["repo", "number", "body"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();
        let owner = match resolve_owner(&input, &self.default_owner) {
            Ok(o) => o,
            Err(e) => return Ok(ToolResult::failure(e, start.elapsed().as_millis() as u64)),
        };
        let client = match resolve_client(&self.clients, &self.default_workspace, &input) {
            Ok(c) => c,
            Err(e) => {
                return Ok(ToolResult::failure(
                    format!("{e}"),
                    start.elapsed().as_millis() as u64,
                ))
            }
        };
        let repo = match input["repo"].as_str() {
            Some(r) => r,
            None => {
                return Ok(ToolResult::failure(
                    "Missing required 'repo' parameter",
                    start.elapsed().as_millis() as u64,
                ))
            }
        };
        let number = match coerce_to_u64(&input["number"]) {
            Some(n) => n,
            None => {
                return Ok(ToolResult::failure(
                    "Missing required 'number' parameter",
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

        let path = format!("/repos/{owner}/{repo}/issues/{number}/comments");
        let payload = serde_json::json!({ "body": body });
        let duration_ms = start.elapsed().as_millis() as u64;

        match client.post(&path, &payload).await {
            Ok(result) => {
                info!(owner = %owner, repo = %repo, number, "Added issue comment");
                Ok(ToolResult::success(
                    serde_json::json!({
                        "id": result["id"],
                        "html_url": result["html_url"],
                        "created_at": result["created_at"],
                    }),
                    duration_ms,
                ))
            }
            Err(e) => Ok(ToolResult::from_report(
                &e.wrap_err("Failed to add comment"),
                duration_ms,
            )),
        }
    }
}

// ── github.issue_update ──────────────────────────────────────────

pub struct GithubIssueUpdateTool {
    clients: Arc<HashMap<String, Arc<GithubClient>>>,
    default_workspace: String,
    default_owner: Option<String>,
}

impl GithubIssueUpdateTool {
    pub fn new(
        clients: Arc<HashMap<String, Arc<GithubClient>>>,
        default_workspace: String,
        default_owner: Option<String>,
    ) -> Self {
        Self {
            clients,
            default_workspace,
            default_owner,
        }
    }
}

#[async_trait]
impl Tool for GithubIssueUpdateTool {
    fn name(&self) -> &str {
        "github.issue_update"
    }

    fn description(&self) -> &str {
        "Update a GitHub issue — change state (open/closed), title, body, labels, or"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "owner": { "type": "string", "description": owner_description(&self.clients, &self.default_workspace) },
                "repo": { "type": "string", "description": "Repository name" },
                "number": { "type": "integer", "description": "Issue number (integer, required — NOT a string)" },
                "state": { "type": "string", "enum": ["open", "closed"], "description": "Issue state" },
                "title": { "type": "string", "description": "New title" },
                "body": { "type": "string", "description": "New body text" },
                "labels": { "type": "array", "items": { "type": "string" }, "description": "Labels to set (replaces existing)" },
                "assignees": { "type": "array", "items": { "type": "string" }, "description": "Assignees to set (replaces existing)" }
            },
            "required": ["repo", "number"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();
        let owner = match resolve_owner(&input, &self.default_owner) {
            Ok(o) => o,
            Err(e) => return Ok(ToolResult::failure(e, start.elapsed().as_millis() as u64)),
        };
        let client = match resolve_client(&self.clients, &self.default_workspace, &input) {
            Ok(c) => c,
            Err(e) => {
                return Ok(ToolResult::failure(
                    format!("{e}"),
                    start.elapsed().as_millis() as u64,
                ))
            }
        };
        let repo = match input["repo"].as_str() {
            Some(r) => r,
            None => {
                return Ok(ToolResult::failure(
                    "Missing required 'repo' parameter",
                    start.elapsed().as_millis() as u64,
                ))
            }
        };
        let number = match coerce_to_u64(&input["number"]) {
            Some(n) => n,
            None => {
                return Ok(ToolResult::failure(
                    "Missing required 'number' parameter",
                    start.elapsed().as_millis() as u64,
                ))
            }
        };

        let mut payload = serde_json::Map::new();
        if let Some(state) = input["state"].as_str() {
            payload.insert("state".to_string(), serde_json::json!(state));
        }
        if let Some(title) = input["title"].as_str() {
            payload.insert("title".to_string(), serde_json::json!(title));
        }
        if let Some(body) = input["body"].as_str() {
            payload.insert("body".to_string(), serde_json::json!(body));
        }
        if let Some(labels) = input["labels"].as_array() {
            payload.insert("labels".to_string(), serde_json::json!(labels));
        }
        if let Some(assignees) = input["assignees"].as_array() {
            payload.insert("assignees".to_string(), serde_json::json!(assignees));
        }

        if payload.is_empty() {
            return Ok(ToolResult::failure(
                "No fields to update. Provide at least one of: state, title, body, labels, assignees",
                start.elapsed().as_millis() as u64,
            ));
        }

        let path = format!("/repos/{owner}/{repo}/issues/{number}");
        let body = serde_json::Value::Object(payload);
        let duration_ms = start.elapsed().as_millis() as u64;

        match client.patch(&path, &body).await {
            Ok(result) => {
                info!(owner = %owner, repo = %repo, number, "Updated issue");
                Ok(ToolResult::success(
                    serde_json::json!({
                        "number": result["number"],
                        "state": result["state"],
                        "title": result["title"],
                        "html_url": result["html_url"],
                        "updated_at": result["updated_at"],
                    }),
                    duration_ms,
                ))
            }
            Err(e) => Ok(ToolResult::from_report(
                &e.wrap_err("Failed to update issue"),
                duration_ms,
            )),
        }
    }
}

// ── github.dir_list ──────────────────────────────────────────────

pub struct GithubDirListTool {
    clients: Arc<HashMap<String, Arc<GithubClient>>>,
    default_workspace: String,
    default_owner: Option<String>,
}

impl GithubDirListTool {
    pub fn new(
        clients: Arc<HashMap<String, Arc<GithubClient>>>,
        default_workspace: String,
        default_owner: Option<String>,
    ) -> Self {
        Self {
            clients,
            default_workspace,
            default_owner,
        }
    }
}

#[async_trait]
impl Tool for GithubDirListTool {
    fn name(&self) -> &str {
        "github.dir_list"
    }

    fn description(&self) -> &str {
        "List directory contents in a GitHub repository at a specific ref (branch,"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "owner": { "type": "string", "description": owner_description(&self.clients, &self.default_workspace) },
                "repo": { "type": "string", "description": "Repository name" },
                "path": { "type": "string", "description": "Directory path (e.g. 'src/components'). Defaults to root." },
                "ref": { "type": "string", "description": "Git ref — branch, tag, or commit SHA" }
            },
            "required": ["repo"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();
        let owner = match resolve_owner(&input, &self.default_owner) {
            Ok(o) => o,
            Err(e) => return Ok(ToolResult::failure(e, start.elapsed().as_millis() as u64)),
        };
        let client = match resolve_client(&self.clients, &self.default_workspace, &input) {
            Ok(c) => c,
            Err(e) => {
                return Ok(ToolResult::failure(
                    format!("{e}"),
                    start.elapsed().as_millis() as u64,
                ))
            }
        };
        let repo = match input["repo"].as_str() {
            Some(r) => r,
            None => {
                return Ok(ToolResult::failure(
                    "Missing required 'repo' parameter",
                    start.elapsed().as_millis() as u64,
                ))
            }
        };
        let path = input["path"].as_str().unwrap_or("");
        let git_ref = input["ref"].as_str();

        let api_path = format!("/repos/{owner}/{repo}/contents/{path}");
        let duration_ms = start.elapsed().as_millis() as u64;

        let result = if let Some(r) = git_ref {
            client.get_with_query(&api_path, &[("ref", r)]).await
        } else {
            client.get(&api_path).await
        };

        match result {
            Ok(value) => {
                if let Some(entries) = value.as_array() {
                    let items: Vec<serde_json::Value> = entries
                        .iter()
                        .map(|e| {
                            serde_json::json!({
                                "name": e["name"],
                                "path": e["path"],
                                "type": e["type"],
                                "size": e["size"],
                                "sha": e["sha"],
                            })
                        })
                        .collect();
                    Ok(ToolResult::success(
                        serde_json::json!({ "entries": items, "count": items.len() }),
                        duration_ms,
                    ))
                } else {
                    // Single file was returned instead of a directory
                    Ok(ToolResult::failure(
                        "Path is a file, not a directory. Use github.get_file instead.",
                        duration_ms,
                    ))
                }
            }
            Err(e) => Ok(ToolResult::from_report(
                &e.wrap_err("Failed to list directory"),
                duration_ms,
            )),
        }
    }
}

// ── github.compare ───────────────────────────────────────────────

pub struct GithubCompareTool {
    clients: Arc<HashMap<String, Arc<GithubClient>>>,
    default_workspace: String,
    default_owner: Option<String>,
}

impl GithubCompareTool {
    pub fn new(
        clients: Arc<HashMap<String, Arc<GithubClient>>>,
        default_workspace: String,
        default_owner: Option<String>,
    ) -> Self {
        Self {
            clients,
            default_workspace,
            default_owner,
        }
    }
}

#[async_trait]
impl Tool for GithubCompareTool {
    fn name(&self) -> &str {
        "github.compare"
    }

    fn description(&self) -> &str {
        "Compare two branches, tags, or commits"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "owner": { "type": "string", "description": owner_description(&self.clients, &self.default_workspace) },
                "repo": { "type": "string", "description": "Repository name" },
                "base": { "type": "string", "description": "Base branch/tag/commit" },
                "head": { "type": "string", "description": "Head branch/tag/commit" }
            },
            "required": ["repo", "base", "head"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();
        let owner = match resolve_owner(&input, &self.default_owner) {
            Ok(o) => o,
            Err(e) => return Ok(ToolResult::failure(e, start.elapsed().as_millis() as u64)),
        };
        let client = match resolve_client(&self.clients, &self.default_workspace, &input) {
            Ok(c) => c,
            Err(e) => {
                return Ok(ToolResult::failure(
                    format!("{e}"),
                    start.elapsed().as_millis() as u64,
                ))
            }
        };
        let repo = match input["repo"].as_str() {
            Some(r) => r,
            None => {
                return Ok(ToolResult::failure(
                    "Missing required 'repo' parameter",
                    start.elapsed().as_millis() as u64,
                ))
            }
        };
        let base = match input["base"].as_str() {
            Some(b) => b,
            None => {
                return Ok(ToolResult::failure(
                    "Missing required 'base' parameter",
                    start.elapsed().as_millis() as u64,
                ))
            }
        };
        let head = match input["head"].as_str() {
            Some(h) => h,
            None => {
                return Ok(ToolResult::failure(
                    "Missing required 'head' parameter",
                    start.elapsed().as_millis() as u64,
                ))
            }
        };

        let path = format!("/repos/{owner}/{repo}/compare/{base}...{head}");
        let duration_ms = start.elapsed().as_millis() as u64;

        match client.get(&path).await {
            Ok(result) => {
                let files: Vec<serde_json::Value> = result["files"]
                    .as_array()
                    .unwrap_or(&vec![])
                    .iter()
                    .map(|f| {
                        serde_json::json!({
                            "filename": f["filename"],
                            "status": f["status"],
                            "additions": f["additions"],
                            "deletions": f["deletions"],
                            "changes": f["changes"],
                        })
                    })
                    .collect();

                Ok(ToolResult::success(
                    serde_json::json!({
                        "status": result["status"],
                        "ahead_by": result["ahead_by"],
                        "behind_by": result["behind_by"],
                        "total_commits": result["total_commits"],
                        "files": files,
                        "file_count": files.len(),
                    }),
                    duration_ms,
                ))
            }
            Err(e) => Ok(ToolResult::from_report(
                &e.wrap_err("Failed to compare"),
                duration_ms,
            )),
        }
    }
}

// ── github.search_code ───────────────────────────────────────────

pub struct GithubSearchCodeTool {
    clients: Arc<HashMap<String, Arc<GithubClient>>>,
    default_workspace: String,
    default_owner: Option<String>,
}

impl GithubSearchCodeTool {
    pub fn new(
        clients: Arc<HashMap<String, Arc<GithubClient>>>,
        default_workspace: String,
        default_owner: Option<String>,
    ) -> Self {
        Self {
            clients,
            default_workspace,
            default_owner,
        }
    }
}

#[async_trait]
impl Tool for GithubSearchCodeTool {
    fn name(&self) -> &str {
        "github.search_code"
    }

    fn description(&self) -> &str {
        "Search code across GitHub repositories"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "Search query (code to find)" },
                "owner": { "type": "string", "description": owner_description(&self.clients, &self.default_workspace) },
                "repo": { "type": "string", "description": "Repository name to scope search (optional, requires owner)" },
                "language": { "type": "string", "description": "Filter by programming language (e.g. 'rust', 'python')" },
                "per_page": { "type": "integer", "description": "Results per page (integer, default: 20, max: 30)", "minimum": 1, "maximum": 30 }
            },
            "required": ["query"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();
        let client = match resolve_client(&self.clients, &self.default_workspace, &input) {
            Ok(c) => c,
            Err(e) => {
                return Ok(ToolResult::failure(
                    format!("{e}"),
                    start.elapsed().as_millis() as u64,
                ))
            }
        };
        let query = match input["query"].as_str() {
            Some(q) => q,
            None => {
                return Ok(ToolResult::failure(
                    "Missing required 'query' parameter",
                    start.elapsed().as_millis() as u64,
                ))
            }
        };

        let owner = input["owner"].as_str().or(self.default_owner.as_deref());
        let repo = input["repo"].as_str();
        let language = input["language"].as_str();
        let per_page = coerce_to_u64(&input["per_page"])
            .unwrap_or(20)
            .min(30)
            .to_string();

        // Build the search query string
        let mut q = query.to_string();
        if let (Some(owner), Some(repo)) = (owner, repo) {
            q = format!("{q} repo:{owner}/{repo}");
        } else if let Some(owner) = owner {
            q = format!("{q} org:{owner}");
        }
        if let Some(lang) = language {
            q = format!("{q} language:{lang}");
        }

        let path = "/search/code";
        let params = [("q", q.as_str()), ("per_page", &per_page)];
        let duration_ms = start.elapsed().as_millis() as u64;

        match client.get_with_query(path, &params).await {
            Ok(result) => {
                let items: Vec<serde_json::Value> = result["items"]
                    .as_array()
                    .unwrap_or(&vec![])
                    .iter()
                    .map(|item| {
                        serde_json::json!({
                            "name": item["name"],
                            "path": item["path"],
                            "repository": item["repository"]["full_name"],
                            "html_url": item["html_url"],
                            "score": item["score"],
                        })
                    })
                    .collect();

                Ok(ToolResult::success(
                    serde_json::json!({
                        "total_count": result["total_count"],
                        "items": items,
                        "returned": items.len(),
                    }),
                    duration_ms,
                ))
            }
            Err(e) => Ok(ToolResult::from_report(
                &e.wrap_err("Failed to search code"),
                duration_ms,
            )),
        }
    }
}

// ── github.search_commits ────────────────────────────────────────

/// Assemble a GitHub commit-search `q` string from structured parts.
///
/// Returns `None` when no qualifier is supplied — the Search API rejects an
/// empty query. Repeated `author:` qualifiers are OR-ed by GitHub, so an
/// `authors` list expresses "any of this team". A `repo` only applies when an
/// `owner` is known; otherwise the owner alone scopes to the whole org.
fn nonblank(s: Option<&str>) -> Option<&str> {
    s.map(str::trim).filter(|s| !s.is_empty())
}

fn build_commit_search_query(
    query: Option<&str>,
    authors: &[&str],
    owner: Option<&str>,
    repo: Option<&str>,
    since: Option<&str>,
    until: Option<&str>,
) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    if let Some(q) = nonblank(query) {
        parts.push(q.to_string());
    }
    parts.extend(
        authors
            .iter()
            .map(|a| a.trim())
            .filter(|a| !a.is_empty())
            .map(|a| format!("author:{a}")),
    );
    match (owner, repo) {
        (Some(owner), Some(repo)) => parts.push(format!("repo:{owner}/{repo}")),
        (Some(owner), None) => parts.push(format!("org:{owner}")),
        _ => {}
    }
    // One qualifier for the window: commit search silently ignores a pair of
    // `committer-date:>=` and `committer-date:<=` qualifiers (every window
    // then returns the lifetime count), while the `A..B` range form filters.
    match (nonblank(since), nonblank(until)) {
        (Some(since), Some(until)) => parts.push(format!("committer-date:{since}..{until}")),
        (Some(since), None) => parts.push(format!("committer-date:>={since}")),
        (None, Some(until)) => parts.push(format!("committer-date:<={until}")),
        (None, None) => {}
    }
    (!parts.is_empty()).then(|| parts.join(" "))
}

pub struct GithubSearchCommitsTool {
    clients: Arc<HashMap<String, Arc<GithubClient>>>,
    default_workspace: String,
    default_owner: Option<String>,
}

impl GithubSearchCommitsTool {
    pub fn new(
        clients: Arc<HashMap<String, Arc<GithubClient>>>,
        default_workspace: String,
        default_owner: Option<String>,
    ) -> Self {
        Self {
            clients,
            default_workspace,
            default_owner,
        }
    }
}

#[async_trait]
impl Tool for GithubSearchCommitsTool {
    fn name(&self) -> &str {
        "github.search_commits"
    }

    fn description(&self) -> &str {
        "Search commits across GitHub repositories by author, org/repo, and date. \
         Searches the default branch only. Results cap at 1000 — narrow since/until if total_count exceeds it."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "Free-text terms or extra qualifiers, appended verbatim" },
                "authors": { "type": "array", "items": { "type": "string" }, "description": "GitHub usernames; matches commits by any of them" },
                "owner": { "type": "string", "description": owner_description(&self.clients, &self.default_workspace) },
                "repo": { "type": "string", "description": "Repository name to scope search (optional, requires owner)" },
                "since": { "type": "string", "description": "Earliest committer date, ISO 8601 (e.g. '2026-05-20')" },
                "until": { "type": "string", "description": "Latest committer date, ISO 8601" },
                "sort": { "type": "string", "enum": ["author-date", "committer-date"], "description": "Sort field (default: best match)" },
                "order": { "type": "string", "enum": ["asc", "desc"], "description": "Sort order (default: desc)" },
                "per_page": { "type": "integer", "description": "Results per page (default: 20, max: 100)", "minimum": 1, "maximum": 100 }
            }
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();
        let client = match resolve_client(&self.clients, &self.default_workspace, &input) {
            Ok(c) => c,
            Err(e) => {
                return Ok(ToolResult::failure(
                    format!("{e}"),
                    start.elapsed().as_millis() as u64,
                ))
            }
        };

        let authors: Vec<&str> = input["authors"]
            .as_array()
            .map(|a| a.iter().filter_map(serde_json::Value::as_str).collect())
            .unwrap_or_default();
        let owner = input["owner"].as_str().or(self.default_owner.as_deref());

        let q = match build_commit_search_query(
            input["query"].as_str(),
            &authors,
            owner,
            input["repo"].as_str(),
            input["since"].as_str(),
            input["until"].as_str(),
        ) {
            Some(q) => q,
            None => {
                return Ok(ToolResult::failure(
                    "Provide at least one of: query, authors, owner, repo, since, until",
                    start.elapsed().as_millis() as u64,
                ))
            }
        };

        let per_page = coerce_to_u64(&input["per_page"])
            .unwrap_or(20)
            .clamp(1, 100)
            .to_string();
        let mut params = vec![("q", q.as_str()), ("per_page", per_page.as_str())];
        if let Some(sort) = input["sort"].as_str() {
            params.push(("sort", sort));
        }
        if let Some(order) = input["order"].as_str() {
            params.push(("order", order));
        }

        let duration_ms = start.elapsed().as_millis() as u64;
        match client.get_with_query("/search/commits", &params).await {
            Ok(result) => {
                let items: Vec<serde_json::Value> = result["items"]
                    .as_array()
                    .unwrap_or(&vec![])
                    .iter()
                    .map(|item| {
                        serde_json::json!({
                            "sha": item["sha"],
                            "message": wrap_field(&item["commit"]["message"], "github.search_commits"),
                            "author": item["commit"]["author"]["name"],
                            "login": item.get("author").and_then(|a| a.get("login")).and_then(serde_json::Value::as_str),
                            "committer_date": item["commit"]["committer"]["date"],
                            "repository": item["repository"]["full_name"],
                            "html_url": item["html_url"],
                        })
                    })
                    .collect();

                Ok(ToolResult::success(
                    serde_json::json!({
                        "total_count": result["total_count"],
                        "items": items,
                        "returned": items.len(),
                    }),
                    duration_ms,
                ))
            }
            Err(e) => Ok(ToolResult::from_report(
                &e.wrap_err("Failed to search commits"),
                duration_ms,
            )),
        }
    }
}

// ── github.branch_list ───────────────────────────────────────────

pub struct GithubBranchListTool {
    clients: Arc<HashMap<String, Arc<GithubClient>>>,
    default_workspace: String,
    default_owner: Option<String>,
}

impl GithubBranchListTool {
    pub fn new(
        clients: Arc<HashMap<String, Arc<GithubClient>>>,
        default_workspace: String,
        default_owner: Option<String>,
    ) -> Self {
        Self {
            clients,
            default_workspace,
            default_owner,
        }
    }
}

#[async_trait]
impl Tool for GithubBranchListTool {
    fn name(&self) -> &str {
        "github.branch_list"
    }

    fn description(&self) -> &str {
        "List branches in a GitHub repository."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "owner": { "type": "string", "description": owner_description(&self.clients, &self.default_workspace) },
                "repo": { "type": "string", "description": "Repository name" },
                "protected": { "type": "boolean", "description": "Filter to only protected branches" },
                "per_page": { "type": "integer", "description": "Results per page (integer, default: 30, max: 100)", "minimum": 1, "maximum": 100 }
            },
            "required": ["repo"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();
        let owner = match resolve_owner(&input, &self.default_owner) {
            Ok(o) => o,
            Err(e) => return Ok(ToolResult::failure(e, start.elapsed().as_millis() as u64)),
        };
        let client = match resolve_client(&self.clients, &self.default_workspace, &input) {
            Ok(c) => c,
            Err(e) => {
                return Ok(ToolResult::failure(
                    format!("{e}"),
                    start.elapsed().as_millis() as u64,
                ))
            }
        };
        let repo = match input["repo"].as_str() {
            Some(r) => r,
            None => {
                return Ok(ToolResult::failure(
                    "Missing required 'repo' parameter",
                    start.elapsed().as_millis() as u64,
                ))
            }
        };

        let per_page = coerce_to_u64(&input["per_page"])
            .unwrap_or(30)
            .min(100)
            .to_string();
        let path = format!("/repos/{owner}/{repo}/branches");

        let mut params: Vec<(&str, &str)> = vec![("per_page", &per_page)];
        let protected_str;
        if let Some(protected) = input["protected"].as_bool() {
            protected_str = protected.to_string();
            params.push(("protected", &protected_str));
        }

        let duration_ms = start.elapsed().as_millis() as u64;

        match client.get_with_query(&path, &params).await {
            Ok(result) => {
                let branches: Vec<serde_json::Value> = result
                    .as_array()
                    .unwrap_or(&vec![])
                    .iter()
                    .map(|b| {
                        serde_json::json!({
                            "name": b["name"],
                            "sha": b["commit"]["sha"],
                            "protected": b["protected"],
                        })
                    })
                    .collect();

                Ok(ToolResult::success(
                    serde_json::json!({
                        "branches": branches,
                        "count": branches.len(),
                    }),
                    duration_ms,
                ))
            }
            Err(e) => Ok(ToolResult::from_report(
                &e.wrap_err("Failed to list branches"),
                duration_ms,
            )),
        }
    }
}

// ── github.branch_create ─────────────────────────────────────────

pub struct GithubBranchCreateTool {
    clients: Arc<HashMap<String, Arc<GithubClient>>>,
    default_workspace: String,
    default_owner: Option<String>,
}

impl GithubBranchCreateTool {
    pub fn new(
        clients: Arc<HashMap<String, Arc<GithubClient>>>,
        default_workspace: String,
        default_owner: Option<String>,
    ) -> Self {
        Self {
            clients,
            default_workspace,
            default_owner,
        }
    }
}

#[async_trait]
impl Tool for GithubBranchCreateTool {
    fn name(&self) -> &str {
        "github.branch_create"
    }

    fn description(&self) -> &str {
        "Create a new branch from a specific ref (branch name or commit SHA)."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "owner": { "type": "string", "description": owner_description(&self.clients, &self.default_workspace) },
                "repo": { "type": "string", "description": "Repository name" },
                "branch": { "type": "string", "description": "New branch name" },
                "from": { "type": "string", "description": "Source branch name or commit SHA to branch from" }
            },
            "required": ["repo", "branch", "from"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();
        let owner = match resolve_owner(&input, &self.default_owner) {
            Ok(o) => o,
            Err(e) => return Ok(ToolResult::failure(e, start.elapsed().as_millis() as u64)),
        };
        let client = match resolve_client(&self.clients, &self.default_workspace, &input) {
            Ok(c) => c,
            Err(e) => {
                return Ok(ToolResult::failure(
                    format!("{e}"),
                    start.elapsed().as_millis() as u64,
                ))
            }
        };
        let repo = match input["repo"].as_str() {
            Some(r) => r,
            None => {
                return Ok(ToolResult::failure(
                    "Missing required 'repo' parameter",
                    start.elapsed().as_millis() as u64,
                ))
            }
        };
        let branch = match input["branch"].as_str() {
            Some(b) => b,
            None => {
                return Ok(ToolResult::failure(
                    "Missing required 'branch' parameter",
                    start.elapsed().as_millis() as u64,
                ))
            }
        };
        let from = match input["from"].as_str() {
            Some(f) => f,
            None => {
                return Ok(ToolResult::failure(
                    "Missing required 'from' parameter (branch name or SHA)",
                    start.elapsed().as_millis() as u64,
                ))
            }
        };

        // Resolve source to SHA — if it looks like a SHA (40 hex chars), use directly
        let sha = if from.len() == 40 && from.chars().all(|c| c.is_ascii_hexdigit()) {
            from.to_string()
        } else {
            // Resolve branch name to SHA
            let ref_path = format!("/repos/{owner}/{repo}/git/ref/heads/{from}");
            match client.get(&ref_path).await {
                Ok(ref_result) => match ref_result["object"]["sha"].as_str() {
                    Some(s) => s.to_string(),
                    None => {
                        return Ok(ToolResult::failure(
                            format!("Could not resolve ref '{from}' to SHA"),
                            start.elapsed().as_millis() as u64,
                        ))
                    }
                },
                Err(e) => {
                    return Ok(ToolResult::failure(
                        format!("Could not resolve ref '{from}': {e}"),
                        start.elapsed().as_millis() as u64,
                    ))
                }
            }
        };

        let path = format!("/repos/{owner}/{repo}/git/refs");
        let payload = serde_json::json!({
            "ref": format!("refs/heads/{branch}"),
            "sha": sha,
        });
        let duration_ms = start.elapsed().as_millis() as u64;

        match client.post(&path, &payload).await {
            Ok(result) => {
                info!(owner = %owner, repo = %repo, branch, "Created branch");
                Ok(ToolResult::success(
                    serde_json::json!({
                        "ref": result["ref"],
                        "sha": result["object"]["sha"],
                    }),
                    duration_ms,
                ))
            }
            Err(e) => Ok(ToolResult::from_report(
                &e.wrap_err("Failed to create branch"),
                duration_ms,
            )),
        }
    }
}

// ── github.branch_delete ─────────────────────────────────────────

pub struct GithubBranchDeleteTool {
    clients: Arc<HashMap<String, Arc<GithubClient>>>,
    default_workspace: String,
    default_owner: Option<String>,
}

impl GithubBranchDeleteTool {
    pub fn new(
        clients: Arc<HashMap<String, Arc<GithubClient>>>,
        default_workspace: String,
        default_owner: Option<String>,
    ) -> Self {
        Self {
            clients,
            default_workspace,
            default_owner,
        }
    }
}

#[async_trait]
impl Tool for GithubBranchDeleteTool {
    fn name(&self) -> &str {
        "github.branch_delete"
    }

    fn description(&self) -> &str {
        "Delete a remote branch. Cannot delete protected branches."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "owner": { "type": "string", "description": owner_description(&self.clients, &self.default_workspace) },
                "repo": { "type": "string", "description": "Repository name" },
                "branch": { "type": "string", "description": "Branch name to delete" }
            },
            "required": ["repo", "branch"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();
        let owner = match resolve_owner(&input, &self.default_owner) {
            Ok(o) => o,
            Err(e) => return Ok(ToolResult::failure(e, start.elapsed().as_millis() as u64)),
        };
        let client = match resolve_client(&self.clients, &self.default_workspace, &input) {
            Ok(c) => c,
            Err(e) => {
                return Ok(ToolResult::failure(
                    format!("{e}"),
                    start.elapsed().as_millis() as u64,
                ))
            }
        };
        let repo = match input["repo"].as_str() {
            Some(r) => r,
            None => {
                return Ok(ToolResult::failure(
                    "Missing required 'repo' parameter",
                    start.elapsed().as_millis() as u64,
                ))
            }
        };
        let branch = match input["branch"].as_str() {
            Some(b) => b,
            None => {
                return Ok(ToolResult::failure(
                    "Missing required 'branch' parameter",
                    start.elapsed().as_millis() as u64,
                ))
            }
        };

        let path = format!("/repos/{owner}/{repo}/git/refs/heads/{branch}");
        let duration_ms = start.elapsed().as_millis() as u64;

        match client.delete(&path).await {
            Ok(_) => {
                info!(owner = %owner, repo = %repo, branch, "Deleted branch");
                Ok(ToolResult::success(
                    serde_json::json!({
                        "branch": branch,
                        "deleted": true,
                    }),
                    duration_ms,
                ))
            }
            Err(e) => Ok(ToolResult::from_report(
                &e.wrap_err("Failed to delete branch"),
                duration_ms,
            )),
        }
    }
}

// ── github.branch_protection ─────────────────────────────────────

pub struct GithubBranchProtectionTool {
    clients: Arc<HashMap<String, Arc<GithubClient>>>,
    default_workspace: String,
    default_owner: Option<String>,
}

impl GithubBranchProtectionTool {
    pub fn new(
        clients: Arc<HashMap<String, Arc<GithubClient>>>,
        default_workspace: String,
        default_owner: Option<String>,
    ) -> Self {
        Self {
            clients,
            default_workspace,
            default_owner,
        }
    }
}

#[async_trait]
impl Tool for GithubBranchProtectionTool {
    fn name(&self) -> &str {
        "github.branch_protection"
    }

    fn description(&self) -> &str {
        "View branch protection rules for a branch (read-only)."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "owner": { "type": "string", "description": owner_description(&self.clients, &self.default_workspace) },
                "repo": { "type": "string", "description": "Repository name" },
                "branch": { "type": "string", "description": "Branch name (e.g. 'main')" }
            },
            "required": ["repo", "branch"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();
        let owner = match resolve_owner(&input, &self.default_owner) {
            Ok(o) => o,
            Err(e) => return Ok(ToolResult::failure(e, start.elapsed().as_millis() as u64)),
        };
        let client = match resolve_client(&self.clients, &self.default_workspace, &input) {
            Ok(c) => c,
            Err(e) => {
                return Ok(ToolResult::failure(
                    format!("{e}"),
                    start.elapsed().as_millis() as u64,
                ))
            }
        };
        let repo = match input["repo"].as_str() {
            Some(r) => r,
            None => {
                return Ok(ToolResult::failure(
                    "Missing required 'repo' parameter",
                    start.elapsed().as_millis() as u64,
                ))
            }
        };
        let branch = match input["branch"].as_str() {
            Some(b) => b,
            None => {
                return Ok(ToolResult::failure(
                    "Missing required 'branch' parameter",
                    start.elapsed().as_millis() as u64,
                ))
            }
        };

        let path = format!("/repos/{owner}/{repo}/branches/{branch}/protection");
        let duration_ms = start.elapsed().as_millis() as u64;

        match client.get(&path).await {
            Ok(result) => Ok(ToolResult::success(
                serde_json::json!({
                    "enforce_admins": result["enforce_admins"]["enabled"],
                    "required_status_checks": result["required_status_checks"],
                    "required_pull_request_reviews": {
                        "required_approving_review_count": result["required_pull_request_reviews"]["required_approving_review_count"],
                        "dismiss_stale_reviews": result["required_pull_request_reviews"]["dismiss_stale_reviews"],
                        "require_code_owner_reviews": result["required_pull_request_reviews"]["require_code_owner_reviews"],
                    },
                    "restrictions": result["restrictions"],
                    "required_linear_history": result["required_linear_history"]["enabled"],
                    "allow_force_pushes": result["allow_force_pushes"]["enabled"],
                    "allow_deletions": result["allow_deletions"]["enabled"],
                }),
                duration_ms,
            )),
            Err(e) => Ok(ToolResult::from_report(
                &e.wrap_err("Failed to get branch protection"),
                duration_ms,
            )),
        }
    }
}

// ── github.ci_status ─────────────────────────────────────────────

pub struct GithubCiStatusTool {
    clients: Arc<HashMap<String, Arc<GithubClient>>>,
    default_workspace: String,
    default_owner: Option<String>,
}

impl GithubCiStatusTool {
    pub fn new(
        clients: Arc<HashMap<String, Arc<GithubClient>>>,
        default_workspace: String,
        default_owner: Option<String>,
    ) -> Self {
        Self {
            clients,
            default_workspace,
            default_owner,
        }
    }
}

#[async_trait]
impl Tool for GithubCiStatusTool {
    fn name(&self) -> &str {
        "github.ci_status"
    }

    fn description(&self) -> &str {
        "Get CI/check run status for a git ref (branch, tag, or commit SHA)"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "owner": { "type": "string", "description": owner_description(&self.clients, &self.default_workspace) },
                "repo": { "type": "string", "description": "Repository name" },
                "ref": { "type": "string", "description": "Git ref — branch name, tag, or commit SHA" }
            },
            "required": ["repo", "ref"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();
        let owner = match resolve_owner(&input, &self.default_owner) {
            Ok(o) => o,
            Err(e) => return Ok(ToolResult::failure(e, start.elapsed().as_millis() as u64)),
        };
        let client = match resolve_client(&self.clients, &self.default_workspace, &input) {
            Ok(c) => c,
            Err(e) => {
                return Ok(ToolResult::failure(
                    format!("{e}"),
                    start.elapsed().as_millis() as u64,
                ))
            }
        };
        let repo = match input["repo"].as_str() {
            Some(r) => r,
            None => {
                return Ok(ToolResult::failure(
                    "Missing required 'repo' parameter",
                    start.elapsed().as_millis() as u64,
                ))
            }
        };
        let git_ref = match input["ref"].as_str() {
            Some(r) => r,
            None => {
                return Ok(ToolResult::failure(
                    "Missing required 'ref' parameter",
                    start.elapsed().as_millis() as u64,
                ))
            }
        };

        let path = format!("/repos/{owner}/{repo}/commits/{git_ref}/check-runs");
        let duration_ms = start.elapsed().as_millis() as u64;

        match client.get(&path).await {
            Ok(result) => {
                let check_runs: Vec<serde_json::Value> = result["check_runs"]
                    .as_array()
                    .unwrap_or(&vec![])
                    .iter()
                    .map(|cr| {
                        serde_json::json!({
                            "name": cr["name"],
                            "status": cr["status"],
                            "conclusion": cr["conclusion"],
                            "started_at": cr["started_at"],
                            "completed_at": cr["completed_at"],
                            "html_url": cr["html_url"],
                        })
                    })
                    .collect();

                // Evaluate check runs using shared CI gate logic
                let gate = evaluate_check_runs(&check_runs);

                Ok(ToolResult::success(
                    serde_json::json!({
                        "overall": gate.overall,
                        "total_count": gate.total_count,
                        "success_count": gate.success_count,
                        "skipped_count": gate.skipped_count,
                        "neutral_count": gate.neutral_count,
                        "failed_checks": gate.failed_checks,
                        "check_runs": check_runs,
                    }),
                    duration_ms,
                ))
            }
            Err(e) => Ok(ToolResult::from_report(
                &e.wrap_err("Failed to get CI status"),
                duration_ms,
            )),
        }
    }
}

// ── github.workflow_runs ─────────────────────────────────────────

pub struct GithubWorkflowRunsTool {
    clients: Arc<HashMap<String, Arc<GithubClient>>>,
    default_workspace: String,
    default_owner: Option<String>,
}

impl GithubWorkflowRunsTool {
    pub fn new(
        clients: Arc<HashMap<String, Arc<GithubClient>>>,
        default_workspace: String,
        default_owner: Option<String>,
    ) -> Self {
        Self {
            clients,
            default_workspace,
            default_owner,
        }
    }
}

#[async_trait]
impl Tool for GithubWorkflowRunsTool {
    fn name(&self) -> &str {
        "github.workflow_runs"
    }

    fn description(&self) -> &str {
        "List recent GitHub Actions workflow runs for a repository."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "owner": { "type": "string", "description": owner_description(&self.clients, &self.default_workspace) },
                "repo": { "type": "string", "description": "Repository name" },
                "branch": { "type": "string", "description": "Filter by branch name" },
                "status": { "type": "string", "enum": ["completed", "action_required", "cancelled", "failure", "neutral", "skipped", "stale", "success", "timed_out", "in_progress", "queued", "requested", "waiting", "pending"], "description": "Filter by status" },
                "per_page": { "type": "integer", "description": "Results per page (integer, default: 10, max: 30)", "minimum": 1, "maximum": 30 }
            },
            "required": ["repo"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();
        let owner = match resolve_owner(&input, &self.default_owner) {
            Ok(o) => o,
            Err(e) => return Ok(ToolResult::failure(e, start.elapsed().as_millis() as u64)),
        };
        let client = match resolve_client(&self.clients, &self.default_workspace, &input) {
            Ok(c) => c,
            Err(e) => {
                return Ok(ToolResult::failure(
                    format!("{e}"),
                    start.elapsed().as_millis() as u64,
                ))
            }
        };
        let repo = match input["repo"].as_str() {
            Some(r) => r,
            None => {
                return Ok(ToolResult::failure(
                    "Missing required 'repo' parameter",
                    start.elapsed().as_millis() as u64,
                ))
            }
        };

        let per_page = coerce_to_u64(&input["per_page"])
            .unwrap_or(10)
            .min(30)
            .to_string();
        let path = format!("/repos/{owner}/{repo}/actions/runs");

        let mut params: Vec<(&str, &str)> = vec![("per_page", &per_page)];
        let branch_val;
        if let Some(branch) = input["branch"].as_str() {
            branch_val = branch.to_string();
            params.push(("branch", &branch_val));
        }
        let status_val;
        if let Some(status) = input["status"].as_str() {
            status_val = status.to_string();
            params.push(("status", &status_val));
        }

        let duration_ms = start.elapsed().as_millis() as u64;

        match client.get_with_query(&path, &params).await {
            Ok(result) => {
                let runs: Vec<serde_json::Value> = result["workflow_runs"]
                    .as_array()
                    .unwrap_or(&vec![])
                    .iter()
                    .map(|r| {
                        serde_json::json!({
                            "id": r["id"],
                            "name": r["name"],
                            "head_branch": r["head_branch"],
                            "status": r["status"],
                            "conclusion": r["conclusion"],
                            "run_number": r["run_number"],
                            "html_url": r["html_url"],
                            "created_at": r["created_at"],
                            "updated_at": r["updated_at"],
                        })
                    })
                    .collect();

                Ok(ToolResult::success(
                    serde_json::json!({
                        "total_count": result["total_count"],
                        "runs": runs,
                        "returned": runs.len(),
                    }),
                    duration_ms,
                ))
            }
            Err(e) => Ok(ToolResult::from_report(
                &e.wrap_err("Failed to list workflow runs"),
                duration_ms,
            )),
        }
    }
}

// ── github.workflow_run_logs ─────────────────────────────────────

pub struct GithubWorkflowRunLogsTool {
    clients: Arc<HashMap<String, Arc<GithubClient>>>,
    default_workspace: String,
    default_owner: Option<String>,
}

impl GithubWorkflowRunLogsTool {
    pub fn new(
        clients: Arc<HashMap<String, Arc<GithubClient>>>,
        default_workspace: String,
        default_owner: Option<String>,
    ) -> Self {
        Self {
            clients,
            default_workspace,
            default_owner,
        }
    }
}

#[async_trait]
impl Tool for GithubWorkflowRunLogsTool {
    fn name(&self) -> &str {
        "github.workflow_run_logs"
    }

    fn description(&self) -> &str {
        "Get details and jobs for a specific workflow run"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "owner": { "type": "string", "description": owner_description(&self.clients, &self.default_workspace) },
                "repo": { "type": "string", "description": "Repository name" },
                "run_id": { "type": "integer", "description": "Workflow run ID (integer, required — NOT a string)" }
            },
            "required": ["repo", "run_id"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();
        let owner = match resolve_owner(&input, &self.default_owner) {
            Ok(o) => o,
            Err(e) => return Ok(ToolResult::failure(e, start.elapsed().as_millis() as u64)),
        };
        let client = match resolve_client(&self.clients, &self.default_workspace, &input) {
            Ok(c) => c,
            Err(e) => {
                return Ok(ToolResult::failure(
                    format!("{e}"),
                    start.elapsed().as_millis() as u64,
                ))
            }
        };
        let repo = match input["repo"].as_str() {
            Some(r) => r,
            None => {
                return Ok(ToolResult::failure(
                    "Missing required 'repo' parameter",
                    start.elapsed().as_millis() as u64,
                ))
            }
        };
        let run_id = match coerce_to_u64(&input["run_id"]) {
            Some(id) => id,
            None => {
                return Ok(ToolResult::failure(
                    "Missing required 'run_id' parameter",
                    start.elapsed().as_millis() as u64,
                ))
            }
        };

        // Get jobs for the run (more useful than raw zip logs)
        let path = format!("/repos/{owner}/{repo}/actions/runs/{run_id}/jobs");
        let duration_ms = start.elapsed().as_millis() as u64;

        match client.get(&path).await {
            Ok(result) => {
                let jobs: Vec<serde_json::Value> = result["jobs"]
                    .as_array()
                    .unwrap_or(&vec![])
                    .iter()
                    .map(|j| {
                        let steps: Vec<serde_json::Value> = j["steps"]
                            .as_array()
                            .unwrap_or(&vec![])
                            .iter()
                            .map(|s| {
                                serde_json::json!({
                                    "name": s["name"],
                                    "status": s["status"],
                                    "conclusion": s["conclusion"],
                                    "number": s["number"],
                                })
                            })
                            .collect();

                        serde_json::json!({
                            "id": j["id"],
                            "name": j["name"],
                            "status": j["status"],
                            "conclusion": j["conclusion"],
                            "started_at": j["started_at"],
                            "completed_at": j["completed_at"],
                            "html_url": j["html_url"],
                            "steps": steps,
                        })
                    })
                    .collect();

                Ok(ToolResult::success(
                    serde_json::json!({
                        "total_count": result["total_count"],
                        "jobs": jobs,
                    }),
                    duration_ms,
                ))
            }
            Err(e) => Ok(ToolResult::from_report(
                &e.wrap_err("Failed to get workflow run jobs"),
                duration_ms,
            )),
        }
    }
}

// ── github.workflow_rerun ────────────────────────────────────────

pub struct GithubWorkflowRerunTool {
    clients: Arc<HashMap<String, Arc<GithubClient>>>,
    default_workspace: String,
    default_owner: Option<String>,
}

impl GithubWorkflowRerunTool {
    pub fn new(
        clients: Arc<HashMap<String, Arc<GithubClient>>>,
        default_workspace: String,
        default_owner: Option<String>,
    ) -> Self {
        Self {
            clients,
            default_workspace,
            default_owner,
        }
    }
}

#[async_trait]
impl Tool for GithubWorkflowRerunTool {
    fn name(&self) -> &str {
        "github.workflow_rerun"
    }

    fn description(&self) -> &str {
        "Re-run a GitHub Actions workflow run. By default re-runs only failed jobs."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "owner": { "type": "string", "description": owner_description(&self.clients, &self.default_workspace) },
                "repo": { "type": "string", "description": "Repository name" },
                "run_id": { "type": "integer", "description": "Workflow run ID to re-run (integer, required — NOT a string)" },
                "all_jobs": { "type": "boolean", "description": "Re-run all jobs instead of only failed ones (default: false)" }
            },
            "required": ["repo", "run_id"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();
        let owner = match resolve_owner(&input, &self.default_owner) {
            Ok(o) => o,
            Err(e) => return Ok(ToolResult::failure(e, start.elapsed().as_millis() as u64)),
        };
        let client = match resolve_client(&self.clients, &self.default_workspace, &input) {
            Ok(c) => c,
            Err(e) => {
                return Ok(ToolResult::failure(
                    format!("{e}"),
                    start.elapsed().as_millis() as u64,
                ))
            }
        };
        let repo = match input["repo"].as_str() {
            Some(r) => r,
            None => {
                return Ok(ToolResult::failure(
                    "Missing required 'repo' parameter",
                    start.elapsed().as_millis() as u64,
                ))
            }
        };
        let run_id = match coerce_to_u64(&input["run_id"]) {
            Some(id) => id,
            None => {
                return Ok(ToolResult::failure(
                    "Missing required 'run_id' parameter",
                    start.elapsed().as_millis() as u64,
                ))
            }
        };
        let all_jobs = input["all_jobs"].as_bool().unwrap_or(false);

        let path = if all_jobs {
            format!("/repos/{owner}/{repo}/actions/runs/{run_id}/rerun")
        } else {
            format!("/repos/{owner}/{repo}/actions/runs/{run_id}/rerun-failed-jobs")
        };
        let duration_ms = start.elapsed().as_millis() as u64;

        match client.post(&path, &serde_json::json!({})).await {
            Ok(_) => {
                info!(owner = %owner, repo = %repo, run_id, "Re-triggered workflow run");
                Ok(ToolResult::success(
                    serde_json::json!({
                        "run_id": run_id,
                        "rerun": if all_jobs { "all_jobs" } else { "failed_jobs" },
                        "status": "triggered",
                    }),
                    duration_ms,
                ))
            }
            Err(e) => Ok(ToolResult::from_report(
                &e.wrap_err("Failed to re-run workflow"),
                duration_ms,
            )),
        }
    }
}

// ── github.pr_update ─────────────────────────────────────────────

pub struct GithubPrUpdateTool {
    clients: Arc<HashMap<String, Arc<GithubClient>>>,
    default_workspace: String,
    default_owner: Option<String>,
}

impl GithubPrUpdateTool {
    pub fn new(
        clients: Arc<HashMap<String, Arc<GithubClient>>>,
        default_workspace: String,
        default_owner: Option<String>,
    ) -> Self {
        Self {
            clients,
            default_workspace,
            default_owner,
        }
    }
}

#[async_trait]
impl Tool for GithubPrUpdateTool {
    fn name(&self) -> &str {
        "github.pr_update"
    }

    fn description(&self) -> &str {
        "Update a pull request — change title, body, base branch, state, labels,"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "owner": {
                    "type": "string",
                    "description": owner_description(&self.clients, &self.default_workspace)
                },
                "repo": {
                    "type": "string",
                    "description": "Repository name"
                },
                "number": {
                    "type": "integer",
                    "description": "Pull request number (integer, required — NOT a string)"
                },
                "title": {
                    "type": "string",
                    "description": "New PR title"
                },
                "body": {
                    "type": "string",
                    "description": "New PR body text (Markdown supported)"
                },
                "base": {
                    "type": "string",
                    "description": "New base branch"
                },
                "state": {
                    "type": "string",
                    "enum": ["open", "closed"],
                    "description": "PR state"
                },
                "draft": {
                    "type": "boolean",
                    "description": "Set to false to mark a draft PR as ready for review"
                },
                "labels": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Labels to set (replaces existing labels)"
                },
                "assignees": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Assignees to set (replaces existing assignees)"
                },
                "reviewers": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Reviewers to request (additive)"
                }
            },
            "required": ["repo", "number"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();

        let owner = match resolve_owner(&input, &self.default_owner) {
            Ok(o) => o,
            Err(e) => return Ok(ToolResult::failure(e, start.elapsed().as_millis() as u64)),
        };
        let client = match resolve_client(&self.clients, &self.default_workspace, &input) {
            Ok(c) => c,
            Err(e) => {
                return Ok(ToolResult::failure(
                    format!("{e}"),
                    start.elapsed().as_millis() as u64,
                ))
            }
        };
        let repo = match input["repo"].as_str() {
            Some(r) => r,
            None => {
                return Ok(ToolResult::failure(
                    "Missing required 'repo' parameter",
                    start.elapsed().as_millis() as u64,
                ))
            }
        };
        let number = match coerce_to_u64(&input["number"]) {
            Some(n) => n,
            None => {
                return Ok(ToolResult::failure(
                    "Missing required 'number' parameter",
                    start.elapsed().as_millis() as u64,
                ))
            }
        };

        let mut any_update = false;

        // PATCH the PR itself (title, body, base, state)
        let mut pr_payload = serde_json::Map::new();
        if let Some(title) = input["title"].as_str() {
            pr_payload.insert("title".to_string(), serde_json::json!(title));
        }
        if let Some(body) = input["body"].as_str() {
            pr_payload.insert("body".to_string(), serde_json::json!(body));
        }
        if let Some(base) = input["base"].as_str() {
            pr_payload.insert("base".to_string(), serde_json::json!(base));
        }
        if let Some(state) = input["state"].as_str() {
            pr_payload.insert("state".to_string(), serde_json::json!(state));
        }

        let mut results = serde_json::Map::new();

        if !pr_payload.is_empty() {
            any_update = true;
            let path = format!("/repos/{owner}/{repo}/pulls/{number}");
            let body = serde_json::Value::Object(pr_payload);
            match client.patch(&path, &body).await {
                Ok(result) => {
                    results.insert("title".to_string(), result["title"].clone());
                    results.insert("state".to_string(), result["state"].clone());
                    results.insert("html_url".to_string(), result["html_url"].clone());
                }
                Err(e) => {
                    return Ok(ToolResult::from_report(
                        &e.wrap_err("Failed to update PR"),
                        start.elapsed().as_millis() as u64,
                    ))
                }
            }
        }

        // Handle draft → ready conversion
        if let Some(draft) = input["draft"].as_bool() {
            if !draft {
                any_update = true;
                // Mark draft as ready for review
                let path = format!("/repos/{owner}/{repo}/pulls/{number}/ready-for-review");
                match client.put(&path, &serde_json::json!({})).await {
                    Ok(_) => {
                        results.insert("draft".to_string(), serde_json::json!(false));
                    }
                    Err(e) => {
                        return Ok(ToolResult::from_report(
                            &e.wrap_err("Failed to mark PR as ready for review"),
                            start.elapsed().as_millis() as u64,
                        ))
                    }
                }
            } else {
                return Ok(ToolResult::failure(
                    "Converting a ready PR back to draft is not supported via the REST API. \
                     Use the GitHub web UI or GraphQL API instead.",
                    start.elapsed().as_millis() as u64,
                ));
            }
        }

        // Handle labels (via issues endpoint, replaces all)
        if let Some(labels) = input["labels"].as_array() {
            any_update = true;
            let path = format!("/repos/{owner}/{repo}/issues/{number}/labels");
            let body = serde_json::json!({ "labels": labels });
            match client.put(&path, &body).await {
                Ok(result) => {
                    let label_names: Vec<&str> = result
                        .as_array()
                        .map(|arr| arr.iter().filter_map(|l| l["name"].as_str()).collect())
                        .unwrap_or_default();
                    results.insert("labels".to_string(), serde_json::json!(label_names));
                }
                Err(e) => {
                    return Ok(ToolResult::from_report(
                        &e.wrap_err("Failed to update labels"),
                        start.elapsed().as_millis() as u64,
                    ))
                }
            }
        }

        // Handle assignees (via issues endpoint)
        if let Some(assignees) = input["assignees"].as_array() {
            any_update = true;
            let path = format!("/repos/{owner}/{repo}/issues/{number}");
            let body = serde_json::json!({ "assignees": assignees });
            match client.patch(&path, &body).await {
                Ok(result) => {
                    let assignee_names: Vec<&str> = result["assignees"]
                        .as_array()
                        .map(|arr| arr.iter().filter_map(|a| a["login"].as_str()).collect())
                        .unwrap_or_default();
                    results.insert("assignees".to_string(), serde_json::json!(assignee_names));
                }
                Err(e) => {
                    return Ok(ToolResult::from_report(
                        &e.wrap_err("Failed to update assignees"),
                        start.elapsed().as_millis() as u64,
                    ))
                }
            }
        }

        // Handle reviewers (additive)
        if let Some(reviewers) = input["reviewers"].as_array() {
            any_update = true;
            let path = format!("/repos/{owner}/{repo}/pulls/{number}/requested_reviewers");
            let body = serde_json::json!({ "reviewers": reviewers });
            match client.post(&path, &body).await {
                Ok(_) => {
                    let reviewer_names: Vec<&str> =
                        reviewers.iter().filter_map(|r| r.as_str()).collect();
                    results.insert(
                        "requested_reviewers".to_string(),
                        serde_json::json!(reviewer_names),
                    );
                }
                Err(e) => {
                    return Ok(ToolResult::from_report(
                        &e.wrap_err("Failed to request reviewers"),
                        start.elapsed().as_millis() as u64,
                    ))
                }
            }
        }

        let duration_ms = start.elapsed().as_millis() as u64;

        if !any_update {
            return Ok(ToolResult::failure(
                "No fields to update. Provide at least one of: title, body, base, state, \
                 draft, labels, assignees, reviewers",
                duration_ms,
            ));
        }

        results.insert("number".to_string(), serde_json::json!(number));
        info!(owner = %owner, repo = %repo, number, "Updated PR");
        Ok(ToolResult::success(
            serde_json::Value::Object(results),
            duration_ms,
        ))
    }
}

// ── github.workflow_run_failed_logs ─────────────────────────────

pub struct GithubWorkflowRunFailedLogsTool {
    clients: Arc<HashMap<String, Arc<GithubClient>>>,
    default_workspace: String,
    default_owner: Option<String>,
}

impl GithubWorkflowRunFailedLogsTool {
    pub fn new(
        clients: Arc<HashMap<String, Arc<GithubClient>>>,
        default_workspace: String,
        default_owner: Option<String>,
    ) -> Self {
        Self {
            clients,
            default_workspace,
            default_owner,
        }
    }
}

#[async_trait]
impl Tool for GithubWorkflowRunFailedLogsTool {
    fn name(&self) -> &str {
        "github.workflow_run_failed_logs"
    }

    fn description(&self) -> &str {
        "Get the actual log text for failed steps in a workflow run"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "owner": {
                    "type": "string",
                    "description": owner_description(&self.clients, &self.default_workspace)
                },
                "repo": {
                    "type": "string",
                    "description": "Repository name"
                },
                "run_id": {
                    "type": "integer",
                    "description": "Workflow run ID (integer, required — NOT a string)"
                },
                "max_log_lines": {
                    "type": "integer",
                    "description": "Maximum log lines per failed job, integer (default: 200)"
                }
            },
            "required": ["repo", "run_id"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();

        let owner = match resolve_owner(&input, &self.default_owner) {
            Ok(o) => o,
            Err(e) => return Ok(ToolResult::failure(e, start.elapsed().as_millis() as u64)),
        };
        let client = match resolve_client(&self.clients, &self.default_workspace, &input) {
            Ok(c) => c,
            Err(e) => {
                return Ok(ToolResult::failure(
                    format!("{e}"),
                    start.elapsed().as_millis() as u64,
                ))
            }
        };
        let repo = match input["repo"].as_str() {
            Some(r) => r,
            None => {
                return Ok(ToolResult::failure(
                    "Missing required 'repo' parameter",
                    start.elapsed().as_millis() as u64,
                ))
            }
        };
        let run_id = match coerce_to_u64(&input["run_id"]) {
            Some(id) => id,
            None => {
                return Ok(ToolResult::failure(
                    "Missing required 'run_id' parameter",
                    start.elapsed().as_millis() as u64,
                ))
            }
        };
        let max_lines = coerce_to_u64(&input["max_log_lines"]).unwrap_or(200) as usize;

        // 1. Get all jobs for this run
        let jobs_path = format!("/repos/{owner}/{repo}/actions/runs/{run_id}/jobs");
        let jobs_response = match client.get(&jobs_path).await {
            Ok(r) => r,
            Err(e) => {
                return Ok(ToolResult::from_report(
                    &e.wrap_err("Failed to fetch jobs"),
                    start.elapsed().as_millis() as u64,
                ))
            }
        };

        let jobs = jobs_response["jobs"]
            .as_array()
            .cloned()
            .unwrap_or_default();

        // 2. Filter to failed jobs
        let failed_jobs: Vec<&serde_json::Value> = jobs
            .iter()
            .filter(|j| j["conclusion"].as_str() == Some("failure"))
            .collect();

        if failed_jobs.is_empty() {
            return Ok(ToolResult::success(
                serde_json::json!({
                    "run_id": run_id,
                    "message": "No failed jobs found in this workflow run",
                    "failed_jobs": [],
                }),
                start.elapsed().as_millis() as u64,
            ));
        }

        // 3. Fetch logs for each failed job
        let mut failed_job_results = Vec::new();

        for job in &failed_jobs {
            let job_id = match job["id"].as_u64() {
                Some(id) => id,
                None => continue,
            };
            let job_name = job["name"].as_str().unwrap_or("unknown");

            // Collect failed step names
            let failed_steps: Vec<&str> = job["steps"]
                .as_array()
                .map(|steps| {
                    steps
                        .iter()
                        .filter(|s| s["conclusion"].as_str() == Some("failure"))
                        .filter_map(|s| s["name"].as_str())
                        .collect()
                })
                .unwrap_or_default();

            // Fetch the raw log text
            let log_path = format!("/repos/{owner}/{repo}/actions/jobs/{job_id}/logs");
            let log_text = match client.get_text(&log_path).await {
                Ok(text) => {
                    // Truncate to max_lines (take last N lines for relevance)
                    let lines: Vec<&str> = text.lines().collect();
                    if lines.len() > max_lines {
                        lines[lines.len() - max_lines..].join("\n")
                    } else {
                        text
                    }
                }
                Err(e) => {
                    format!("Failed to fetch log: {e}")
                }
            };

            failed_job_results.push(serde_json::json!({
                "id": job_id,
                "name": job_name,
                "failed_steps": failed_steps,
                "log": log_text,
            }));
        }

        info!(
            owner = %owner, repo = %repo, run_id,
            failed_count = failed_job_results.len(),
            "Fetched failed workflow run logs"
        );

        Ok(ToolResult::success(
            serde_json::json!({
                "run_id": run_id,
                "failed_jobs": failed_job_results,
            }),
            start.elapsed().as_millis() as u64,
        ))
    }
}

// ── github.api ──────────────────────────────────────────────────

pub struct GithubApiTool {
    clients: Arc<HashMap<String, Arc<GithubClient>>>,
    default_workspace: String,
    #[allow(dead_code)]
    default_owner: Option<String>,
}

impl GithubApiTool {
    pub fn new(
        clients: Arc<HashMap<String, Arc<GithubClient>>>,
        default_workspace: String,
        default_owner: Option<String>,
    ) -> Self {
        Self {
            clients,
            default_workspace,
            default_owner,
        }
    }
}

#[async_trait]
impl Tool for GithubApiTool {
    fn name(&self) -> &str {
        "github.api"
    }

    fn description(&self) -> &str {
        "Make an arbitrary GitHub API request"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "method": {
                    "type": "string",
                    "enum": ["GET", "POST", "PUT", "PATCH", "DELETE"],
                    "description": "HTTP method"
                },
                "path": {
                    "type": "string",
                    "description": "API path starting with '/'"
                },
                "body": {
                    "type": "object",
                    "description": "Request body for POST/PUT/PATCH methods"
                },
                "query": {
                    "type": "object",
                    "description": "Query parameters as key-value string pairs"
                },
                "workspace": {
                    "type": "string",
                    "description": owner_description(&self.clients, &self.default_workspace)
                }
            },
            "required": ["method", "path"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();

        let client = match resolve_client(&self.clients, &self.default_workspace, &input) {
            Ok(c) => c,
            Err(e) => {
                return Ok(ToolResult::failure(
                    format!("{e}"),
                    start.elapsed().as_millis() as u64,
                ))
            }
        };

        let method = match input["method"].as_str() {
            Some(m) => m,
            None => {
                return Ok(ToolResult::failure(
                    "Missing required 'method' parameter",
                    start.elapsed().as_millis() as u64,
                ))
            }
        };
        let path = match input["path"].as_str() {
            Some(p) => p,
            None => {
                return Ok(ToolResult::failure(
                    "Missing required 'path' parameter",
                    start.elapsed().as_millis() as u64,
                ))
            }
        };

        if !path.starts_with('/') {
            return Ok(ToolResult::failure(
                "Path must start with '/' (e.g., '/repos/owner/repo/releases')",
                start.elapsed().as_millis() as u64,
            ));
        }

        let body = input.get("body").cloned().unwrap_or(serde_json::json!({}));

        let result = match method {
            "GET" => {
                if let Some(query_obj) = input["query"].as_object() {
                    let query_strings: Vec<(String, String)> = query_obj
                        .iter()
                        .map(|(k, v)| {
                            (
                                k.clone(),
                                v.as_str()
                                    .map(String::from)
                                    .unwrap_or_else(|| v.to_string()),
                            )
                        })
                        .collect();
                    let query_refs: Vec<(&str, &str)> = query_strings
                        .iter()
                        .map(|(k, v)| (k.as_str(), v.as_str()))
                        .collect();
                    client.get_with_query(path, &query_refs).await
                } else {
                    client.get(path).await
                }
            }
            "POST" => client.post(path, &body).await,
            "PUT" => client.put(path, &body).await,
            "PATCH" => client.patch(path, &body).await,
            "DELETE" => client.delete(path).await,
            _ => {
                return Ok(ToolResult::failure(
                    format!("Unsupported method: {method}"),
                    start.elapsed().as_millis() as u64,
                ))
            }
        };

        let duration_ms = start.elapsed().as_millis() as u64;

        match result {
            Ok(response) => {
                info!(method = %method, path = %path, "GitHub API request");
                Ok(ToolResult::success(response, duration_ms))
            }
            Err(e) => Ok(ToolResult::from_report(
                &e.wrap_err("GitHub API request failed"),
                duration_ms,
            )),
        }
    }
}

// ── CI gate evaluation ──────────────────────────────────────────────

/// Result of evaluating check runs for CI gating decisions.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CiGateResult {
    /// Overall status: "success", "pending", or "failure"
    overall: String,
    /// Names of check runs that failed (conclusion: failure or timed_out)
    failed_checks: Vec<String>,
    /// Total number of check runs evaluated
    total_count: usize,
    /// Count of check runs with conclusion "success"
    success_count: usize,
    /// Count of check runs with conclusion "skipped"
    skipped_count: usize,
    /// Count of check runs with conclusion "neutral"
    neutral_count: usize,
}

/// Evaluate an array of GitHub check run JSON objects to determine
/// whether CI passes, fails, or is still pending.
///
/// Completion logic:
/// - A check run is "done" if `status == "completed"`
/// - A check run "failed" if `conclusion` is `"failure"` or `"timed_out"`
/// - `skipped`, `neutral`, `success`, `cancelled` conclusions are all non-blocking
/// - If not all checks are completed → overall "pending"
/// - If any check failed → overall "failure"
/// - Otherwise → overall "success" (including when there are zero check runs)
fn evaluate_check_runs(check_runs: &[serde_json::Value]) -> CiGateResult {
    let total_count = check_runs.len();

    if total_count == 0 {
        return CiGateResult {
            overall: "success".to_string(),
            failed_checks: vec![],
            total_count: 0,
            success_count: 0,
            skipped_count: 0,
            neutral_count: 0,
        };
    }

    let all_completed = check_runs
        .iter()
        .all(|cr| cr["status"].as_str() == Some("completed"));

    let failed_checks: Vec<String> = check_runs
        .iter()
        .filter(|cr| {
            matches!(
                cr["conclusion"].as_str(),
                Some("failure") | Some("timed_out")
            )
        })
        .filter_map(|cr| cr["name"].as_str().map(String::from))
        .collect();

    let success_count = check_runs
        .iter()
        .filter(|cr| cr["conclusion"].as_str() == Some("success"))
        .count();

    let skipped_count = check_runs
        .iter()
        .filter(|cr| cr["conclusion"].as_str() == Some("skipped"))
        .count();

    let neutral_count = check_runs
        .iter()
        .filter(|cr| cr["conclusion"].as_str() == Some("neutral"))
        .count();

    let overall = if !all_completed {
        "pending"
    } else if !failed_checks.is_empty() {
        "failure"
    } else {
        // All completed, none failed — success/skipped/neutral/cancelled are all fine
        "success"
    };

    CiGateResult {
        overall: overall.to_string(),
        failed_checks,
        total_count,
        success_count,
        skipped_count,
        neutral_count,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Tool metadata tests (no HTTP needed) ────────────────────────

    fn mock_clients() -> Arc<HashMap<String, Arc<GithubClient>>> {
        let client = Arc::new(GithubClient::new("ghp_test", "https://api.github.com").unwrap());
        let mut map = HashMap::new();
        map.insert("default".to_string(), client);
        Arc::new(map)
    }

    fn default_ws() -> String {
        "default".to_string()
    }

    #[test]
    fn resolve_client_under_environment_lock() {
        use workforce_agent::runner::{ACTIVE_ENVIRONMENT, ACTIVE_SKILL_INSTANCES};
        let clients: HashMap<String, Arc<GithubClient>> = ["acme", "other"]
            .into_iter()
            .map(|n| {
                (
                    n.to_string(),
                    Arc::new(GithubClient::new("ghp", "https://api.github.com").unwrap()),
                )
            })
            .collect();
        let pins: workforce_agent::llm::environment::SkillPins =
            [("github".to_string(), vec!["acme".to_string()])].into();
        ACTIVE_ENVIRONMENT.sync_scope("harbor-prod".to_string(), || {
            ACTIVE_SKILL_INSTANCES.sync_scope(pins, || {
                let acme = clients.get("acme").unwrap();
                // No owner → the pin (acme), overriding the "other" default.
                let c = resolve_client(&clients, "other", &serde_json::json!({})).unwrap();
                assert!(Arc::ptr_eq(&c, acme));
                // An owner naming the pinned workspace resolves.
                let c = resolve_client(&clients, "other", &serde_json::json!({ "owner": "acme" }))
                    .unwrap();
                assert!(Arc::ptr_eq(&c, acme));
                // An owner naming a DIFFERENT configured workspace is refused.
                assert!(resolve_client(
                    &clients,
                    "other",
                    &serde_json::json!({ "owner": "other" })
                )
                .is_err());
                // An explicit mismatching workspace is refused.
                assert!(resolve_client(
                    &clients,
                    "other",
                    &serde_json::json!({ "workspace": "other" })
                )
                .is_err());
            });
        });
    }

    #[test]
    fn list_repos_tool_metadata() {
        let tool = GithubListReposTool::new(mock_clients(), default_ws(), None);
        assert_eq!(tool.name(), "github.list_repos");
        assert!(!tool.description().is_empty());
        let schema = tool.input_schema();
        assert_eq!(schema["type"], "object");
    }

    #[test]
    fn get_repo_tool_metadata() {
        let tool = GithubGetRepoTool::new(mock_clients(), default_ws(), None);
        assert_eq!(tool.name(), "github.get_repo");
        let schema = tool.input_schema();
        assert_eq!(schema["required"], serde_json::json!(["repo"]));
    }

    #[test]
    fn list_issues_tool_metadata() {
        let tool = GithubListIssuesTool::new(mock_clients(), default_ws(), None);
        assert_eq!(tool.name(), "github.list_issues");
        let schema = tool.input_schema();
        assert_eq!(schema["required"], serde_json::json!(["repo"]));
    }

    #[test]
    fn get_issue_tool_metadata() {
        let tool = GithubGetIssueTool::new(mock_clients(), default_ws(), None);
        assert_eq!(tool.name(), "github.get_issue");
        let schema = tool.input_schema();
        assert_eq!(schema["required"], serde_json::json!(["repo", "number"]));
    }

    #[test]
    fn create_issue_tool_metadata() {
        let tool = GithubCreateIssueTool::new(mock_clients(), default_ws(), None);
        assert_eq!(tool.name(), "github.create_issue");
        let schema = tool.input_schema();
        assert_eq!(schema["required"], serde_json::json!(["repo", "title"]));
    }

    #[test]
    fn list_pulls_tool_metadata() {
        let tool = GithubListPullsTool::new(mock_clients(), default_ws(), None);
        assert_eq!(tool.name(), "github.list_pulls");
        let schema = tool.input_schema();
        assert_eq!(schema["required"], serde_json::json!(["repo"]));
    }

    #[test]
    fn get_pull_tool_metadata() {
        let tool = GithubGetPullTool::new(mock_clients(), default_ws(), None);
        assert_eq!(tool.name(), "github.get_pull");
        let schema = tool.input_schema();
        assert_eq!(schema["required"], serde_json::json!(["repo", "number"]));
    }

    #[test]
    fn get_file_tool_metadata() {
        let tool = GithubGetFileTool::new(mock_clients(), default_ws(), None);
        assert_eq!(tool.name(), "github.get_file");
        let schema = tool.input_schema();
        assert_eq!(schema["required"], serde_json::json!(["repo", "path"]));
    }

    // ── resolve_owner tests ─────────────────────────────────────────

    #[test]
    fn resolve_owner_from_input() {
        let input = serde_json::json!({"owner": "acme"});
        assert_eq!(resolve_owner(&input, &None).unwrap(), "acme");
    }

    #[test]
    fn resolve_owner_from_default() {
        let input = serde_json::json!({});
        let default = Some("fallback-org".to_string());
        assert_eq!(resolve_owner(&input, &default).unwrap(), "fallback-org");
    }

    #[test]
    fn resolve_owner_missing_both() {
        let input = serde_json::json!({});
        assert!(resolve_owner(&input, &None).is_err());
    }

    #[test]
    fn resolve_owner_input_overrides_default() {
        let input = serde_json::json!({"owner": "explicit"});
        let default = Some("default-org".to_string());
        assert_eq!(resolve_owner(&input, &default).unwrap(), "explicit");
    }

    // ── Missing required param tests (no HTTP) ──────────────────────

    #[tokio::test]
    async fn get_repo_missing_repo_fails() {
        let tool = GithubGetRepoTool::new(mock_clients(), default_ws(), Some("acme".to_string()));
        let result = tool.execute(serde_json::json!({})).await.unwrap();
        assert!(!result.success);
    }

    #[tokio::test]
    async fn get_issue_missing_number_fails() {
        let tool = GithubGetIssueTool::new(mock_clients(), default_ws(), Some("acme".to_string()));
        let result = tool
            .execute(serde_json::json!({"repo": "test"}))
            .await
            .unwrap();
        assert!(!result.success);
    }

    #[tokio::test]
    async fn create_issue_missing_title_fails() {
        let tool =
            GithubCreateIssueTool::new(mock_clients(), default_ws(), Some("acme".to_string()));
        let result = tool
            .execute(serde_json::json!({"repo": "test"}))
            .await
            .unwrap();
        assert!(!result.success);
    }

    #[tokio::test]
    async fn list_repos_missing_owner_fails() {
        let tool = GithubListReposTool::new(mock_clients(), default_ws(), None);
        let result = tool.execute(serde_json::json!({})).await.unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("owner"));
    }

    // ── New PR tool metadata tests ──────────────────────────────────

    #[test]
    fn pr_create_tool_metadata() {
        let tool = GithubPrCreateTool::new(mock_clients(), default_ws(), None);
        assert_eq!(tool.name(), "github.pr_create");
        assert!(!tool.description().is_empty());
        let schema = tool.input_schema();
        assert_eq!(
            schema["required"],
            serde_json::json!(["repo", "title", "head"])
        );
    }

    #[test]
    fn pr_diff_tool_metadata() {
        let tool = GithubPrDiffTool::new(mock_clients(), default_ws(), None);
        assert_eq!(tool.name(), "github.pr_diff");
        let schema = tool.input_schema();
        assert_eq!(schema["required"], serde_json::json!(["repo", "number"]));
    }

    #[test]
    fn pr_review_tool_metadata() {
        let tool = GithubPrReviewTool::new(mock_clients(), default_ws(), None);
        assert_eq!(tool.name(), "github.pr_review");
        let schema = tool.input_schema();
        assert_eq!(
            schema["required"],
            serde_json::json!(["repo", "number", "event"])
        );
    }

    #[test]
    fn pr_comment_tool_metadata() {
        let tool = GithubPrCommentTool::new(mock_clients(), default_ws(), None);
        assert_eq!(tool.name(), "github.pr_comment");
        let schema = tool.input_schema();
        assert_eq!(
            schema["required"],
            serde_json::json!(["repo", "number", "body"])
        );
    }

    #[test]
    fn pr_merge_tool_metadata() {
        let tool = GithubPrMergeTool::new(mock_clients(), default_ws(), None);
        assert_eq!(tool.name(), "github.pr_merge");
        let schema = tool.input_schema();
        assert_eq!(schema["required"], serde_json::json!(["repo", "number"]));
    }

    // ── New PR tool missing param tests ─────────────────────────────

    #[tokio::test]
    async fn pr_create_missing_title_fails() {
        let tool = GithubPrCreateTool::new(mock_clients(), default_ws(), Some("acme".to_string()));
        let result = tool
            .execute(serde_json::json!({"repo": "test", "head": "feature"}))
            .await
            .unwrap();
        assert!(!result.success);
    }

    #[tokio::test]
    async fn pr_create_missing_head_fails() {
        let tool = GithubPrCreateTool::new(mock_clients(), default_ws(), Some("acme".to_string()));
        let result = tool
            .execute(serde_json::json!({"repo": "test", "title": "My PR"}))
            .await
            .unwrap();
        assert!(!result.success);
    }

    #[tokio::test]
    async fn pr_diff_missing_number_fails() {
        let tool = GithubPrDiffTool::new(mock_clients(), default_ws(), Some("acme".to_string()));
        let result = tool
            .execute(serde_json::json!({"repo": "test"}))
            .await
            .unwrap();
        assert!(!result.success);
    }

    #[tokio::test]
    async fn pr_review_missing_event_fails() {
        let tool = GithubPrReviewTool::new(mock_clients(), default_ws(), Some("acme".to_string()));
        let result = tool
            .execute(serde_json::json!({"repo": "test", "number": 1}))
            .await
            .unwrap();
        assert!(!result.success);
    }

    #[tokio::test]
    async fn pr_comment_missing_body_fails() {
        let tool = GithubPrCommentTool::new(mock_clients(), default_ws(), Some("acme".to_string()));
        let result = tool
            .execute(serde_json::json!({"repo": "test", "number": 1}))
            .await
            .unwrap();
        assert!(!result.success);
    }

    #[tokio::test]
    async fn pr_merge_missing_number_fails() {
        let tool = GithubPrMergeTool::new(mock_clients(), default_ws(), Some("acme".to_string()));
        let result = tool
            .execute(serde_json::json!({"repo": "test"}))
            .await
            .unwrap();
        assert!(!result.success);
    }

    // ── DW-115 issue tool metadata tests ────────────────────────────

    #[test]
    fn issue_comment_tool_metadata() {
        let tool = GithubIssueCommentTool::new(mock_clients(), default_ws(), None);
        assert_eq!(tool.name(), "github.issue_comment");
        assert!(!tool.description().is_empty());
        let schema = tool.input_schema();
        assert_eq!(
            schema["required"],
            serde_json::json!(["repo", "number", "body"])
        );
    }

    #[test]
    fn issue_update_tool_metadata() {
        let tool = GithubIssueUpdateTool::new(mock_clients(), default_ws(), None);
        assert_eq!(tool.name(), "github.issue_update");
        let schema = tool.input_schema();
        assert_eq!(schema["required"], serde_json::json!(["repo", "number"]));
    }

    #[tokio::test]
    async fn issue_comment_missing_body_fails() {
        let tool =
            GithubIssueCommentTool::new(mock_clients(), default_ws(), Some("acme".to_string()));
        let result = tool
            .execute(serde_json::json!({"repo": "test", "number": 1}))
            .await
            .unwrap();
        assert!(!result.success);
    }

    #[tokio::test]
    async fn issue_update_no_fields_fails() {
        let tool =
            GithubIssueUpdateTool::new(mock_clients(), default_ws(), Some("acme".to_string()));
        let result = tool
            .execute(serde_json::json!({"repo": "test", "number": 1}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("No fields to update"));
    }

    // ── DW-112 repo browsing tool metadata tests ────────────────────

    #[test]
    fn dir_list_tool_metadata() {
        let tool = GithubDirListTool::new(mock_clients(), default_ws(), None);
        assert_eq!(tool.name(), "github.dir_list");
        let schema = tool.input_schema();
        assert_eq!(schema["required"], serde_json::json!(["repo"]));
    }

    #[test]
    fn compare_tool_metadata() {
        let tool = GithubCompareTool::new(mock_clients(), default_ws(), None);
        assert_eq!(tool.name(), "github.compare");
        let schema = tool.input_schema();
        assert_eq!(
            schema["required"],
            serde_json::json!(["repo", "base", "head"])
        );
    }

    #[test]
    fn search_code_tool_metadata() {
        let tool = GithubSearchCodeTool::new(mock_clients(), default_ws(), None);
        assert_eq!(tool.name(), "github.search_code");
        let schema = tool.input_schema();
        assert_eq!(schema["required"], serde_json::json!(["query"]));
    }

    #[tokio::test]
    async fn compare_missing_base_fails() {
        let tool = GithubCompareTool::new(mock_clients(), default_ws(), Some("acme".to_string()));
        let result = tool
            .execute(serde_json::json!({"repo": "test", "head": "feature"}))
            .await
            .unwrap();
        assert!(!result.success);
    }

    #[tokio::test]
    async fn search_code_missing_query_fails() {
        let tool = GithubSearchCodeTool::new(mock_clients(), default_ws(), None);
        let result = tool.execute(serde_json::json!({})).await.unwrap();
        assert!(!result.success);
    }

    #[test]
    fn search_commits_tool_metadata() {
        let tool = GithubSearchCommitsTool::new(mock_clients(), default_ws(), None);
        assert_eq!(tool.name(), "github.search_commits");
        assert!(!tool.description().is_empty());
        assert_eq!(tool.input_schema()["type"], "object");
    }

    #[test]
    fn build_commit_query_combines_authors_org_and_dates() {
        let q = build_commit_search_query(
            None,
            &["alice", "bob"],
            Some("acme"),
            None,
            Some("2026-05-20"),
            Some("2026-05-27"),
        )
        .unwrap();
        assert_eq!(
            q,
            "author:alice author:bob org:acme committer-date:2026-05-20..2026-05-27"
        );
    }

    #[test]
    fn build_commit_query_scopes_to_repo_only_with_owner() {
        assert_eq!(
            build_commit_search_query(None, &[], Some("acme"), Some("widget"), None, None).unwrap(),
            "repo:acme/widget"
        );
        assert_eq!(
            build_commit_search_query(None, &[], None, Some("widget"), None, None),
            None
        );
    }

    #[test]
    fn build_commit_query_ignores_blank_parts_and_returns_none_when_empty() {
        assert_eq!(
            build_commit_search_query(Some("  "), &["", "  "], None, None, Some(""), None),
            None
        );
        assert_eq!(
            build_commit_search_query(Some("fix bug"), &[], None, None, None, None).unwrap(),
            "fix bug"
        );
    }

    #[tokio::test]
    async fn search_commits_no_qualifiers_fails() {
        let tool = GithubSearchCommitsTool::new(mock_clients(), default_ws(), None);
        let result = tool.execute(serde_json::json!({})).await.unwrap();
        assert!(!result.success);
    }

    // ── DW-113 branch tool metadata tests ───────────────────────────

    #[test]
    fn branch_list_tool_metadata() {
        let tool = GithubBranchListTool::new(mock_clients(), default_ws(), None);
        assert_eq!(tool.name(), "github.branch_list");
        let schema = tool.input_schema();
        assert_eq!(schema["required"], serde_json::json!(["repo"]));
    }

    #[test]
    fn branch_create_tool_metadata() {
        let tool = GithubBranchCreateTool::new(mock_clients(), default_ws(), None);
        assert_eq!(tool.name(), "github.branch_create");
        let schema = tool.input_schema();
        assert_eq!(
            schema["required"],
            serde_json::json!(["repo", "branch", "from"])
        );
    }

    #[test]
    fn branch_delete_tool_metadata() {
        let tool = GithubBranchDeleteTool::new(mock_clients(), default_ws(), None);
        assert_eq!(tool.name(), "github.branch_delete");
        let schema = tool.input_schema();
        assert_eq!(schema["required"], serde_json::json!(["repo", "branch"]));
    }

    #[test]
    fn branch_protection_tool_metadata() {
        let tool = GithubBranchProtectionTool::new(mock_clients(), default_ws(), None);
        assert_eq!(tool.name(), "github.branch_protection");
        let schema = tool.input_schema();
        assert_eq!(schema["required"], serde_json::json!(["repo", "branch"]));
    }

    #[tokio::test]
    async fn branch_create_missing_from_fails() {
        let tool =
            GithubBranchCreateTool::new(mock_clients(), default_ws(), Some("acme".to_string()));
        let result = tool
            .execute(serde_json::json!({"repo": "test", "branch": "feat"}))
            .await
            .unwrap();
        assert!(!result.success);
    }

    #[tokio::test]
    async fn branch_delete_missing_branch_fails() {
        let tool =
            GithubBranchDeleteTool::new(mock_clients(), default_ws(), Some("acme".to_string()));
        let result = tool
            .execute(serde_json::json!({"repo": "test"}))
            .await
            .unwrap();
        assert!(!result.success);
    }

    // ── DW-114 CI/Actions tool metadata tests ───────────────────────

    #[test]
    fn ci_status_tool_metadata() {
        let tool = GithubCiStatusTool::new(mock_clients(), default_ws(), None);
        assert_eq!(tool.name(), "github.ci_status");
        let schema = tool.input_schema();
        assert_eq!(schema["required"], serde_json::json!(["repo", "ref"]));
    }

    #[test]
    fn workflow_runs_tool_metadata() {
        let tool = GithubWorkflowRunsTool::new(mock_clients(), default_ws(), None);
        assert_eq!(tool.name(), "github.workflow_runs");
        let schema = tool.input_schema();
        assert_eq!(schema["required"], serde_json::json!(["repo"]));
    }

    #[test]
    fn workflow_run_logs_tool_metadata() {
        let tool = GithubWorkflowRunLogsTool::new(mock_clients(), default_ws(), None);
        assert_eq!(tool.name(), "github.workflow_run_logs");
        let schema = tool.input_schema();
        assert_eq!(schema["required"], serde_json::json!(["repo", "run_id"]));
    }

    #[test]
    fn workflow_rerun_tool_metadata() {
        let tool = GithubWorkflowRerunTool::new(mock_clients(), default_ws(), None);
        assert_eq!(tool.name(), "github.workflow_rerun");
        let schema = tool.input_schema();
        assert_eq!(schema["required"], serde_json::json!(["repo", "run_id"]));
    }

    #[tokio::test]
    async fn ci_status_missing_ref_fails() {
        let tool = GithubCiStatusTool::new(mock_clients(), default_ws(), Some("acme".to_string()));
        let result = tool
            .execute(serde_json::json!({"repo": "test"}))
            .await
            .unwrap();
        assert!(!result.success);
    }

    #[tokio::test]
    async fn workflow_run_logs_missing_run_id_fails() {
        let tool =
            GithubWorkflowRunLogsTool::new(mock_clients(), default_ws(), Some("acme".to_string()));
        let result = tool
            .execute(serde_json::json!({"repo": "test"}))
            .await
            .unwrap();
        assert!(!result.success);
    }

    #[tokio::test]
    async fn workflow_rerun_missing_run_id_fails() {
        let tool =
            GithubWorkflowRerunTool::new(mock_clients(), default_ws(), Some("acme".to_string()));
        let result = tool
            .execute(serde_json::json!({"repo": "test"}))
            .await
            .unwrap();
        assert!(!result.success);
    }

    // ── New tool metadata tests ─────────────────────────────────────

    #[test]
    fn pr_update_tool_metadata() {
        let tool = GithubPrUpdateTool::new(mock_clients(), default_ws(), None);
        assert_eq!(tool.name(), "github.pr_update");
        let schema = tool.input_schema();
        assert_eq!(schema["required"], serde_json::json!(["repo", "number"]));
    }

    #[tokio::test]
    async fn pr_update_no_fields_fails() {
        let tool = GithubPrUpdateTool::new(mock_clients(), default_ws(), Some("acme".to_string()));
        let result = tool
            .execute(serde_json::json!({"repo": "test", "number": 1}))
            .await
            .unwrap();
        assert!(!result.success);
    }

    #[test]
    fn workflow_run_failed_logs_tool_metadata() {
        let tool = GithubWorkflowRunFailedLogsTool::new(mock_clients(), default_ws(), None);
        assert_eq!(tool.name(), "github.workflow_run_failed_logs");
        let schema = tool.input_schema();
        assert_eq!(schema["required"], serde_json::json!(["repo", "run_id"]));
    }

    #[tokio::test]
    async fn workflow_run_failed_logs_missing_run_id_fails() {
        let tool = GithubWorkflowRunFailedLogsTool::new(
            mock_clients(),
            default_ws(),
            Some("acme".to_string()),
        );
        let result = tool
            .execute(serde_json::json!({"repo": "test"}))
            .await
            .unwrap();
        assert!(!result.success);
    }

    #[test]
    fn api_tool_metadata() {
        let tool = GithubApiTool::new(mock_clients(), default_ws(), None);
        assert_eq!(tool.name(), "github.api");
        let schema = tool.input_schema();
        assert_eq!(schema["required"], serde_json::json!(["method", "path"]));
    }

    #[tokio::test]
    async fn api_tool_invalid_path_fails() {
        let tool = GithubApiTool::new(mock_clients(), default_ws(), None);
        let result = tool
            .execute(serde_json::json!({"method": "GET", "path": "no-leading-slash"}))
            .await
            .unwrap();
        assert!(!result.success);
    }

    #[test]
    fn integer_params_have_type_hints_in_descriptions() {
        // DW-431: Verify that integer parameter descriptions explicitly mention
        // the integer type to guide LLM agents.
        let tool = GithubPrMergeTool::new(mock_clients(), default_ws(), None);
        let schema = tool.input_schema();
        if let Some(props) = schema["properties"].as_object() {
            for (name, prop) in props {
                if prop["type"].as_str() == Some("integer") {
                    let desc = prop["description"].as_str().unwrap_or("");
                    assert!(
                        desc.contains("integer") || desc.contains("Integer"),
                        "Param '{}' has type integer but description '{}' does not mention 'integer'",
                        name, desc
                    );
                }
            }
        }
    }

    // ── evaluate_check_runs unit tests ──────────────────────────────

    fn make_check_run(name: &str, status: &str, conclusion: Option<&str>) -> serde_json::Value {
        let mut cr = serde_json::json!({
            "name": name,
            "status": status,
        });
        if let Some(c) = conclusion {
            cr["conclusion"] = serde_json::json!(c);
        }
        cr
    }

    #[test]
    fn ci_gate_all_success() {
        let runs = vec![
            make_check_run("build", "completed", Some("success")),
            make_check_run("test", "completed", Some("success")),
            make_check_run("lint", "completed", Some("success")),
        ];
        let gate = evaluate_check_runs(&runs);
        assert_eq!(gate.overall, "success");
        assert!(gate.failed_checks.is_empty());
        assert_eq!(gate.total_count, 3);
        assert_eq!(gate.success_count, 3);
        assert_eq!(gate.skipped_count, 0);
        assert_eq!(gate.neutral_count, 0);
    }

    #[test]
    fn ci_gate_all_skipped() {
        let runs = vec![
            make_check_run("deploy-staging", "completed", Some("skipped")),
            make_check_run("deploy-prod", "completed", Some("skipped")),
        ];
        let gate = evaluate_check_runs(&runs);
        assert_eq!(gate.overall, "success");
        assert!(gate.failed_checks.is_empty());
        assert_eq!(gate.total_count, 2);
        assert_eq!(gate.success_count, 0);
        assert_eq!(gate.skipped_count, 2);
    }

    #[test]
    fn ci_gate_mixed_success_and_skipped() {
        let runs = vec![
            make_check_run("build", "completed", Some("success")),
            make_check_run("test", "completed", Some("success")),
            make_check_run("deploy-staging", "completed", Some("skipped")),
            make_check_run("deploy-prod", "completed", Some("skipped")),
        ];
        let gate = evaluate_check_runs(&runs);
        assert_eq!(gate.overall, "success");
        assert!(gate.failed_checks.is_empty());
        assert_eq!(gate.total_count, 4);
        assert_eq!(gate.success_count, 2);
        assert_eq!(gate.skipped_count, 2);
    }

    #[test]
    fn ci_gate_failure_among_success_and_skipped() {
        let runs = vec![
            make_check_run("build", "completed", Some("success")),
            make_check_run("test", "completed", Some("failure")),
            make_check_run("deploy", "completed", Some("skipped")),
        ];
        let gate = evaluate_check_runs(&runs);
        assert_eq!(gate.overall, "failure");
        assert_eq!(gate.failed_checks, vec!["test"]);
        assert_eq!(gate.total_count, 3);
        assert_eq!(gate.success_count, 1);
        assert_eq!(gate.skipped_count, 1);
    }

    #[test]
    fn ci_gate_timed_out() {
        let runs = vec![
            make_check_run("build", "completed", Some("success")),
            make_check_run("integration", "completed", Some("timed_out")),
        ];
        let gate = evaluate_check_runs(&runs);
        assert_eq!(gate.overall, "failure");
        assert_eq!(gate.failed_checks, vec!["integration"]);
    }

    #[test]
    fn ci_gate_in_progress() {
        let runs = vec![
            make_check_run("build", "completed", Some("success")),
            make_check_run("test", "in_progress", None),
        ];
        let gate = evaluate_check_runs(&runs);
        assert_eq!(gate.overall, "pending");
        assert!(gate.failed_checks.is_empty());
    }

    #[test]
    fn ci_gate_empty_check_runs() {
        let runs: Vec<serde_json::Value> = vec![];
        let gate = evaluate_check_runs(&runs);
        assert_eq!(gate.overall, "success");
        assert_eq!(gate.total_count, 0);
        assert!(gate.failed_checks.is_empty());
    }

    #[test]
    fn ci_gate_mixed_success_neutral_skipped() {
        let runs = vec![
            make_check_run("build", "completed", Some("success")),
            make_check_run("lint", "completed", Some("neutral")),
            make_check_run("deploy", "completed", Some("skipped")),
            make_check_run("test", "completed", Some("success")),
        ];
        let gate = evaluate_check_runs(&runs);
        assert_eq!(gate.overall, "success");
        assert!(gate.failed_checks.is_empty());
        assert_eq!(gate.total_count, 4);
        assert_eq!(gate.success_count, 2);
        assert_eq!(gate.neutral_count, 1);
        assert_eq!(gate.skipped_count, 1);
    }

    mod pr_review_identity {
        use super::*;
        use crate::adapters::github::app_auth::test_keys;
        use crate::adapters::github::app_auth::AppCredentials;
        use wiremock::matchers::{body_json, header_regex, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        fn clients_for(client: GithubClient) -> Arc<HashMap<String, Arc<GithubClient>>> {
            Arc::new(HashMap::from([("default".to_string(), Arc::new(client))]))
        }

        #[test]
        fn only_approvals_and_change_requests_need_the_second_identity() {
            assert!(review_needs_second_identity("APPROVE"));
            assert!(review_needs_second_identity("REQUEST_CHANGES"));
            assert!(!review_needs_second_identity("COMMENT"));
        }

        #[test]
        fn the_fallback_comment_keeps_the_verdict_and_the_text() {
            assert_eq!(
                fallback_comment("APPROVE", Some("  Merge.  ")),
                "**Approved** (review posted as a comment: no GitHub App identity)\n\nMerge."
            );
            assert_eq!(
                fallback_comment("REQUEST_CHANGES", None),
                "**Changes requested** (review posted as a comment: no GitHub App identity)"
            );
        }

        #[tokio::test]
        async fn without_an_app_an_approval_becomes_a_comment_and_says_so() {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/repos/org/repo/issues/7/comments"))
                .and(body_json(serde_json::json!({
                    "body": "**Approved** (review posted as a comment: no GitHub App identity)\n\nMerge."
                })))
                .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
                    "id": 55, "html_url": "https://github.com/org/repo/pull/7#issuecomment-55"
                })))
                .expect(1)
                .mount(&server)
                .await;
            let tool = GithubPrReviewTool::new(
                clients_for(GithubClient::new("ghp_test", &server.uri()).unwrap()),
                default_ws(),
                Some("org".into()),
            );
            let out = tool
                .execute(serde_json::json!({
                    "repo": "repo", "number": 7, "event": "APPROVE", "body": "Merge.", "commit_id": "abc"
                }))
                .await
                .unwrap();
            assert!(out.success);
            assert_eq!(out.output["submitted"], false);
            assert_eq!(out.output["fallback"], "comment");
            assert_eq!(
                out.output["reason"],
                "no GitHub App is configured for this workspace"
            );
            assert_eq!(out.output["id"], 55);
        }

        #[tokio::test]
        async fn with_an_app_the_review_is_submitted_at_the_reviewed_sha() {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/repos/org/repo/installation"))
                .respond_with(
                    ResponseTemplate::new(200).set_body_json(serde_json::json!({ "id": 3 })),
                )
                .mount(&server)
                .await;
            Mock::given(method("POST"))
                .and(path("/app/installations/3/access_tokens"))
                .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
                    "token": "ghs_x",
                    "expires_at": (chrono::Utc::now() + chrono::Duration::hours(1)).to_rfc3339()
                })))
                .mount(&server)
                .await;
            Mock::given(method("POST"))
                .and(path("/repos/org/repo/pulls/7/reviews"))
                .and(header_regex("authorization", "^Bearer ghs_x$"))
                .and(body_json(serde_json::json!({
                    "event": "REQUEST_CHANGES", "body": "Not yet.", "commit_id": "abc"
                })))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": 9, "state": "CHANGES_REQUESTED", "commit_id": "abc",
                    "html_url": "https://github.com/org/repo/pull/7#pullrequestreview-9"
                })))
                .expect(1)
                .mount(&server)
                .await;
            let client = GithubClient::new("ghp_test", &server.uri())
                .unwrap()
                .with_app(AppCredentials {
                    app_id: "1".into(),
                    private_key_pem: test_keys::PRIVATE_PEM.into(),
                });
            let tool =
                GithubPrReviewTool::new(clients_for(client), default_ws(), Some("org".into()));
            let out = tool
                .execute(serde_json::json!({
                    "repo": "repo", "number": 7, "event": "REQUEST_CHANGES", "body": "Not yet.", "commit_id": "abc"
                }))
                .await
                .unwrap();
            assert!(out.success, "{:?}", out.error);
            assert_eq!(out.output["submitted"], true);
            assert_eq!(out.output["identity"], "app");
            assert_eq!(out.output["state"], "CHANGES_REQUESTED");
        }

        #[tokio::test]
        async fn a_comment_review_stays_with_the_token() {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/repos/org/repo/pulls/7/reviews"))
                .and(header_regex("authorization", "^Bearer ghp_test$"))
                .and(body_json(
                    serde_json::json!({ "event": "COMMENT", "body": "note" }),
                ))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": 2, "state": "COMMENTED"
                })))
                .expect(1)
                .mount(&server)
                .await;
            let tool = GithubPrReviewTool::new(
                clients_for(GithubClient::new("ghp_test", &server.uri()).unwrap()),
                default_ws(),
                Some("org".into()),
            );
            let out = tool
                .execute(serde_json::json!({ "repo": "repo", "number": 7, "event": "COMMENT", "body": "note" }))
                .await
                .unwrap();
            assert!(out.success);
            assert_eq!(out.output["identity"], "token");
        }
    }
}
