//! Fused context tool — `context.fuse`.
//!
//! Composes code knowledge (graph + vector search) with agent memory into a
//! single query, returning structured cross-referenced results. No LLM
//! synthesis — results are returned as-is for the calling agent to interpret.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use eyre::Result;
use serde::Deserialize;

use workforce_agent::tools::{Tool, ToolResult};
use workforce_core::types::AgentId;
use workforce_memory::store::{apply_agent_scope, MemoryStore};
use workforce_memory::types::MemoryFilter;

use crate::knowledge::{resolve_query_repo, KnowledgeAccessGuard, KnowledgeService, RepoResolver};
use crate::resolved_config::ConfigCell;

#[derive(Deserialize)]
struct FuseInput {
    query: String,
    #[serde(default)]
    repo: Option<String>,
    /// What `repo` was called before it took a name. Agents still write it,
    /// and it means the same thing.
    #[serde(default)]
    path: Option<String>,
    #[serde(default = "default_scope")]
    scope: String,
    #[serde(default)]
    include: Vec<String>,
    #[serde(default = "default_max_results")]
    max_results: usize,
}

fn default_scope() -> String {
    "full".into()
}

fn default_max_results() -> usize {
    20
}

pub struct ContextFuseTool {
    knowledge: Arc<KnowledgeService>,
    memory: Arc<dyn MemoryStore>,
    agent_workspace: PathBuf,
    agent_id: AgentId,
    resolved: ConfigCell,
    /// Canonical name → `repo_id` resolver, as the `knowledge.*` tools hold.
    /// `None` in single-agent setups, which query the service's own root.
    resolver: Option<Arc<RepoResolver>>,
    /// The cascade gate of `knowledge.ask`, which is what the code half of a
    /// fused query runs: a user refused that tool is refused this half too.
    code_guard: Option<KnowledgeAccessGuard>,
}

/// Which repo the code half of a query reads, or why it reads none.
///
/// A repo the caller named and the hub does not know fails the call: the
/// argument is wrong and the message lists the right ones. A repo left out
/// where several are indexed costs only the code half — the memory half does
/// not depend on it, and the message that asks for `repo` rides along.
fn code_target(
    named: bool,
    resolved: Result<String, String>,
) -> Result<Result<String, String>, String> {
    match (resolved, named) {
        (Ok(repo_id), _) => Ok(Ok(repo_id)),
        (Err(unknown), true) => Err(unknown),
        (Err(unspecified), false) => Ok(Err(unspecified)),
    }
}

impl ContextFuseTool {
    pub fn new(
        knowledge: Arc<KnowledgeService>,
        memory: Arc<dyn MemoryStore>,
        agent_workspace: PathBuf,
        agent_id: AgentId,
        resolved: ConfigCell,
    ) -> Self {
        Self {
            knowledge,
            memory,
            agent_workspace,
            agent_id,
            resolved,
            resolver: None,
            code_guard: None,
        }
    }

    pub fn with_resolver(mut self, resolver: Arc<RepoResolver>) -> Self {
        self.resolver = Some(resolver);
        self
    }

    pub fn with_code_guard(mut self, guard: KnowledgeAccessGuard) -> Self {
        self.code_guard = Some(guard);
        self
    }
}

fn input_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "query": {
                "type": "string",
                "description": "Natural language question about the codebase"
            },
            "repo": {
                "type": "string",
                "description": "Indexed repo or project to query, by name (e.g. \"core-api\"), as repo.list gives it. Omit only when a single repo is indexed."
            },
            "scope": {
                "type": "string",
                "enum": ["direct", "full"],
                "description": "direct = top entities only; full = includes graph traversal context. Default: full"
            },
            "include": {
                "type": "array",
                "items": { "type": "string", "enum": ["code", "graph", "memory"] },
                "description": "Which sources to query. Default: all sources"
            },
            "max_results": {
                "type": "integer",
                "description": "Maximum results per source. Default: 20"
            }
        },
        "required": ["query"]
    })
}

