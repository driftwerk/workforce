//! `context.fuse` — composite fan-out tool that queries the code graph and
//! project memory concurrently in a single call. Returns a cross-referenced
//! payload so the caller can reason about code + history together without
//! making separate round-trips.
//!
//! Single-repo mode (default): code+graph results live under top-level
//! `code_matches` / `graph_context`. Project mode (`project="harbor"`):
//! code+graph results are bucketed by repo under `code_by_repo`.

use std::sync::Arc;
use std::time::Instant;

use rmcp::{handler::server::wrapper::Parameters, schemars, tool, tool_router};
use serde::{Deserialize, Serialize};
use workforce_codebase_knowledge::query::QueryResult;
use workforce_knowledge_service::KnowledgeService;
use workforce_memory::store::MemoryStore;
use workforce_memory::types::{MemoryFilter, MemorySearchResult};

use crate::tools::common::memory_layer_str;
use crate::tools::project::find_project;
use crate::LocalMcpServer;

const DEFAULT_MAX_RESULTS: usize = 20;
const DEFAULT_SCOPE: &str = "full";

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct FuseArgs {
    /// Natural-language question about the codebase or project context.
    pub query: String,
    /// Repo path (default: workspace root). Mutually exclusive with
    /// `project`; single-repo mode.
    #[serde(default)]
    pub path: Option<String>,
    /// Project name. When set, the code query fans out across every repo in
    /// the project and results are bucketed by repo. Mutually exclusive with
    /// `path`.
    #[serde(default)]
    pub project: Option<String>,
    /// Traversal scope: "direct" (top entities only) or "full" (includes
    /// graph-walk context). Default: "full".
    #[serde(default)]
    pub scope: Option<String>,
    /// Which sources to query: any of "code", "graph", "memory". Empty = all.
    #[serde(default)]
    pub include: Vec<String>,
    /// Max results per source. Default 20.
    #[serde(default)]
    pub max_results: Option<usize>,
}

#[derive(Debug, Serialize)]
struct CodeMatch {
    file: String,
    entity: String,
    kind: String,
    lines: String,
    relevance: f32,
}

#[derive(Debug, Serialize)]
struct GraphContext {
    answer: String,
    suggestion: Option<String>,
}

#[derive(Debug, Serialize)]
struct MemoryMatch {
    content: String,
    layer: String,
    tags: Vec<String>,
    relevance: f32,
    created_at: String,
}

