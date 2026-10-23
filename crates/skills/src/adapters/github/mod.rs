//! GitHub skill — agent-callable tools for interacting with the GitHub API.
//!
//! Provides tools for listing repos, managing issues, pull requests, and
//! reading file contents. Authentication is via PAT stored in the secrets
//! vault or environment variables.
//! Supports both single-token and multi-org configurations.

pub mod app_auth;
pub mod client;
pub mod config;
pub mod event_handler;
pub mod tools;

use std::collections::HashMap;
use std::sync::Arc;

use workforce_agent::tools::Tool;

use crate::adapters::utils::{build_instances, Instances};
use crate::definition::{
    CredentialRequirement, SkillDefinition, SkillEventSource, SkillPolicy, SkillRequirements,
    SkillTool,
};

pub use config::GithubSkillConfig;

/// Read-only GitHub tools — never mutate the remote. The single source of truth
/// for the worker-acquirable GitHub read surface (see the hub's
/// `worker_read` catalogue). Explicit names, never a `github.*` glob: the
/// surface mixes reads and writes under one prefix.
pub const READ_TOOLS: &[&str] = &[
    "github.list_repos",
    "github.get_repo",
    "github.list_issues",
    "github.get_issue",
    "github.list_pulls",
    "github.get_pull",
    "github.get_file",
    "github.pr_diff",
    "github.dir_list",
    "github.compare",
    "github.search_code",
    "github.search_commits",
    "github.branch_list",
    "github.branch_protection",
    "github.ci_status",
    "github.workflow_runs",
    "github.workflow_run_logs",
    "github.workflow_run_failed_logs",
];

/// Write/mutating GitHub tools. `github.workflow_rerun` triggers a workflow and
/// `github.api` is an arbitrary-request passthrough, so both are writes despite
/// reading-sounding names.
pub const WRITE_TOOLS: &[&str] = &[
    "github.create_issue",
    "github.pr_create",
    "github.pr_review",
    "github.pr_comment",
    "github.pr_merge",
    "github.issue_comment",
    "github.issue_update",
    "github.branch_create",
    "github.branch_delete",
    "github.workflow_rerun",
    "github.pr_update",
    "github.api",
];

/// The GitHub skill — provides tools for interacting with the GitHub REST API.
pub struct GithubSkill {
    clients: Arc<HashMap<String, Arc<client::GithubClient>>>,
    default_workspace: String,
    default_owner: Option<String>,
}

impl GithubSkill {
    /// Create from config with static credentials. Use
    /// [`GithubSkill::with_store`] for per-user resolution.
    pub fn new(config: GithubSkillConfig) -> eyre::Result<Self> {
        Self::build(config, |_, ws| {
            client::GithubClient::new(&ws.token, &ws.base_url)
        })
    }

    /// Build a `GithubSkill` whose clients resolve PATs from `store` per
    /// request. Pass a `LayeredUserVault` for per-user resolution.
    pub fn with_store(
        config: GithubSkillConfig,
        store: Arc<dyn workforce_common::settings::SecretStore>,
    ) -> eyre::Result<Self> {
        Self::build(config, |name, ws| {
            client::GithubClient::with_store(Arc::clone(&store), name, &ws.base_url)
        })
    }