#[async_trait]
impl Tool for ContextFuseTool {
    fn name(&self) -> &str {
        "context.fuse"
    }

    fn description(&self) -> &str {
        "Query code knowledge and agent memory in a single call. Returns cross-referenced \
         results from the code graph (entities, call chains, dependencies), semantic vector \
         search (similarity matches), and agent memory (past decisions, incidents, learnings). \
         Use this for design tasks, impact analysis, or when you need full context about a \
         codebase area before making changes."
    }

    fn input_schema(&self) -> serde_json::Value {
        input_schema()
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();
        let elapsed = || start.elapsed().as_millis() as u64;

        let input: FuseInput = match serde_json::from_value(input) {
            Ok(i) => i,
            Err(e) => {
                return Ok(ToolResult::failure(
                    format!("Invalid input: {e}"),
                    elapsed(),
                ))
            }
        };

        let include_all = input.include.is_empty();
        let include_code = include_all || input.include.iter().any(|s| s == "code");
        let include_graph = include_all || input.include.iter().any(|s| s == "graph");
        let include_memory = include_all || input.include.iter().any(|s| s == "memory");

        let thorough = input.scope == "full";

        // Fan out queries concurrently
        let code_handle = if include_code || include_graph {
            let named = input.repo.or(input.path);
            let refused = self
                .code_guard
                .as_ref()
                .and_then(|guard| guard.guard(start))
                .and_then(|denied| denied.error);
            let target = match refused {
                Some(refused) => Err(refused),
                None => match code_target(
                    named.is_some(),
                    resolve_query_repo(
                        &self.knowledge,
                        &self.agent_workspace,
                        self.resolver.as_deref(),
                        &named,
                    ),
                ) {
                    Ok(target) => target,
                    Err(unknown) => return Ok(ToolResult::failure(unknown, elapsed())),
                },
            };
            let qe = Arc::clone(self.knowledge.query_engine());
            let query = input.query.clone();
            Some(tokio::spawn(async move {
                match target {
                    Ok(repo) => qe
                        .ask(&repo, &query, None, thorough)
                        .await
                        .map_err(|e| format!("Knowledge query failed: {e}")),
                    Err(no_repo) => Err(no_repo),
                }
            }))
        } else {
            None
        };

        let memory_handle = if include_memory {
            let mem = Arc::clone(&self.memory);
            let query = input.query.clone();
            let limit = input.max_results;
            let identity_id = crate::user_config::current_identity_id();
            let scope = self.resolved.snapshot().memory.agent_recall_scope;
            let agent_id = self.agent_id.clone();
            Some(tokio::spawn(async move {
                mem.recall(
                    &query,
                    MemoryFilter {
                        owner: scope.owned_only().then(|| agent_id.clone()),
                        identity_id,
                        limit: Some(limit),
                        ..Default::default()
                    },
                )
                .await
                .map(|results| apply_agent_scope(scope, &agent_id, results))
            }))
        } else {
            None
        };

        // Collect results
        let mut result = serde_json::Map::new();

        if let Some(handle) = code_handle {
            match handle.await {
                Ok(Ok(query_result)) => {
                    if include_code {
                        let sources: Vec<serde_json::Value> = query_result
                            .sources
                            .iter()
                            .take(input.max_results)
                            .map(|s| {
                                serde_json::json!({
                                    "file": s.file,
                                    "entity": s.entity,
                                    "kind": s.kind,
                                    "lines": s.lines,
                                    "relevance": s.relevance,
                                })
                            })
                            .collect();
                        result.insert("code_matches".into(), serde_json::json!(sources));
                    }
                    if include_graph {
                        result.insert(
                            "graph_context".into(),
                            serde_json::json!({
                                "answer": query_result.answer,
                                "suggestion": query_result.suggestion,
                                "vectors_skipped": query_result.vectors_skipped,
                            }),
                        );
                    }
                    result.insert(
                        "confidence".into(),
                        serde_json::json!(query_result.confidence),
                    );
                    result.insert(
                        "freshness".into(),
                        serde_json::json!(query_result.freshness),
                    );
                }
                Ok(Err(e)) => {
                    result.insert("code_error".into(), serde_json::json!(e));
                }
                Err(e) => {
                    result.insert(
                        "code_error".into(),
                        serde_json::json!(format!("Knowledge query panicked: {e}")),
                    );
                }
            }
        }

        if let Some(handle) = memory_handle {
            match handle.await {
                Ok(Ok(memories)) => {
                    let memory_results: Vec<serde_json::Value> = memories
                        .into_iter()
                        .take(input.max_results)
                        .map(|m| {
                            let mut entry = serde_json::json!({
                                "content": m.memory.content,
                                "layer": format!("{:?}", m.memory.layer),
                                "tags": m.memory.tags,
                                "relevance": m.score,
                                "created_at": m.memory.created_at.to_rfc3339(),
                            });
                            // Shared scope can surface another agent's
                            // Team/Global memories — name the origin so the
                            // caller doesn't absorb them as its own.
                            if m.memory.owner != self.agent_id {
                                entry["origin_agent"] = serde_json::json!(m.memory.owner.as_str());
                            }
                            entry
                        })
                        .collect();
                    result.insert("memory_context".into(), serde_json::json!(memory_results));
                }
                Ok(Err(e)) => {
                    result.insert(
                        "memory_error".into(),
                        serde_json::json!(format!("Memory recall failed: {e}")),
                    );
                }
                Err(e) => {
                    result.insert(
                        "memory_error".into(),
                        serde_json::json!(format!("Memory recall panicked: {e}")),
                    );
                }
            }
        }

        result.insert("query".into(), serde_json::json!(input.query));
        result.insert("duration_ms".into(), serde_json::json!(elapsed()));

        Ok(ToolResult::success(
            serde_json::Value::Object(result),
            elapsed(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fuse_input_defaults() {
        let input: FuseInput = serde_json::from_value(serde_json::json!({
            "query": "test query"
        }))
        .unwrap();
        assert_eq!(input.scope, "full");
        assert_eq!(input.max_results, 20);
        assert!(input.include.is_empty());
        assert!(input.repo.is_none() && input.path.is_none());
    }

    #[test]
    fn the_repo_is_a_name_whichever_parameter_carries_it() {
        let named = |field: &str| {
            let input: FuseInput =
                serde_json::from_value(serde_json::json!({ "query": "q", field: "core-api" }))
                    .unwrap();
            input.repo.or(input.path)
        };
        assert_eq!(named("repo").as_deref(), Some("core-api"));
        assert_eq!(named("path").as_deref(), Some("core-api"));
    }

    #[test]
    fn the_schema_asks_for_a_repo_name_and_offers_no_path() {
        let schema = input_schema();
        let repo = schema["properties"]["repo"]["description"]
            .as_str()
            .unwrap();
        assert!(
            repo.contains("by name") && repo.contains("repo.list"),
            "{repo}"
        );
        assert!(schema["properties"].get("path").is_none());
    }

    /// `context.fuse` over a hub that has `acme/core-api` and `acme/web`
    /// registered, bound to the catalog as `rebind_knowledge_tools` binds it.
    fn fuse_over_two_indexed_repos() -> (tempfile::TempDir, ContextFuseTool) {
        use workforce_common::settings::WorkforceConfig;
        use workforce_projects::sqlite::SqliteProjectStore;
        use workforce_projects::{Project, ProjectRepo, ProjectStore, RepoSource};

        let tmp = tempfile::TempDir::new().unwrap();
        let store: Arc<dyn ProjectStore> = Arc::new(SqliteProjectStore::in_memory().unwrap());
        let mut project = Project::new("allan", "platform");
        project.repos = ["core-api", "web"]
            .map(|repo| ProjectRepo {
                alias: repo.into(),
                source: RepoSource::Github {
                    owner: "acme".into(),
                    repo: repo.into(),
                },
                default_branch: None,
                setup: None,
            })
            .into();
        store.put(&project).unwrap();
        let workspace = tmp.path().join("ws");
        let tool = ContextFuseTool::new(
            Arc::new(KnowledgeService::in_memory(workspace.clone()).unwrap()),
            Arc::new(workforce_memory::in_memory::InMemoryStore::new()),
            workspace.clone(),
            AgentId::new("allan"),
            ConfigCell::new(crate::resolved_config::ResolvedConfig::resolve(
                None,
                None,
                &WorkforceConfig::default(),
                "anthropic",
                None,
            )),
        )
        .with_resolver(Arc::new(RepoResolver::new(
            store,
            tmp.path().to_path_buf(),
            workspace,
        )));
        (tmp, tool)
    }

    async fn fused(tool: &ContextFuseTool, input: serde_json::Value) -> ToolResult {
        tool.execute(input).await.unwrap()
    }

    /// The measured failure: a repo name where the tool took a workspace path
    /// was answered "Path 'core-api' does not exist". A name now goes to the
    /// catalog, under either parameter, and the code half runs.
    #[tokio::test]
    async fn a_repo_name_reaches_the_catalog_under_either_parameter() {
        let (_tmp, tool) = fuse_over_two_indexed_repos();
        for parameter in ["repo", "path"] {
            let result = fused(
                &tool,
                serde_json::json!({ "query": "auth", parameter: "core-api", "include": ["code"] }),
            )
            .await;
            assert!(result.success, "{parameter}: {:?}", result.error);
            assert!(
                result.output.get("code_error").is_none(),
                "{:?}",
                result.output
            );
        }
    }

    #[tokio::test]
    async fn a_repo_the_catalog_lacks_is_refused_with_the_repos_it_has() {
        let (_tmp, tool) = fuse_over_two_indexed_repos();
        let result = fused(
            &tool,
            serde_json::json!({ "query": "auth", "repo": "nope" }),
        )
        .await;
        let error = result.error.unwrap();
        assert!(error.contains("unknown repo or project 'nope'"), "{error}");
        assert!(
            error.contains("acme/core-api") && error.contains("acme/web"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn no_repo_among_several_still_returns_memory_and_asks_for_the_repo() {
        let (_tmp, tool) = fuse_over_two_indexed_repos();
        let result = fused(&tool, serde_json::json!({ "query": "auth" })).await;
        assert!(result.success, "{:?}", result.error);
        let asked = result.output["code_error"].as_str().unwrap();
        assert!(asked.contains("pass `repo` naming one of"), "{asked}");
        assert!(
            asked.contains("acme/core-api") && asked.contains("acme/web"),
            "{asked}"
        );
        assert!(
            result.output["memory_context"].is_array(),
            "{:?}",
            result.output
        );
    }

    #[test]
    fn a_repo_the_hub_does_not_know_fails_the_call_with_the_names() {
        let unknown = "unknown repo or project 'monolit'; indexed: core-api, web".to_string();
        assert_eq!(code_target(true, Err(unknown.clone())), Err(unknown));
    }

    #[test]
    fn a_repo_left_out_among_several_costs_only_the_code_half() {
        let unspecified = "multiple repos are indexed — pass `repo` naming one of: core-api, web";
        assert_eq!(
            code_target(false, Err(unspecified.to_string())),
            Ok(Err(unspecified.to_string()))
        );
        assert_eq!(
            code_target(false, Ok("core-api-1a2b3c4d".to_string())),
            Ok(Ok("core-api-1a2b3c4d".to_string()))
        );
    }

    #[test]
    fn fuse_input_with_all_fields() {
        let input: FuseInput = serde_json::from_value(serde_json::json!({
            "query": "auth middleware",
            "path": "services/auth",
            "scope": "direct",
            "include": ["code", "memory"],
            "max_results": 5
        }))
        .unwrap();
        assert_eq!(input.scope, "direct");
        assert_eq!(input.max_results, 5);
        assert_eq!(input.include.len(), 2);
        assert_eq!(input.path.as_deref(), Some("services/auth"));
    }
}