#[derive(Debug, Serialize)]
struct RepoBucket {
    repo_id: String,
    repo_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    code_matches: Option<Vec<CodeMatch>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    graph_context: Option<GraphContext>,
    confidence: f32,
    freshness: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

impl From<&MemorySearchResult> for MemoryMatch {
    fn from(r: &MemorySearchResult) -> Self {
        Self {
            content: r.memory.content.clone(),
            layer: memory_layer_str(&r.memory.layer).to_string(),
            tags: r.memory.tags.clone(),
            relevance: r.score,
            created_at: r.memory.created_at.to_rfc3339(),
        }
    }
}

#[tool_router(router = fuse_router, vis = "pub(crate)")]
impl LocalMcpServer {
    #[tool(
        name = "context.fuse",
        description = "Fan-out query: runs code-graph retrieval and memory recall concurrently \
                       for one natural-language question, returning a cross-referenced payload \
                       (code_matches + graph_context + memory_context + confidence + freshness). \
                       Pass `project` to fan out across every repo in a project (results bucketed \
                       under `code_by_repo`); otherwise operates on a single repo (`path`, defaults \
                       to workspace). `include` restricts sources; scope=\"direct\" skips graph \
                       traversal context."
    )]
    async fn context_fuse(&self, Parameters(args): Parameters<FuseArgs>) -> Result<String, String> {
        if args.path.is_some() && args.project.is_some() {
            return Err("pass either `path` or `project`, not both".into());
        }

        let start = Instant::now();
        let sources = IncludeFilter::from(&args.include);
        let max_results = args.max_results.unwrap_or(DEFAULT_MAX_RESULTS);
        let thorough = args.scope.as_deref().unwrap_or(DEFAULT_SCOPE) == "full";

        let memory_future = sources.memory.then(|| {
            let mem = Arc::clone(&self.state.memory) as Arc<dyn MemoryStore>;
            let owner = self.state.owner.clone();
            let query = args.query.clone();
            let filter = MemoryFilter {
                owner: Some(owner),
                limit: Some(max_results),
                ..MemoryFilter::system_unscoped()
            };
            tokio::spawn(async move { mem.recall(&query, filter).await })
        });

        let mut payload = serde_json::Map::new();
        payload.insert("query".into(), serde_json::json!(args.query));

        match args.project.as_deref() {
            Some(project_name) => {
                self.merge_project_code(
                    &mut payload,
                    project_name,
                    &args.query,
                    &sources,
                    thorough,
                    max_results,
                )
                .await?;
            }
            None => {
                self.merge_single_repo_code(
                    &mut payload,
                    &args.path,
                    &args.query,
                    &sources,
                    thorough,
                    max_results,
                )
                .await?;
            }
        }

        if let Some(handle) = memory_future {
            match handle.await {
                Ok(Ok(hits)) => {
                    let items: Vec<MemoryMatch> = hits
                        .iter()
                        .take(max_results)
                        .map(MemoryMatch::from)
                        .collect();
                    payload.insert("memory_context".into(), serde_json::json!(items));
                }
                Ok(Err(e)) => {
                    payload.insert(
                        "memory_error".into(),
                        serde_json::json!(format!("memory recall failed: {e}")),
                    );
                }
                Err(e) => {
                    payload.insert(
                        "memory_error".into(),
                        serde_json::json!(format!("memory task panicked: {e}")),
                    );
                }
            }
        }

        payload.insert(
            "duration_ms".into(),
            serde_json::json!(start.elapsed().as_millis() as u64),
        );
        Ok(serde_json::Value::Object(payload).to_string())
    }

    async fn merge_single_repo_code(
        &self,
        payload: &mut serde_json::Map<String, serde_json::Value>,
        path: &Option<String>,
        query: &str,
        sources: &IncludeFilter,
        thorough: bool,
        max_results: usize,
    ) -> Result<(), String> {
        let repo_id = self
            .state
            .knowledge
            .resolve_repo_id(&self.state.workspace, path)?;
        payload.insert("repo_id".into(), serde_json::json!(repo_id));

        if !sources.code_or_graph() {
            return Ok(());
        }

        let qe = Arc::clone(self.state.knowledge.query_engine());
        let repo = repo_id.clone();
        let q = query.to_string();
        let handle = tokio::spawn(async move { qe.ask(&repo, &q, None, thorough).await });

        match handle.await {
            Ok(Ok(result)) => merge_code_payload(payload, sources, max_results, &result),
            Ok(Err(e)) => {
                payload.insert(
                    "code_error".into(),
                    serde_json::json!(format!("knowledge query failed: {e}")),
                );
            }
            Err(e) => {
                payload.insert(
                    "code_error".into(),
                    serde_json::json!(format!("knowledge task panicked: {e}")),
                );
            }
        }
        Ok(())
    }

    async fn merge_project_code(
        &self,
        payload: &mut serde_json::Map<String, serde_json::Value>,
        project_name: &str,
        query: &str,
        sources: &IncludeFilter,
        thorough: bool,
        max_results: usize,
    ) -> Result<(), String> {
        let project = find_project(&self.state.projects_dir, project_name)?;
        payload.insert("project".into(), serde_json::json!(project.name));

        if !sources.code_or_graph() || project.repos.is_empty() {
            payload.insert(
                "code_by_repo".into(),
                serde_json::json!(Vec::<RepoBucket>::new()),
            );
            return Ok(());
        }

        let data_dir = self.state.data_dir.clone();
        let tasks: Vec<_> = project
            .repos
            .iter()
            .map(|repo| {
                let qe = Arc::clone(self.state.knowledge.query_engine());
                let repo_path = repo.effective_path(&project, &data_dir);
                let repo_id = KnowledgeService::repo_id_for(&repo_path);
                let repo_name = repo.name.clone();
                let q = query.to_string();
                let id_for_task = repo_id.clone();
                let handle =
                    tokio::spawn(async move { qe.ask(&id_for_task, &q, None, thorough).await });
                (repo_id, repo_name, handle)
            })
            .collect();

        let mut buckets = Vec::with_capacity(tasks.len());
        for (repo_id, repo_name, handle) in tasks {
            buckets.push(bucket_from_task(repo_id, repo_name, handle, sources, max_results).await);
        }
        payload.insert("code_by_repo".into(), serde_json::json!(buckets));
        Ok(())
    }
}

