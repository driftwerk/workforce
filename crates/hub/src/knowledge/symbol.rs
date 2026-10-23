use async_trait::async_trait;
use eyre::Result;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use workforce_agent::tools::{Tool, ToolResult};
use workforce_codebase_knowledge::graph::entities::CodeEntity;

use super::access_guard::{maybe_guard, KnowledgeAccessGuard};
use super::{resolve_query_repo, KnowledgeService, RepoResolver};

const DEFAULT_LIMIT: usize = 20;

#[derive(Deserialize)]
struct Input {
    name: String,
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default, alias = "path")]
    repo: Option<String>,
}

/// Clean, signal-only view of a matched entity. Mirrors the shape emitted by
/// the Claude Code `codegraph.find_symbol` tool so both surfaces agree, without
/// the hub taking a dependency on the local-mcp crate's `EntitySummary`.
#[derive(Serialize)]
struct SymbolMatch {
    entity_id: String,
    name: String,
    qualified_name: String,
    kind: String,
    file: String,
    start_line: usize,
    end_line: usize,
    language: String,
    signature: Option<String>,
}

impl From<&CodeEntity> for SymbolMatch {
    fn from(e: &CodeEntity) -> Self {
        Self {
            entity_id: e.id.to_string(),
            name: e.name.clone(),
            qualified_name: e.qualified_name.clone(),
            kind: e.kind.as_str().to_string(),
            file: e.file_path.clone(),
            start_line: e.start_line,
            end_line: e.end_line,
            language: e.language.as_str().to_string(),
            signature: e.signature.clone(),
        }
    }
}

/// `knowledge.symbol` — exact symbol-name lookup against the code graph.
///
/// This is the agent-facing counterpart to `codegraph.find_symbol`: it answers
/// "where is X defined / used" by exact name, returning kind-tagged matches
/// with file + line. It deliberately does *not* go through the semantic vector
/// path (`knowledge.ask`), which ranks by similarity and is the wrong primitive
/// for an exact-symbol lookup.
pub struct KnowledgeSymbolTool {
    service: Arc<KnowledgeService>,
    agent_workspace: PathBuf,
    resolver: Option<Arc<RepoResolver>>,
    access_guard: Option<KnowledgeAccessGuard>,
}

impl KnowledgeSymbolTool {
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
impl Tool for KnowledgeSymbolTool {
    fn name(&self) -> &str {
        "knowledge.symbol"
    }

