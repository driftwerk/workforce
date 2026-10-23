use async_trait::async_trait;
use eyre::Result;
use serde::Deserialize;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use workforce_agent::tools::{Tool, ToolResult};

use super::access_guard::{maybe_guard, KnowledgeAccessGuard};
use super::{resolve_query_repo, KnowledgeService, RepoResolver};

#[derive(Deserialize)]
struct Input {
    #[serde(default = "default_scope")]
    scope: String,
    #[serde(default = "default_detail")]
    detail: String,
    #[serde(default, alias = "path")]
    repo: Option<String>,
}

fn default_scope() -> String {
    "full".to_string()
}

fn default_detail() -> String {
    "summary".to_string()
}

pub struct KnowledgeOverviewTool {
    service: Arc<KnowledgeService>,
    agent_workspace: PathBuf,
    resolver: Option<Arc<RepoResolver>>,
    access_guard: Option<KnowledgeAccessGuard>,
}

impl KnowledgeOverviewTool {
    pub fn new(service: Arc<KnowledgeService>, agent_workspace: PathBuf) -> Self {
        Self {
            service,
            agent_workspace,
            resolver: None,
            access_guard: None,
        }
    }

    pub fn with_resolver(mut self, resolver: Arc<RepoResolver>) -> Self {
        self.resolver = Some(resolver);
        self
    }

    pub fn with_access_guard(mut self, guard: KnowledgeAccessGuard) -> Self {
        self.access_guard = Some(guard);
        self
    }
}

#[async_trait]
impl Tool for KnowledgeOverviewTool {
    fn name(&self) -> &str {
        "knowledge.overview"
    }

    fn description(&self) -> &str {
        "Get codebase architecture overview with stats and modules"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "scope": {
                    "type": "string",
                    "description": "Scope for the overview"
                },
                "detail": {
                    "type": "string",
                    "enum": ["summary", "full"],
                    "description": "Level of detail"
                },
                "repo": {
                    "type": "string",
                    "description": "Repo or project to query, by name. Omit when only one repo is indexed."
                }
            }
        ,
            "required": []})
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();
        let input: Input =
            serde_json::from_value(input).map_err(|e| eyre::eyre!("Invalid input: {}", e))?;

        if let Some(denied) = maybe_guard(self.access_guard.as_ref(), start) {
            return Ok(denied);
        }

        let repo_id = match resolve_query_repo(
            &self.service,
            &self.agent_workspace,
            self.resolver.as_deref(),
            &input.repo,
        ) {
            Ok(id) => id,
            Err(msg) => return Ok(ToolResult::failure(msg, start.elapsed().as_millis() as u64)),
        };
        let scope = if input.scope == "full" {
            None
        } else {
            Some(input.scope.as_str())
        };

        let detail = if input.detail == "full" {
            "full"
        } else {
            "summary"
        };

        match self
            .service
            .query_engine()
            .overview(&repo_id, scope, detail)
            .await
        {
            Ok(result) => Ok(ToolResult::success(
                serde_json::to_value(result)?,
                start.elapsed().as_millis() as u64,
            )),
            Err(e) => Ok(ToolResult::failure(
                format!("Overview failed: {}", e),
                start.elapsed().as_millis() as u64,
            )),
        }
    }
}
