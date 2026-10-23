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
    query: String,
    entity: String,
    #[serde(default = "default_depth")]
    depth: usize,
    #[serde(default, alias = "path")]
    repo: Option<String>,
}

fn default_depth() -> usize {
    2
}

pub struct KnowledgeGraphTool {
    service: Arc<KnowledgeService>,
    agent_workspace: PathBuf,
    resolver: Option<Arc<RepoResolver>>,
    access_guard: Option<KnowledgeAccessGuard>,
}

impl KnowledgeGraphTool {
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
impl Tool for KnowledgeGraphTool {
    fn name(&self) -> &str {
        "knowledge.graph"
    }

    fn description(&self) -> &str {
        "Query code knowledge graph for entity relationships"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "enum": ["callers_of", "callees_of", "imports_of", "implementations_of", "tests_for", "depends_on", "affected_by"],
                    "description": "The type of graph query to run"
                },
                "entity": {
                    "type": "string",
                    "description": "Entity name or ID to query about"
                },
                "depth": {
                    "type": "integer",
                    "description": "Maximum traversal depth for recursive queries. Default: 2"
                },
                "repo": {
                    "type": "string",
                    "description": "Repo or project to query, by name. Omit when only one repo is indexed."
                }
            },
            "required": ["query", "entity"]
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
            .graph_query(&input.query, &entity_id, input.depth)
            .await;

        match result {
            Ok(results) if !results.is_empty() => Ok(ToolResult::success(
                serde_json::to_value(&results)?,
                start.elapsed().as_millis() as u64,
            )),
            _ => {
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
                    .find_entities_by_name(&repo_id, &input.entity, 1)
                    .await?;

                if let Some(entity) = entities.first() {
                    match self
                        .service
                        .query_engine()
                        .graph_query(&input.query, &entity.id, input.depth)
                        .await
                    {
                        Ok(results) => Ok(ToolResult::success(
                            serde_json::json!({
                                "entity_id": entity.id.as_str(),
                                "entity_name": entity.qualified_name,
                                "query": input.query,
                                "results": results,
                                "count": results.len(),
                            }),
                            start.elapsed().as_millis() as u64,
                        )),
                        Err(e) => Ok(ToolResult::failure(
                            format!("Graph query failed: {}", e),
                            start.elapsed().as_millis() as u64,
                        )),
                    }
                } else {
                    Ok(ToolResult::failure(
                        format!(
                            "Entity '{}' not found. Use knowledge.ask to search for it, \
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