    fn description(&self) -> &str {
        "Find code symbols (functions, classes, structs, modules, …) by exact name. \
         Returns kind-tagged matches with file and line — use this for \"where is X \
         defined/used\" lookups. For conceptual questions (\"how does X work\") use \
         knowledge.ask instead; to walk callers/callees from a match, pass its \
         entity_id to knowledge.graph."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Exact symbol name to find (function, class, struct, module, etc.)."
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum number of matches to return. Default: 20."
                },
                "repo": {
                    "type": "string",
                    "description": "Repo or project to query, by name. Omit when only one repo is indexed."
                }
            },
            "required": ["name"]
        })
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

        let limit = input.limit.unwrap_or(DEFAULT_LIMIT);
        let entities = self
            .service
            .graph()
            .find_entities_by_name(&repo_id, &input.name, limit)
            .await?;

        if entities.is_empty() {
            return Ok(ToolResult::failure(
                format!(
                    "No symbol named '{}' found in {}. Check the spelling, try knowledge.ask \
                     for a concept search, or run knowledge.index if the codebase hasn't been \
                     indexed yet.",
                    input.name, repo_id
                ),
                start.elapsed().as_millis() as u64,
            ));
        }

        let matches: Vec<SymbolMatch> = entities.iter().map(SymbolMatch::from).collect();
        Ok(ToolResult::success(
            serde_json::json!({
                "repo_id": repo_id,
                "query": input.name,
                "count": matches.len(),
                "matches": matches,
            }),
            start.elapsed().as_millis() as u64,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use workforce_codebase_knowledge::graph::entities::{
        CodeVisibility, EntityId, EntityKind, Language,
    };

    fn entity(id: &str, name: &str, kind: EntityKind) -> CodeEntity {
        CodeEntity {
            id: EntityId(id.into()),
            repo_id: "repo-1".into(),
            kind,
            name: name.into(),
            qualified_name: format!("knowledge::symbol::{name}"),
            file_path: "crates/hub/src/knowledge/symbol.rs".into(),
            start_line: 60,
            end_line: 64,
            signature: Some(format!("pub struct {name}")),
            visibility: CodeVisibility::Public,
            parent_id: None,
            doc_comment: Some("secret internal note".into()),
            content_hash: "deadbeef".into(),
            language: Language::Rust,
        }
    }

    /// The emitted shape is signal-only: kind/file/line are tagged, and noisy
    /// internals (content_hash, visibility, parent_id, doc_comment) never leak.
    #[test]
    fn symbol_match_is_kind_tagged_and_clean() {
        let e = entity(
            "rust:struct:KnowledgeSymbolTool",
            "KnowledgeSymbolTool",
            EntityKind::Struct,
        );
        let v = serde_json::to_value(SymbolMatch::from(&e)).unwrap();

        assert_eq!(v["entity_id"], "rust:struct:KnowledgeSymbolTool");
        assert_eq!(v["kind"], "struct");
        assert_eq!(v["file"], "crates/hub/src/knowledge/symbol.rs");
        assert_eq!(v["start_line"], 60);
        assert_eq!(v["end_line"], 64);
        assert_eq!(v["language"], "rust");
        assert_eq!(v["signature"], "pub struct KnowledgeSymbolTool");

        let obj = v.as_object().unwrap();
        for leaked in [
            "content_hash",
            "visibility",
            "parent_id",
            "doc_comment",
            "repo_id",
        ] {
            assert!(!obj.contains_key(leaked), "{leaked} should not be emitted");
        }
    }

    /// End-to-end through the tool: seed the in-memory graph, resolve the repo
    /// from the workspace (resolver-less path), and exact-match by name.
    #[tokio::test]
    async fn execute_returns_exact_match_only() {
        let workspace = std::path::PathBuf::from("/virtual/workforce");
        let ks = Arc::new(KnowledgeService::in_memory(workspace.clone()).unwrap());
        let repo_id = ks.repo_id();
        ks.graph()
            .register_repo(&repo_id, "workforce", "/virtual/workforce")
            .await
            .unwrap();

        // Seed the target plus a decoy with a different name.
        for mut e in [
            entity("id:target", "KnowledgeSymbolTool", EntityKind::Struct),
            entity("id:decoy", "SomethingElse", EntityKind::Function),
        ] {
            e.repo_id = repo_id.clone();
            ks.graph().upsert_entity(&e).await.unwrap();
        }

        let tool = KnowledgeSymbolTool::new(Arc::clone(&ks), workspace);

        let hit = tool
            .execute(serde_json::json!({ "name": "KnowledgeSymbolTool" }))
            .await
            .unwrap();
        assert!(hit.success, "expected success, got {:?}", hit.error);
        assert_eq!(hit.output["repo_id"], repo_id);
        assert_eq!(hit.output["count"], 1);
        let matches = hit.output["matches"].as_array().unwrap();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0]["name"], "KnowledgeSymbolTool");
        assert_eq!(matches[0]["kind"], "struct");

        // A name that isn't indexed fails cleanly rather than returning noise.
        let miss = tool
            .execute(serde_json::json!({ "name": "NoSuchSymbol" }))
            .await
            .unwrap();
        assert!(!miss.success);
        assert!(miss
            .error
            .unwrap()
            .contains("No symbol named 'NoSuchSymbol'"));
    }
}
