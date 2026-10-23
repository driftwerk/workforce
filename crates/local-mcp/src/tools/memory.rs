//! `memory.*` MCP tools — durable note-taking across Claude Code sessions.
//! Backed by the SQLite FTS5 store from `workforce-memory`; no embeddings in
//! Phase 1, so recall is BM25 keyword-based. The store supports hybrid
//! vector rerank — that slots in via `SqliteMemoryConfig.embedding_provider`
//! when Phase 2 lands.

use rmcp::{handler::server::wrapper::Parameters, schemars, tool, tool_router};
use serde::{Deserialize, Serialize};
use workforce_memory::store::MemoryStore;
use workforce_memory::types::{Memory, MemoryFilter, MemoryId, MemoryLayer, MemorySource};

use crate::tools::common::memory_layer_str;
use crate::LocalMcpServer;

const DEFAULT_LIMIT: usize = 10;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RememberArgs {
    /// The note to record.
    pub content: String,
    /// Tags to attach to the note for later filtering.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Importance hint between 0.0 and 1.0 (default 0.5). Influences ranking
    /// during recall.
    #[serde(default)]
    pub importance: Option<f32>,
    /// Memory layer. Defaults to "semantic" (long-lived, domain knowledge).
    /// Other options: "episodic" (personal experience), "working" (session
    /// scratch), "team" (shared), "organisation".
    #[serde(default)]
    pub layer: Option<LayerArg>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RecallArgs {
    /// Natural-language query.
    pub query: String,
    /// Max matches (default 10).
    #[serde(default)]
    pub limit: Option<usize>,
    /// Restrict to a specific layer.
    #[serde(default)]
    pub layer: Option<LayerArg>,
    /// Require these tags (any match).
    #[serde(default)]
    pub tags: Vec<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ForgetArgs {
    /// The memory ID returned by `memory.remember` / `memory.recall`.
    pub memory_id: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ListArgs {
    /// Max matches (default 10).
    #[serde(default)]
    pub limit: Option<usize>,
    /// Restrict to a specific layer.
    #[serde(default)]
    pub layer: Option<LayerArg>,
    /// Require these tags (any match).
    #[serde(default)]
    pub tags: Vec<String>,
}

/// Thin serde mirror of [`MemoryLayer`] — exposed separately so we can control
/// the schemars-rendered enum docs without leaking `workforce-memory`'s type.
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum LayerArg {
    Working,
    Episodic,
    Semantic,
    Team,
    Organisation,
}

impl From<LayerArg> for MemoryLayer {
    fn from(l: LayerArg) -> Self {
        match l {
            LayerArg::Working => MemoryLayer::Working,
            LayerArg::Episodic => MemoryLayer::Episodic,
            LayerArg::Semantic => MemoryLayer::Semantic,
            LayerArg::Team => MemoryLayer::Team,
            LayerArg::Organisation => MemoryLayer::Organisation,
        }
    }
}

#[derive(Debug, Serialize)]
struct MemorySummary {
    id: String,
    content: String,
    layer: String,
    tags: Vec<String>,
    importance: f32,
    age: String,
    created_at: String,
}

impl From<&Memory> for MemorySummary {
    fn from(m: &Memory) -> Self {
        Self {
            id: m.id.to_string(),
            content: m.content.clone(),
            layer: memory_layer_str(&m.layer).to_string(),
            tags: m.tags.clone(),
            importance: m.importance,
            age: m.age_label(),
            created_at: m.created_at.to_rfc3339(),
        }
    }
}

#[derive(Debug, Serialize)]
struct RecallHit {
    #[serde(flatten)]
    memory: MemorySummary,
    score: f32,
}

#[tool_router(router = memory_router, vis = "pub(crate)")]
impl LocalMcpServer {
    #[tool(
        name = "memory.remember",
        description = "Record a durable note. Use this to persist design decisions, project \
                       facts, or anything you want future Claude Code sessions to recall. \
                       Returns the stored memory's id."
    )]
    async fn memory_remember(
        &self,
        Parameters(args): Parameters<RememberArgs>,
    ) -> Result<String, String> {
        let layer = args.layer.map_or(MemoryLayer::Semantic, MemoryLayer::from);
        let base = Memory::new(args.content, layer, self.state.owner.clone())
            .with_source(MemorySource::Agent)
            .with_tags(args.tags);
        let memory = match args.importance {
            Some(i) => base.with_importance(i),
            None => base,
        };

        let id = self
            .state
            .memory
            .remember(memory)
            .await
            .map_err(|e| format!("remember failed: {e}"))?;
        Ok(serde_json::json!({ "memory_id": id.to_string() }).to_string())
    }

    #[tool(
        name = "memory.recall",
        description = "Search stored memories by a natural-language query. Phase 1 uses BM25 \
                       keyword matching (FTS5); Phase 2 adds hybrid vector rerank. Results \
                       are ranked by relevance."
    )]
    async fn memory_recall(
        &self,
        Parameters(args): Parameters<RecallArgs>,
    ) -> Result<String, String> {
        let filter = self.build_filter(args.layer, args.tags, args.limit);
        let hits = self
            .state
            .memory
            .recall(&args.query, filter)
            .await
            .map_err(|e| format!("recall failed: {e}"))?;

        let results: Vec<RecallHit> = hits
            .iter()
            .map(|h| RecallHit {
                memory: MemorySummary::from(&h.memory),
                score: h.score,
            })
            .collect();
        Ok(serde_json::json!({ "results": results }).to_string())
    }

    #[tool(
        name = "memory.forget",
        description = "Soft-delete a memory by its id. The content is retained in the audit \
                       trail but no longer surfaces in recall or list."
    )]
    async fn memory_forget(
        &self,
        Parameters(args): Parameters<ForgetArgs>,
    ) -> Result<String, String> {
        self.state
            .memory
            .forget(&MemoryId(args.memory_id.clone()))
            .await
            .map_err(|e| format!("forget failed: {e}"))?;
        Ok(serde_json::json!({ "memory_id": args.memory_id, "deleted": true }).to_string())
    }

    #[tool(
        name = "memory.list",
        description = "List stored memories without a search query. Use this to browse all \
                       notes, optionally filtered by layer or tags."
    )]
    async fn memory_list(&self, Parameters(args): Parameters<ListArgs>) -> Result<String, String> {
        let filter = self.build_filter(args.layer, args.tags, args.limit);
        let memories = self
            .state
            .memory
            .list(filter)
            .await
            .map_err(|e| format!("list failed: {e}"))?;

        let results: Vec<MemorySummary> = memories.iter().map(MemorySummary::from).collect();
        Ok(serde_json::json!({ "results": results }).to_string())
    }

    fn build_filter(
        &self,
        layer: Option<LayerArg>,
        tags: Vec<String>,
        limit: Option<usize>,
    ) -> MemoryFilter {
        MemoryFilter {
            layer: layer.map(MemoryLayer::from),
            owner: Some(self.state.owner.clone()),
            tags: tag_filter(tags),
            limit: Some(limit.unwrap_or(DEFAULT_LIMIT)),
            ..MemoryFilter::system_unscoped()
        }
    }
}

fn tag_filter(tags: Vec<String>) -> Option<Vec<String>> {
    (!tags.is_empty()).then_some(tags)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tag_filter_treats_empty_as_none() {
        assert!(tag_filter(vec![]).is_none());
        assert_eq!(tag_filter(vec!["a".into()]), Some(vec!["a".to_string()]));
    }

    #[test]
    fn layer_arg_roundtrips_to_memory_layer() {
        let cases = [
            (LayerArg::Working, MemoryLayer::Working),
            (LayerArg::Episodic, MemoryLayer::Episodic),
            (LayerArg::Semantic, MemoryLayer::Semantic),
            (LayerArg::Team, MemoryLayer::Team),
            (LayerArg::Organisation, MemoryLayer::Organisation),
        ];
        for (arg, expected) in cases {
            assert_eq!(MemoryLayer::from(arg), expected);
        }
    }
}