    fn build(
        config: GithubSkillConfig,
        make: impl Fn(&str, &config::GithubWorkspaceConfig) -> eyre::Result<client::GithubClient>,
    ) -> eyre::Result<Self> {
        let resolved = config.resolve();
        let Instances { clients, default } = build_instances(
            "github",
            &resolved,
            config.default_workspace.as_deref(),
            "No GitHub workspaces configured. Set token or workspaces.",
            make,
        )?;
        let default_owner = resolved
            .iter()
            .find(|(name, _)| *name == default)
            .and_then(|(_, ws)| ws.default_owner.clone());
        Ok(Self {
            clients,
            default_workspace: default,
            default_owner,
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
        vec![
            Arc::new(tools::GithubListReposTool::new(
                Arc::clone(&self.clients),
                self.default_workspace.clone(),
                self.default_owner.clone(),
            )),
            Arc::new(tools::GithubGetRepoTool::new(
                Arc::clone(&self.clients),
                self.default_workspace.clone(),
                self.default_owner.clone(),
            )),
            Arc::new(tools::GithubListIssuesTool::new(
                Arc::clone(&self.clients),
                self.default_workspace.clone(),
                self.default_owner.clone(),
            )),
            Arc::new(tools::GithubGetIssueTool::new(
                Arc::clone(&self.clients),
                self.default_workspace.clone(),
                self.default_owner.clone(),
            )),
            Arc::new(tools::GithubCreateIssueTool::new(
                Arc::clone(&self.clients),
                self.default_workspace.clone(),
                self.default_owner.clone(),
            )),
            Arc::new(tools::GithubListPullsTool::new(
                Arc::clone(&self.clients),
                self.default_workspace.clone(),
                self.default_owner.clone(),
            )),
            Arc::new(tools::GithubGetPullTool::new(
                Arc::clone(&self.clients),
                self.default_workspace.clone(),
                self.default_owner.clone(),
            )),
            Arc::new(tools::GithubGetFileTool::new(
                Arc::clone(&self.clients),
                self.default_workspace.clone(),
                self.default_owner.clone(),
            )),
            Arc::new(tools::GithubPrCreateTool::new(
                Arc::clone(&self.clients),
                self.default_workspace.clone(),
                self.default_owner.clone(),
            )),
            Arc::new(tools::GithubPrDiffTool::new(
                Arc::clone(&self.clients),
                self.default_workspace.clone(),
                self.default_owner.clone(),
            )),
            Arc::new(tools::GithubPrReviewTool::new(
                Arc::clone(&self.clients),
                self.default_workspace.clone(),
                self.default_owner.clone(),
            )),
            Arc::new(tools::GithubPrCommentTool::new(
                Arc::clone(&self.clients),
                self.default_workspace.clone(),
                self.default_owner.clone(),
            )),
            Arc::new(tools::GithubPrMergeTool::new(
                Arc::clone(&self.clients),
                self.default_workspace.clone(),
                self.default_owner.clone(),
            )),
            // DW-115: Issue management
            Arc::new(tools::GithubIssueCommentTool::new(
                Arc::clone(&self.clients),
                self.default_workspace.clone(),
                self.default_owner.clone(),
            )),
            Arc::new(tools::GithubIssueUpdateTool::new(
                Arc::clone(&self.clients),
                self.default_workspace.clone(),
                self.default_owner.clone(),
            )),
            // DW-112: Repo browsing
            Arc::new(tools::GithubDirListTool::new(
                Arc::clone(&self.clients),
                self.default_workspace.clone(),
                self.default_owner.clone(),
            )),
            Arc::new(tools::GithubCompareTool::new(
                Arc::clone(&self.clients),
                self.default_workspace.clone(),
                self.default_owner.clone(),
            )),
            Arc::new(tools::GithubSearchCodeTool::new(
                Arc::clone(&self.clients),
                self.default_workspace.clone(),
                self.default_owner.clone(),
            )),
            Arc::new(tools::GithubSearchCommitsTool::new(
                Arc::clone(&self.clients),
                self.default_workspace.clone(),
                self.default_owner.clone(),
            )),
            // DW-113: Branch management
            Arc::new(tools::GithubBranchListTool::new(
                Arc::clone(&self.clients),
                self.default_workspace.clone(),
                self.default_owner.clone(),
            )),
            Arc::new(tools::GithubBranchCreateTool::new(
                Arc::clone(&self.clients),
                self.default_workspace.clone(),
                self.default_owner.clone(),
            )),
            Arc::new(tools::GithubBranchDeleteTool::new(
                Arc::clone(&self.clients),
                self.default_workspace.clone(),
                self.default_owner.clone(),
            )),
            Arc::new(tools::GithubBranchProtectionTool::new(
                Arc::clone(&self.clients),
                self.default_workspace.clone(),
                self.default_owner.clone(),
            )),
            // DW-114: CI/Actions
            Arc::new(tools::GithubCiStatusTool::new(
                Arc::clone(&self.clients),
                self.default_workspace.clone(),
                self.default_owner.clone(),
            )),
            Arc::new(tools::GithubWorkflowRunsTool::new(
                Arc::clone(&self.clients),
                self.default_workspace.clone(),
                self.default_owner.clone(),
            )),
            Arc::new(tools::GithubWorkflowRunLogsTool::new(
                Arc::clone(&self.clients),
                self.default_workspace.clone(),
                self.default_owner.clone(),
            )),
            Arc::new(tools::GithubWorkflowRerunTool::new(
                Arc::clone(&self.clients),
                self.default_workspace.clone(),
                self.default_owner.clone(),
            )),
            // PR update, failed logs, raw API
            Arc::new(tools::GithubPrUpdateTool::new(
                Arc::clone(&self.clients),
                self.default_workspace.clone(),
                self.default_owner.clone(),
            )),
            Arc::new(tools::GithubWorkflowRunFailedLogsTool::new(
                Arc::clone(&self.clients),
                self.default_workspace.clone(),
                self.default_owner.clone(),
            )),
            Arc::new(tools::GithubApiTool::new(
                Arc::clone(&self.clients),
                self.default_workspace.clone(),
                self.default_owner.clone(),
            )),
        ]
    }

    /// Tool names provided by this skill, in registration order.
    pub fn tool_names() -> Vec<&'static str> {
        vec![
            "github.list_repos",
            "github.get_repo",
            "github.list_issues",
            "github.get_issue",
            "github.create_issue",
            "github.list_pulls",
            "github.get_pull",
            "github.get_file",
            "github.pr_create",
            "github.pr_diff",
            "github.pr_review",
            "github.pr_comment",
            "github.pr_merge",
            "github.issue_comment",
            "github.issue_update",
            "github.dir_list",
            "github.compare",
            "github.search_code",
            "github.search_commits",
            "github.branch_list",
            "github.branch_create",
            "github.branch_delete",
            "github.branch_protection",
            "github.ci_status",
            "github.workflow_runs",
            "github.workflow_run_logs",
            "github.workflow_rerun",
            "github.pr_update",
            "github.workflow_run_failed_logs",
            "github.api",
        ]
    }

    /// Skill definition for the GitHub integration.
    pub fn definition() -> SkillDefinition {
        SkillDefinition {
            name: "github-integration".to_string(),
            version: "1.0.0".to_string(),
            description: "Interact with GitHub repositories, issues, pull requests, and files"
                .to_string(),
            author: "workforce".to_string(),
            license: "MIT".to_string(),

            tools: vec![
                SkillTool {
                    name: "github.list_repos".to_string(),
                    description: "List repositories for a user or organisation".to_string(),
                    input_schema: serde_json::json!({
                        "type": "object",
                        "properties": {
                            "owner": { "type": "string" },
                            "sort": { "type": "string" },
                            "per_page": { "type": "integer" }
                        }
                    }),
                },
                SkillTool {
                    name: "github.get_repo".to_string(),
                    description: "Get details about a repository".to_string(),
                    input_schema: serde_json::json!({
                        "type": "object",
                        "properties": {
                            "owner": { "type": "string" },
                            "repo": { "type": "string" }
                        },
                        "required": ["repo"]
                    }),
                },
                SkillTool {
                    name: "github.list_issues".to_string(),
                    description: "List issues in a repository".to_string(),
                    input_schema: serde_json::json!({
                        "type": "object",
                        "properties": {
                            "owner": { "type": "string" },
                            "repo": { "type": "string" },
                            "state": { "type": "string" }
                        },
                        "required": ["repo"]
                    }),
                },
                SkillTool {
                    name: "github.get_issue".to_string(),
                    description: "Get issue details and comments".to_string(),
                    input_schema: serde_json::json!({
                        "type": "object",
                        "properties": {
                            "owner": { "type": "string" },
                            "repo": { "type": "string" },
                            "number": { "type": "integer" }
                        },
                        "required": ["repo", "number"]
                    }),
                },
                SkillTool {
                    name: "github.create_issue".to_string(),
                    description: "Create a new issue".to_string(),
                    input_schema: serde_json::json!({
                        "type": "object",
                        "properties": {
                            "owner": { "type": "string" },
                            "repo": { "type": "string" },
                            "title": { "type": "string" },
                            "body": { "type": "string" }
                        },
                        "required": ["repo", "title"]
                    }),
                },
                SkillTool {
                    name: "github.list_pulls".to_string(),
                    description: "List pull requests".to_string(),
                    input_schema: serde_json::json!({
                        "type": "object",
                        "properties": {
                            "owner": { "type": "string" },
                            "repo": { "type": "string" },
                            "state": { "type": "string" }
                        },
                        "required": ["repo"]
                    }),
                },
                SkillTool {
                    name: "github.get_pull".to_string(),
                    description: "Get pull request details and changed files".to_string(),
                    input_schema: serde_json::json!({
                        "type": "object",
                        "properties": {
                            "owner": { "type": "string" },
                            "repo": { "type": "string" },
                            "number": { "type": "integer" }
                        },
                        "required": ["repo", "number"]
                    }),
                },
                SkillTool {
                    name: "github.get_file".to_string(),
                    description: "Get file contents from a repository".to_string(),
                    input_schema: serde_json::json!({
                        "type": "object",
                        "properties": {
                            "owner": { "type": "string" },
                            "repo": { "type": "string" },
                            "path": { "type": "string" },
                            "ref": { "type": "string" }
                        },
                        "required": ["repo", "path"]
                    }),
                },
                SkillTool {
                    name: "github.pr_create".to_string(),
                    description: "Create a new pull request".to_string(),
                    input_schema: serde_json::json!({
                        "type": "object",
                        "properties": {
                            "owner": { "type": "string" },
                            "repo": { "type": "string" },
                            "title": { "type": "string" },
                            "head": { "type": "string" },
                            "base": { "type": "string" }
                        },
                        "required": ["repo", "title", "head"]
                    }),
                },
                SkillTool {
                    name: "github.pr_diff".to_string(),
                    description: "Get file diffs for a pull request".to_string(),
                    input_schema: serde_json::json!({
                        "type": "object",
                        "properties": {
                            "owner": { "type": "string" },
                            "repo": { "type": "string" },
                            "number": { "type": "integer" }
                        },
                        "required": ["repo", "number"]
                    }),
                },
                SkillTool {
                    name: "github.pr_review".to_string(),
                    description: "Submit a review on a pull request; APPROVE and REQUEST_CHANGES go out as the GitHub App identity when one is configured, else as a comment".to_string(),
                    input_schema: serde_json::json!({
                        "type": "object",
                        "properties": {
                            "owner": { "type": "string" },
                            "repo": { "type": "string" },
                            "number": { "type": "integer" },
                            "event": { "type": "string" },
                            "body": { "type": "string" },
                            "commit_id": { "type": "string" }
                        },
                        "required": ["repo", "number", "event"]
                    }),
                },
                SkillTool {
                    name: "github.pr_comment".to_string(),
                    description: "Add a comment to a pull request".to_string(),
                    input_schema: serde_json::json!({
                        "type": "object",
                        "properties": {
                            "owner": { "type": "string" },
                            "repo": { "type": "string" },
                            "number": { "type": "integer" },
                            "body": { "type": "string" }
                        },
                        "required": ["repo", "number", "body"]
                    }),
                },
                SkillTool {
                    name: "github.pr_merge".to_string(),
                    description: "Merge a pull request".to_string(),
                    input_schema: serde_json::json!({
                        "type": "object",
                        "properties": {
                            "owner": { "type": "string" },
                            "repo": { "type": "string" },
                            "number": { "type": "integer" },
                            "merge_method": { "type": "string" },
                            "sha": { "type": "string" }
                        },
                        "required": ["repo", "number"]
                    }),
                },
                // DW-115: Issue management
                SkillTool {
                    name: "github.issue_comment".to_string(),
                    description: "Add a comment to an issue or PR".to_string(),
                    input_schema: serde_json::json!({
                        "type": "object",
                        "properties": {
                            "owner": { "type": "string" },
                            "repo": { "type": "string" },
                            "number": { "type": "integer" },
                            "body": { "type": "string" }
                        },
                        "required": ["repo", "number", "body"]
                    }),
                },
                SkillTool {
                    name: "github.issue_update".to_string(),
                    description: "Update an issue (state, title, body, labels, assignees)"
                        .to_string(),
                    input_schema: serde_json::json!({
                        "type": "object",
                        "properties": {
                            "owner": { "type": "string" },
                            "repo": { "type": "string" },
                            "number": { "type": "integer" },
                            "state": { "type": "string" }
                        },
                        "required": ["repo", "number"]
                    }),
                },
                // DW-112: Repo browsing
                SkillTool {
                    name: "github.dir_list".to_string(),
                    description: "List directory contents at a ref".to_string(),
                    input_schema: serde_json::json!({
                        "type": "object",
                        "properties": {
                            "owner": { "type": "string" },
                            "repo": { "type": "string" },
                            "path": { "type": "string" },
                            "ref": { "type": "string" }
                        },
                        "required": ["repo"]
                    }),
                },
                SkillTool {
                    name: "github.compare".to_string(),
                    description: "Compare two branches/tags/commits".to_string(),
                    input_schema: serde_json::json!({
                        "type": "object",
                        "properties": {
                            "owner": { "type": "string" },
                            "repo": { "type": "string" },
                            "base": { "type": "string" },
                            "head": { "type": "string" }
                        },
                        "required": ["repo", "base", "head"]
                    }),
                },
                SkillTool {
                    name: "github.search_code".to_string(),
                    description: "Search code across repositories".to_string(),
                    input_schema: serde_json::json!({
                        "type": "object",
                        "properties": {
                            "query": { "type": "string" },
                            "owner": { "type": "string" },
                            "repo": { "type": "string" }
                        },
                        "required": ["query"]
                    }),
                },
                SkillTool {
                    name: "github.search_commits".to_string(),
                    description: "Search commits by author, org/repo, and date (default branch)"
                        .to_string(),
                    input_schema: serde_json::json!({
                        "type": "object",
                        "properties": {
                            "query": { "type": "string" },
                            "authors": { "type": "array", "items": { "type": "string" } },
                            "owner": { "type": "string" },
                            "repo": { "type": "string" },
                            "since": { "type": "string" },
                            "until": { "type": "string" },
                            "sort": { "type": "string", "enum": ["author-date", "committer-date"] },
                            "order": { "type": "string", "enum": ["asc", "desc"] },
                            "per_page": { "type": "integer" }
                        }
                    }),
                },
                // DW-113: Branch management
                SkillTool {
                    name: "github.branch_list".to_string(),
                    description: "List branches in a repository".to_string(),
                    input_schema: serde_json::json!({
                        "type": "object",
                        "properties": {
                            "owner": { "type": "string" },
                            "repo": { "type": "string" }
                        },
                        "required": ["repo"]
                    }),
                },
                SkillTool {
                    name: "github.branch_create".to_string(),
                    description: "Create a branch from a ref".to_string(),
                    input_schema: serde_json::json!({
                        "type": "object",
                        "properties": {
                            "owner": { "type": "string" },
                            "repo": { "type": "string" },
                            "branch": { "type": "string" },
                            "from": { "type": "string" }
                        },
                        "required": ["repo", "branch", "from"]
                    }),
                },
                SkillTool {
                    name: "github.branch_delete".to_string(),
                    description: "Delete a remote branch".to_string(),
                    input_schema: serde_json::json!({
                        "type": "object",
                        "properties": {
                            "owner": { "type": "string" },
                            "repo": { "type": "string" },
                            "branch": { "type": "string" }
                        },
                        "required": ["repo", "branch"]
                    }),
                },
                SkillTool {
                    name: "github.branch_protection".to_string(),
                    description: "View branch protection rules".to_string(),
                    input_schema: serde_json::json!({
                        "type": "object",
                        "properties": {
                            "owner": { "type": "string" },
                            "repo": { "type": "string" },
                            "branch": { "type": "string" }
                        },
                        "required": ["repo", "branch"]
                    }),
                },
                // DW-114: CI/Actions
                SkillTool {
                    name: "github.ci_status".to_string(),
                    description: "Get CI check status for a ref".to_string(),
                    input_schema: serde_json::json!({
                        "type": "object",
                        "properties": {
                            "owner": { "type": "string" },
                            "repo": { "type": "string" },
                            "ref": { "type": "string" }
                        },
                        "required": ["repo", "ref"]
                    }),
                },
                SkillTool {
                    name: "github.workflow_runs".to_string(),
                    description: "List recent workflow runs".to_string(),
                    input_schema: serde_json::json!({
                        "type": "object",
                        "properties": {
                            "owner": { "type": "string" },
                            "repo": { "type": "string" },
                            "branch": { "type": "string" },
                            "status": { "type": "string" }
                        },
                        "required": ["repo"]
                    }),
                },
                SkillTool {
                    name: "github.workflow_run_logs".to_string(),
                    description: "Get jobs and steps for a workflow run".to_string(),
                    input_schema: serde_json::json!({
                        "type": "object",
                        "properties": {
                            "owner": { "type": "string" },
                            "repo": { "type": "string" },
                            "run_id": { "type": "integer" }
                        },
                        "required": ["repo", "run_id"]
                    }),
                },
                SkillTool {
                    name: "github.workflow_rerun".to_string(),
                    description: "Re-run a workflow run".to_string(),
                    input_schema: serde_json::json!({
                        "type": "object",
                        "properties": {
                            "owner": { "type": "string" },
                            "repo": { "type": "string" },
                            "run_id": { "type": "integer" }
                        },
                        "required": ["repo", "run_id"]
                    }),
                },
                // PR update, failed logs, raw API
                SkillTool {
                    name: "github.pr_update".to_string(),
                    description:
                        "Update a pull request (title, body, labels, assignees, reviewers)"
                            .to_string(),
                    input_schema: serde_json::json!({
                        "type": "object",
                        "properties": {
                            "owner": { "type": "string" },
                            "repo": { "type": "string" },
                            "number": { "type": "integer" },
                            "title": { "type": "string" },
                            "body": { "type": "string" },
                            "base": { "type": "string" },
                            "state": { "type": "string" },
                            "draft": { "type": "boolean" },
                            "labels": { "type": "array", "items": { "type": "string" } },
                            "assignees": { "type": "array", "items": { "type": "string" } },
                            "reviewers": { "type": "array", "items": { "type": "string" } }
                        },
                        "required": ["repo", "number"]
                    }),
                },
                SkillTool {
                    name: "github.workflow_run_failed_logs".to_string(),
                    description: "Get log text for failed steps in a workflow run".to_string(),
                    input_schema: serde_json::json!({
                        "type": "object",
                        "properties": {
                            "owner": { "type": "string" },
                            "repo": { "type": "string" },
                            "run_id": { "type": "integer" },
                            "max_log_lines": { "type": "integer" }
                        },
                        "required": ["repo", "run_id"]
                    }),
                },
                SkillTool {
                    name: "github.api".to_string(),
                    description: "Make an arbitrary GitHub API request".to_string(),
                    input_schema: serde_json::json!({
                        "type": "object",
                        "properties": {
                            "method": { "type": "string", "enum": ["GET", "POST", "PUT", "PATCH", "DELETE"] },
                            "path": { "type": "string" },
                            "body": { "type": "object" },
                            "query": { "type": "object" },
                            "workspace": { "type": "string" }
                        },
                        "required": ["method", "path"]
                    }),
                },
            ],

            event_sources: vec![SkillEventSource {
                r#type: "github_webhook".to_string(),
                events: vec![
                    "pull_request.opened".to_string(),
                    "pull_request.closed".to_string(),
                    "pull_request.merged".to_string(),
                    "pull_request_review.submitted".to_string(),
                    "push".to_string(),
                    "issues.opened".to_string(),
                    "issues.closed".to_string(),
                    "issue_comment.created".to_string(),
                    "check_run.completed".to_string(),
                    "workflow_run.completed".to_string(),
                ],
                config_required: vec!["github_webhook_secret".to_string()],
            }],

            default_policies: vec![
                SkillPolicy {
                    tool: "github.list_repos".to_string(),
                    action: "allow".to_string(),
                },
                SkillPolicy {
                    tool: "github.get_repo".to_string(),
                    action: "allow".to_string(),
                },
                SkillPolicy {
                    tool: "github.list_issues".to_string(),
                    action: "allow".to_string(),
                },
                SkillPolicy {
                    tool: "github.get_issue".to_string(),
                    action: "allow".to_string(),
                },
                SkillPolicy {
                    tool: "github.create_issue".to_string(),
                    action: "allow".to_string(),
                },
                SkillPolicy {
                    tool: "github.list_pulls".to_string(),
                    action: "allow".to_string(),
                },
                SkillPolicy {
                    tool: "github.get_pull".to_string(),
                    action: "allow".to_string(),
                },
                SkillPolicy {
                    tool: "github.get_file".to_string(),
                    action: "allow".to_string(),
                },
                SkillPolicy {
                    tool: "github.pr_create".to_string(),
                    action: "allow".to_string(),
                },
                SkillPolicy {
                    tool: "github.pr_diff".to_string(),
                    action: "allow".to_string(),
                },
                SkillPolicy {
                    tool: "github.pr_review".to_string(),
                    action: "allow".to_string(),
                },
                SkillPolicy {
                    tool: "github.pr_comment".to_string(),
                    action: "allow".to_string(),
                },
                SkillPolicy {
                    tool: "github.pr_merge".to_string(),
                    action: "allow".to_string(),
                },
                SkillPolicy {
                    tool: "github.issue_comment".to_string(),
                    action: "allow".to_string(),
                },
                SkillPolicy {
                    tool: "github.issue_update".to_string(),
                    action: "allow".to_string(),
                },
                SkillPolicy {
                    tool: "github.dir_list".to_string(),
                    action: "allow".to_string(),
                },
                SkillPolicy {
                    tool: "github.compare".to_string(),
                    action: "allow".to_string(),
                },
                SkillPolicy {
                    tool: "github.search_code".to_string(),
                    action: "allow".to_string(),
                },
                SkillPolicy {
                    tool: "github.search_commits".to_string(),
                    action: "allow".to_string(),
                },
                SkillPolicy {
                    tool: "github.branch_list".to_string(),
                    action: "allow".to_string(),
                },
                SkillPolicy {
                    tool: "github.branch_create".to_string(),
                    action: "allow".to_string(),
                },
                SkillPolicy {
                    tool: "github.branch_delete".to_string(),
                    action: "allow".to_string(),
                },
                SkillPolicy {
                    tool: "github.branch_protection".to_string(),
                    action: "allow".to_string(),
                },
                SkillPolicy {
                    tool: "github.ci_status".to_string(),
                    action: "allow".to_string(),
                },
                SkillPolicy {
                    tool: "github.workflow_runs".to_string(),
                    action: "allow".to_string(),
                },
                SkillPolicy {
                    tool: "github.workflow_run_logs".to_string(),
                    action: "allow".to_string(),
                },
                SkillPolicy {
                    tool: "github.workflow_rerun".to_string(),
                    action: "allow".to_string(),
                },
                SkillPolicy {
                    tool: "github.pr_update".to_string(),
                    action: "allow".to_string(),
                },
                SkillPolicy {
                    tool: "github.workflow_run_failed_logs".to_string(),
                    action: "allow".to_string(),
                },
                SkillPolicy {
                    tool: "github.api".to_string(),
                    action: "allow".to_string(),
                },
            ],

            memory_schemas: Vec::new(),

            requirements: SkillRequirements {
                credentials: vec![CredentialRequirement {
                    name: "github_token".to_string(),
                    r#type: "api_key".to_string(),
                    description: "GitHub Personal Access Token (PAT)".to_string(),
                }],
                config: Vec::new(),
            },

            dependencies: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn named(names: &[&str], default_workspace: Option<&str>) -> GithubSkillConfig {
        GithubSkillConfig {
            token: None,
            base_url: "https://api.github.com".to_string(),
            default_owner: None,
            workspaces: names
                .iter()
                .map(|name| {
                    (
                        name.to_string(),
                        config::GithubWorkspaceConfig {
                            token: "t".to_string(),
                            base_url: "https://api.github.com".to_string(),
                            default_owner: Some(format!("{name}-org")),
                        },
                    )
                })
                .collect(),
            default_workspace: default_workspace.map(str::to_string),
        }
    }

    #[test]
    fn the_default_owner_follows_the_effective_default_workspace() {
        let sole = GithubSkill::new(named(&["acme"], None)).unwrap();
        assert_eq!(sole.default_workspace, "acme");
        assert_eq!(sole.default_owner.as_deref(), Some("acme-org"));

        let selected = GithubSkill::new(named(&["acme", "client"], Some("client"))).unwrap();
        assert_eq!(selected.default_workspace, "client");
        assert_eq!(selected.default_owner.as_deref(), Some("client-org"));

        let unselected = GithubSkill::new(named(&["acme", "client"], None)).unwrap();
        assert_eq!(unselected.default_workspace, "default");
        assert!(unselected.default_owner.is_none());
    }

    #[test]
    fn tool_names_matches_registered_tools() {
        let cfg = GithubSkillConfig {
            token: Some("t".to_string()),
            base_url: "https://api.github.com".to_string(),
            default_owner: None,
            workspaces: std::collections::HashMap::new(),
            default_workspace: None,
        };
        let skill = GithubSkill::new(cfg).unwrap();
        let registered: Vec<String> = skill.tools().iter().map(|t| t.name().to_string()).collect();
        let declared: Vec<String> = GithubSkill::tool_names()
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(
            registered, declared,
            "GithubSkill::tool_names() drifted from tools()"
        );
    }

    #[test]
    fn read_and_write_tool_lists_cover_all_tools() {
        crate::adapters::test_support::assert_read_write_partition(
            GithubSkill::tool_names(),
            READ_TOOLS,
            WRITE_TOOLS,
        );
    }
}
