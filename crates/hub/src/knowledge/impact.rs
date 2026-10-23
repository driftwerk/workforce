use async_trait::async_trait;
use eyre::Result;
use serde::Deserialize;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use workforce_agent::tools::{Tool, ToolResult};
use workforce_codebase_knowledge::graph::entities::EntityId;

use super::access_guard::{maybe_guard, KnowledgeAccessGuard};
use super::{resolve_query_repo, KnowledgeService, RepoResolver};

#[derive(Deserialize)]
struct Input {
    target: String,
    change_description: String,
    #[serde(default = "default_scope")]
    scope: String,
    #[serde(default, alias = "path")]
    repo: Option<String>,
}

fn default_scope() -> String {
    "full".to_string()
}

pub struct KnowledgeImpactTool {
    service: Arc<KnowledgeService>,
    agent_workspace: PathBuf,
    resolver: Option<Arc<RepoResolver>>,
    access_guard: Option<KnowledgeAccessGuard>,
}

impl KnowledgeImpactTool {
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
impl Tool for KnowledgeImpactTool {
    fn name(&self) -> &str {
        "knowledge.impact"
    }

    fn description(&self) -> &str {
        "Analyse impact of a proposed code change"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "target": {
                    "type": "string",
                    "description": "Entity name or ID to analyse"
                },
                "change_description": {
                    "type": "string",
                    "description": "Description of the proposed change"
                },
                "scope": {
                    "type": "string",
                    "enum": ["direct", "full"],
                    "description": "'direct' analyses only immediate dependencies (1 hop)"
                },
                "repo": {
                    "type": "string",
                    "description": "Repo or project to query, by name. Omit when only one repo is indexed."
                }
            },
            "required": ["target", "change_description"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();
        let input: Input =
            serde_json::from_value(input).map_err(|e| eyre::eyre!("Invalid input: {}", e))?;

        if let Some(denied) = maybe_guard(self.access_guard.as_ref(), start) {
            return Ok(denied);
        }

        let max_depth = if input.scope == "direct" { 1 } else { 3 };

        // Try direct entity ID first
        let entity_id = EntityId(input.target.clone());
        let result = self
            .service
            .query_engine()
            .impact(&entity_id, &input.change_description, max_depth)
            .await;

        match result {
            Ok(impact) => Ok(ToolResult::success(
                serde_json::to_value(impact)?,
                start.elapsed().as_millis() as u64,
            )),
            Err(_) => {
                // Try name lookup
                let repo_id = match resolve_query_repo(
                    &self.service,
                    &self.agent_workspace,
                    self.resolver.as_deref(),
                    &input.repo,
                ) {
                    Ok(id) => id,
                    Err(msg) => {
                        return Ok(ToolResult::failure(msg, start.elapsed().as_millis() as u64))
                    }
                };
                let entities = self
                    .service
                    .graph()
                    .find_entities_by_name(&repo_id, &input.target, 1)
                    .await?;

                if let Some(entity) = entities.first() {
                    match self
                        .service
                        .query_engine()
                        .impact(&entity.id, &input.change_description, max_depth)
                        .await
                    {
                        Ok(impact) => Ok(ToolResult::success(
                            serde_json::to_value(impact)?,
                            start.elapsed().as_millis() as u64,
                        )),
                        Err(e) => Ok(ToolResult::failure(
                            format!("Impact analysis failed: {}", e),
                            start.elapsed().as_millis() as u64,
                        )),
                    }
                } else {
                    Ok(ToolResult::failure(
                        format!(
                            "Entity '{}' not found. Use knowledge.ask to find it first.",
                            input.target
                        ),
                        start.elapsed().as_millis() as u64,
                    ))
                }
            }
        }
    }
}