async fn bucket_from_task(
    repo_id: String,
    repo_name: String,
    handle: tokio::task::JoinHandle<eyre::Result<QueryResult>>,
    sources: &IncludeFilter,
    max_results: usize,
) -> RepoBucket {
    match handle.await {
        Ok(Ok(result)) => bucket_from_result(repo_id, repo_name, &result, sources, max_results),
        Ok(Err(e)) => RepoBucket {
            repo_id,
            repo_name,
            code_matches: None,
            graph_context: None,
            confidence: 0.0,
            freshness: String::new(),
            error: Some(format!("knowledge query failed: {e}")),
        },
        Err(e) => RepoBucket {
            repo_id,
            repo_name,
            code_matches: None,
            graph_context: None,
            confidence: 0.0,
            freshness: String::new(),
            error: Some(format!("knowledge task panicked: {e}")),
        },
    }
}

fn bucket_from_result(
    repo_id: String,
    repo_name: String,
    result: &QueryResult,
    sources: &IncludeFilter,
    max_results: usize,
) -> RepoBucket {
    let code_matches = sources.code.then(|| code_matches(result, max_results));
    let graph_context = sources.graph.then(|| GraphContext {
        answer: result.answer.clone(),
        suggestion: result.suggestion.clone(),
    });
    RepoBucket {
        repo_id,
        repo_name,
        code_matches,
        graph_context,
        confidence: result.confidence,
        freshness: result.freshness.clone(),
        error: None,
    }
}

fn code_matches(result: &QueryResult, max_results: usize) -> Vec<CodeMatch> {
    result
        .sources
        .iter()
        .take(max_results)
        .map(|s| CodeMatch {
            file: s.file.clone(),
            entity: s.entity.clone(),
            kind: s.kind.clone(),
            lines: s.lines.clone(),
            relevance: s.relevance,
        })
        .collect()
}

fn merge_code_payload(
    payload: &mut serde_json::Map<String, serde_json::Value>,
    sources: &IncludeFilter,
    max_results: usize,
    result: &QueryResult,
) {
    if sources.code {
        payload.insert(
            "code_matches".into(),
            serde_json::json!(code_matches(result, max_results)),
        );
    }
    if sources.graph {
        payload.insert(
            "graph_context".into(),
            serde_json::json!(GraphContext {
                answer: result.answer.clone(),
                suggestion: result.suggestion.clone(),
            }),
        );
    }
    payload.insert("confidence".into(), serde_json::json!(result.confidence));
    payload.insert("freshness".into(), serde_json::json!(result.freshness));
}

struct IncludeFilter {
    code: bool,
    graph: bool,
    memory: bool,
}

impl IncludeFilter {
    fn from(include: &[String]) -> Self {
        if include.is_empty() {
            return Self {
                code: true,
                graph: true,
                memory: true,
            };
        }
        Self {
            code: include.iter().any(|s| s == "code"),
            graph: include.iter().any(|s| s == "graph"),
            memory: include.iter().any(|s| s == "memory"),
        }
    }

    fn code_or_graph(&self) -> bool {
        self.code || self.graph
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn include_filter_defaults_to_all_when_empty() {
        let f = IncludeFilter::from(&[]);
        assert!(f.code && f.graph && f.memory);
    }

    #[test]
    fn include_filter_respects_explicit_subset() {
        let f = IncludeFilter::from(&["memory".to_string()]);
        assert!(!f.code);
        assert!(!f.graph);
        assert!(f.memory);
    }

    #[test]
    fn include_filter_unknown_values_are_ignored() {
        let f = IncludeFilter::from(&["banana".to_string(), "code".to_string()]);
        assert!(f.code);
        assert!(!f.graph);
        assert!(!f.memory);
    }
}
