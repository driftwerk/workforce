//! `codegraph.*` MCP tools — find symbols, walk the call graph, and trigger
//! indexing against the local SQLite-backed `CodeGraph`. Each tool maps 1:1
//! onto a `CodeGraph` / `KnowledgeService` API; the MCP layer is pure
//! serialisation glue.

use std::path::{Path, PathBuf};

use rmcp::{handler::server::wrapper::Parameters, schemars, tool, tool_router};
use serde::Deserialize;
use workforce_codebase_knowledge::graph::entities::{CodeEntity, EntityId};
use workforce_codebase_knowledge::graph::queries::GraphQueryResult;
use workforce_knowledge_service::KnowledgeService;

use crate::tools::common::{EntitySummary, GraphHit};
use crate::LocalMcpServer;

const DEFAULT_LIMIT: usize = 20;
const DEFAULT_MAX_DEPTH: usize = 3;

#[derive(Clone, Copy)]
enum WalkDirection {
    Callers,
    Callees,
}

impl WalkDirection {
    fn op_name(self) -> &'static str {
        match self {
            Self::Callers => "callers_of",
            Self::Callees => "callees_of",
        }
    }
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct IndexRepoArgs {
    /// Repo path (absolute, or relative to the server's workspace). Omit to
    /// index the server's default workspace.
    #[serde(default)]
    pub path: Option<String>,
    /// If true, run an incremental index (only changed files). Default: full.
    #[serde(default)]
    pub incremental: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct FindSymbolArgs {
    /// Exact symbol name to find (function, class, struct, etc.).
    pub name: String,
    /// Repo path (default: workspace root).
    #[serde(default)]
    pub path: Option<String>,
    /// Maximum number of matches to return (default 20).
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct GraphWalkArgs {
    /// Entity ID as returned by `codegraph.find_symbol`.
    pub entity_id: String,
    /// Maximum BFS depth (default 3).
    #[serde(default)]
    pub max_depth: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct FileGraphArgs {
    /// File path, relative to the repo root.
    pub file_path: String,
    /// Repo path (default: workspace root).
    #[serde(default)]
    pub path: Option<String>,
}

#[tool_router(router = codegraph_router, vis = "pub(crate)")]
impl LocalMcpServer {
    #[tool(
        name = "codegraph.index_repo",
        description = "Index a SINGLE source repository into the local code graph. \
                       Use this only for one specific repo; if the user refers to a \
                       multi-repo project by name (e.g. \"index the harbor project\"), \
                       call `project.index` instead so all the project's repos are \
                       indexed together. Returns file / entity / relationship counts. \
                       Defaults to a full index; pass incremental=true for a delta pass."
    )]
    async fn codegraph_index_repo(
        &self,
        Parameters(args): Parameters<IndexRepoArgs>,
    ) -> Result<String, String> {
        let root = resolve_path(&self.state.workspace, args.path.as_deref())?;
        let result = if args.incremental {
            self.state
                .knowledge
                .trigger_incremental_index_for(&root)
                .await
        } else {
            self.state.knowledge.trigger_full_index_for(&root).await
        }
        .map_err(|e| format!("indexing failed: {e}"))?;

        Ok(serde_json::json!({
            "repo_id": KnowledgeService::repo_id_for(&root),
            "repo_name": KnowledgeService::repo_name_for(&root),
            "files_indexed": result.files_indexed,
            "entities_created": result.entities_created,
            "relationships_created": result.relationships_created,
            "duration_ms": result.duration_ms,
        })
        .to_string())
    }

    #[tool(
        name = "codegraph.find_symbol",
        description = "Find entities (functions, classes, structs, etc.) by exact name. \
                       Returns a list of matches each with an entity_id that can be passed to \
                       codegraph.callers / codegraph.callees."
    )]
    async fn codegraph_find_symbol(
        &self,
        Parameters(args): Parameters<FindSymbolArgs>,
    ) -> Result<String, String> {
        let repo_id = self
            .state
            .knowledge
            .resolve_repo_id(&self.state.workspace, &args.path)?;
        let limit = args.limit.unwrap_or(DEFAULT_LIMIT);
        let entities = self
            .state
            .knowledge
            .graph()
            .find_entities_by_name(&repo_id, &args.name, limit)
            .await
            .map_err(|e| format!("find_entities_by_name failed: {e}"))?;

        Ok(matches_response(&repo_id, &entities))
    }

    #[tool(
        name = "codegraph.callers",
        description = "List entities that call the given entity (reverse call graph). \
                       Input is the opaque `entity_id` returned by codegraph.find_symbol."
    )]
    async fn codegraph_callers(
        &self,
        Parameters(args): Parameters<GraphWalkArgs>,
    ) -> Result<String, String> {
        self.walk(args, WalkDirection::Callers).await
    }

    #[tool(
        name = "codegraph.callees",
        description = "List entities the given entity calls (forward call graph). \
                       Input is the opaque `entity_id` returned by codegraph.find_symbol."
    )]
    async fn codegraph_callees(
        &self,
        Parameters(args): Parameters<GraphWalkArgs>,
    ) -> Result<String, String> {
        self.walk(args, WalkDirection::Callees).await
    }

    async fn walk(&self, args: GraphWalkArgs, direction: WalkDirection) -> Result<String, String> {
        let entity = EntityId(args.entity_id.clone());
        let depth = args.max_depth.unwrap_or(DEFAULT_MAX_DEPTH);
        let graph = self.state.knowledge.graph();
        let hits = match direction {
            WalkDirection::Callers => graph.callers_of(&entity, depth).await,
            WalkDirection::Callees => graph.callees_of(&entity, depth).await,
        }
        .map_err(|e| format!("{} failed: {e}", direction.op_name()))?;
        Ok(walk_response(&args.entity_id, &hits))
    }

    #[tool(
        name = "codegraph.file_graph",
        description = "List all entities defined in a single source file. Useful for getting \
                       a quick structural overview of a file before diving into call graphs."
    )]
    async fn codegraph_file_graph(
        &self,
        Parameters(args): Parameters<FileGraphArgs>,
    ) -> Result<String, String> {
        let repo_id = self
            .state
            .knowledge
            .resolve_repo_id(&self.state.workspace, &args.path)?;
        let entities = self
            .state
            .knowledge
            .graph()
            .find_entities_in_file(&repo_id, &args.file_path)
            .await
            .map_err(|e| format!("find_entities_in_file failed: {e}"))?;
        Ok(serde_json::json!({
            "repo_id": repo_id,
            "file_path": args.file_path,
            "entities": entities.iter().map(EntitySummary::from).collect::<Vec<_>>(),
        })
        .to_string())
    }
}

