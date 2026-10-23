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
    entity: String,
    #[serde(default)]
    aspect: Option<String>,
    #[serde(default, alias = "path")]
    repo: Option<String>,
}

pub struct KnowledgeExplainTool {
    service: Arc<KnowledgeService>,
    agent_workspace: PathBuf,
    resolver: Option<Arc<RepoResolver>>,
    access_guard: Option<KnowledgeAccessGuard>,
}

impl KnowledgeExplainTool {
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
impl Tool for KnowledgeExplainTool {
    fn name(&self) -> &str {
        "knowledge.explain"
    }

    fn description(&self) -> &str {
        "Get detailed explanation of a code entity"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "entity": {
                    "type": "string",
                    "description": "Entity name or ID to explain"
                },
                "aspect": {
                    "type": "string",
                    "enum": ["full", "flow", "dependencies", "usage", "testing"],
                    "description": "Focus the explanation on a specific aspect"
                },
                "repo": {
                    "type": "string",
                    "description": "Repo or project to query, by name. Omit when only one repo is indexed."
                }
            },
            "required": ["entity"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();
        let input: Input =
            serde_json::from_value(input).map_err(|e| eyre::eyre!("Invalid input: {}", e))?;

        if let Some(denied) = maybe_guard(self.access_guard.as_ref(), start) {
            return Ok(denied);
        }

        // Try direct entity ID first
        let entity_id = EntityId(input.entity.clone());
        let result = self
            .service
            .query_engine()
            .explain(&entity_id, input.aspect.as_deref())
            .await;

        match result {
            Ok(explanation) => Ok(ToolResult::success(
                serde_json::to_value(explanation)?,
                start.elapsed().as_millis() as u64,
            )),
            Err(_) => {
                // Entity ID didn't match — try searching by name
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
                    .find_entities_by_name(&repo_id, &input.entity, 1)
                    .await?;

                if let Some(entity) = entities.first() {
                    match self
                        .service
                        .query_engine()
                        .explain(&entity.id, input.aspect.as_deref())
                        .await
                    {
                        Ok(explanation) => Ok(ToolResult::success(
                            serde_json::to_value(explanation)?,
                            start.elapsed().as_millis() as u64,
                        )),
                        Err(e) => Ok(ToolResult::failure(
                            format!("Explanation failed: {}", e),
                            start.elapsed().as_millis() as u64,
                        )),
                    }
                } else {
                    Ok(ToolResult::failure(
                        format!(
                            "Entity '{}' not found. Use knowledge.ask to search for it first, \
                             or run knowledge.index if the codebase hasn't been indexed yet.",
                            input.entity
                        ),
                        start.elapsed().as_millis() as u64,
                    ))
                }
            }
        }
    }
}