fn resolve_path(workspace: &Path, rel: Option<&str>) -> Result<PathBuf, String> {
    let target = match rel {
        Some(p) => {
            let candidate = PathBuf::from(p);
            if candidate.is_absolute() {
                candidate
            } else {
                workspace.join(p)
            }
        }
        None => workspace.to_path_buf(),
    };
    if !target.is_dir() {
        return Err(format!("path is not a directory: {}", target.display()));
    }
    std::fs::canonicalize(&target).map_err(|e| format!("canonicalise {}: {e}", target.display()))
}

fn matches_response(repo_id: &str, entities: &[CodeEntity]) -> String {
    serde_json::json!({
        "repo_id": repo_id,
        "matches": entities.iter().map(EntitySummary::from).collect::<Vec<_>>(),
    })
    .to_string()
}

fn walk_response(entity_id: &str, hits: &[GraphQueryResult]) -> String {
    serde_json::json!({
        "entity_id": entity_id,
        "results": hits.iter().map(GraphHit::from).collect::<Vec<_>>(),
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_path_defaults_to_workspace() {
        let tmp = tempfile::tempdir().unwrap();
        let resolved = resolve_path(tmp.path(), None).unwrap();
        assert_eq!(resolved, std::fs::canonicalize(tmp.path()).unwrap());
    }

    #[test]
    fn resolve_path_joins_relative_against_workspace() {
        let tmp = tempfile::tempdir().unwrap();
        let sub = tmp.path().join("child");
        std::fs::create_dir(&sub).unwrap();

        let resolved = resolve_path(tmp.path(), Some("child")).unwrap();
        assert_eq!(resolved, std::fs::canonicalize(&sub).unwrap());
    }

    #[test]
    fn resolve_path_accepts_absolute_input() {
        let tmp = tempfile::tempdir().unwrap();
        let abs = tmp.path().to_path_buf();
        // Workspace is a throwaway dir; the absolute path should win.
        let other = tempfile::tempdir().unwrap();
        let resolved = resolve_path(other.path(), Some(abs.to_str().unwrap())).unwrap();
        assert_eq!(resolved, std::fs::canonicalize(&abs).unwrap());
    }

    #[test]
    fn resolve_path_rejects_missing_target() {
        let tmp = tempfile::tempdir().unwrap();
        let err = resolve_path(tmp.path(), Some("does/not/exist")).unwrap_err();
        assert!(err.contains("not a directory"));
    }
}
