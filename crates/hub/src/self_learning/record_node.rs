//! `deep_expertise.record_node` — the knowledge-TREE counterpart to
//! [`super::record::SelfLearningRecordTool`]. Where that tool persists one flat,
//! atomic claim, this upserts one synthesized sub-domain *node* into the agent's
//! deep-expertise tree (the [`DeepExpertiseStore`]).
//!
//! The same two invariants hold:
//!
//! * **every node cites a primary source.** An empty `sources` list, or any
//!   `reference` that is blank, is rejected before any write.
//! * **callable only inside a curation turn.** The tool is built solely in the
//!   curation tool registry and carries its own [`CurationCtx`] (injected as a
//!   field, never read from the task-local).
//!
//! A node is identified by its position in the tree
//! ([`node_path_hash`] over agent/plan/parent/title), so re-curating the same
//! sub-domain UPDATES the living node in place rather than appending a duplicate.
//! Memory is written first, then the node row points at it (crash-safe order);
//! re-records leave the prior body's Memory unreferenced for the startup orphan
//! sweep to reclaim.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use chrono::{Duration, Utc};
use eyre::Result;
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use tracing::warn;
use workforce_agent::tools::{Tool, ToolResult};
use workforce_common::settings::SelfLearningConfig;
use workforce_memory::store::MemoryStore;
use workforce_memory::types::MemoryId;
use workforce_policy::rules::{PolicyRule, PolicyScope, RuleAction};
use workforce_self_learning::{
    node_path_hash, DeepExpertiseStore, DeepNode, ItemSource, NodeStatus, SourceKind, SurfaceItem,
};

use super::record::{current_user_id, memory_for_item, source_provenance, CurationCtx};

/// Constitutional `Allow` rule for the deep-expertise record tool, mirroring
/// [`super::record::default_policies`]: the policy engine default-denies
/// unmatched tools, so without this the only write tool in a deep-expertise
/// curation snapshot would be denied at call time. The tool only ever exists in
/// a curation snapshot (built with a [`CurationCtx`]), so an ordinary agent
/// never has it.
pub fn default_policies() -> Vec<PolicyRule> {
    vec![PolicyRule {
        name: "deep_expertise.record_node-allow-default".to_string(),
        scope: PolicyScope::Global,
        tool: "deep_expertise.record_node".to_string(),
        action: RuleAction::Allow,
        conditions: Vec::new(),
        priority: 10,
        constitutional: true,
    }]
}

#[derive(Deserialize)]
struct RecordNodeInput {
    title: String,
    body: String,
    #[serde(default)]
    parent_id: Option<String>,
    sources: Vec<NodeSource>,
    #[serde(default)]
    coverage: Vec<NodeCoverage>,
    #[serde(default)]
    confidence: Option<f32>,
}

#[derive(Deserialize)]
struct NodeSource {
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    skill: Option<String>,
    reference: String,
    #[serde(default)]
    excerpt: Option<String>,
}

#[derive(Deserialize)]
struct NodeCoverage {
    reference: String,
    #[serde(default)]
    covered: bool,
}

/// `deep_expertise.record_node` — upserts one synthesized sub-domain node into
/// the agent's knowledge tree.
pub struct DeepNodeRecordTool {
    store: Arc<dyn DeepExpertiseStore>,
    memory: Arc<dyn MemoryStore>,
    config: SelfLearningConfig,
    ctx: CurationCtx,
}

impl DeepNodeRecordTool {
    pub fn new(
        store: Arc<dyn DeepExpertiseStore>,
        memory: Arc<dyn MemoryStore>,
        config: SelfLearningConfig,
        ctx: CurationCtx,
    ) -> Self {
        Self {
            store,
            memory,
            config,
            ctx,
        }
    }
}

fn map_source(s: &NodeSource) -> ItemSource {
    ItemSource {
        kind: s
            .kind
            .as_deref()
            .map(SourceKind::parse)
            .unwrap_or(SourceKind::Skill),
        skill: s.skill.clone(),
        reference: s.reference.trim().to_string(),
        excerpt: s.excerpt.clone(),
    }
}

#[async_trait]
impl Tool for DeepNodeRecordTool {
    fn name(&self) -> &str {
        "deep_expertise.record_node"
    }

    fn description(&self) -> &str {
        "Upsert one synthesized sub-domain node into your deep-expertise knowledge tree during \
         this curation turn. `body` is a multi-paragraph markdown synthesis grounded in `sources` \
         — every node REQUIRES at least one source whose `reference` (page id, url, query, or \
         entity) resolves; a node with no source is rejected. Pass `parent_id` to attach the node \
         beneath an existing node, or omit it for a root sub-domain. Re-recording the same \
         sub-domain (same parent + title) updates the living node in place rather than appending."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "title": { "type": "string", "description": "The sub-domain name (identifies the node within its parent)." },
                "body": { "type": "string", "description": "Multi-paragraph markdown synthesis grounded in the cited sources." },
                "parent_id": { "type": "string", "description": "Optional id of the parent node; omit for a root sub-domain." },
                "sources": {
                    "type": "array",
                    "description": "Primary sources the body is grounded in. At least one required.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "kind": { "type": "string", "enum": ["skill", "codebase", "knowledge", "experience"], "description": "Source category (default skill). Use `experience` for a lesson cited to a `session_id:turn_index` transcript turn." },
                            "skill": { "type": "string", "description": "Originating skill/surface, e.g. 'notion'." },
                            "reference": { "type": "string", "description": "Primary source reference: page id, url, query, or entity. REQUIRED." },
                            "excerpt": { "type": "string", "description": "Short verbatim support snippet." }
                        },
                        "required": ["reference"]
                    },
                    "minItems": 1
                },
                "coverage": {
                    "type": "array",
                    "description": "Enumerated surface elements this node must cover and whether the body covers each.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "reference": { "type": "string", "description": "Symbol, file, or edge the node should cover." },
                            "covered": { "type": "boolean", "description": "True if the body covers this element." }
                        },
                        "required": ["reference"]
                    }
                },
                "confidence": { "type": "number", "minimum": 0, "maximum": 1, "description": "Your self-assessed confidence, 0..1." }
            },
            "required": ["title", "body", "sources"]
        })
    }

    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = Instant::now();
        let elapsed = || start.elapsed().as_millis() as u64;

        let ctx = self.ctx.clone();

        let params: RecordNodeInput = match serde_json::from_value(input) {
            Ok(p) => p,
            Err(e) => {
                return Ok(ToolResult::failure(
                    format!("Invalid input: {e}"),
                    elapsed(),
                ))
            }
        };

        let title = params.title.trim();
        if title.is_empty() {
            return Ok(ToolResult::failure("`title` must not be empty.", elapsed()));
        }
        if params.body.trim().is_empty() {
            return Ok(ToolResult::failure("`body` must not be empty.", elapsed()));
        }

        // Invariant #1: every node cites a resolvable primary source.
        if params.sources.is_empty() {
            return Ok(ToolResult::failure(
                "primary source required: `sources` must not be empty.",
                elapsed(),
            ));
        }
        if params.sources.iter().any(|s| s.reference.trim().is_empty()) {
            return Ok(ToolResult::failure(
                "primary source required: every `sources[].reference` must not be empty.",
                elapsed(),
            ));
        }

        // Resolve the parent assertion. Distinguish "not requested" (a root) from
        // "malformed id" and "well-formed but absent" — a child needs a real
        // parent, mirroring the supersedes resolution in the item tool.
        let parent = match params.parent_id.as_deref() {
            None => None,
            Some(raw) => {
                let Ok(id) = Uuid::parse_str(raw.trim()) else {
                    return Ok(ToolResult::failure(
                        format!("`parent_id` is not a valid id: {raw}"),
                        elapsed(),
                    ));
                };
                match self.store.get_node(id) {
                    Ok(Some(node)) => Some(node),
                    Ok(None) => {
                        return Ok(ToolResult::failure(
                            format!("parent node {id} not found; a child needs a real parent."),
                            elapsed(),
                        ));
                    }
                    Err(e) => {
                        return Ok(ToolResult::failure(format!("store error: {e}"), elapsed()))
                    }
                }
            }
        };
        let parent_id = parent.as_ref().map(|p| p.id);
        let depth = parent.as_ref().map(|p| p.depth + 1).unwrap_or(0);

        let path_hash = node_path_hash(&ctx.agent_id, &ctx.plan_id, parent_id, title);
        let existing = match self.store.find_by_path(&ctx.agent_id, &path_hash) {
            Ok(found) => found,
            Err(e) => return Ok(ToolResult::failure(format!("store error: {e}"), elapsed())),
        };

        // Budget guard (design §6.2): only a brand-new node consumes the per-run
        // budget; an UPDATE of an existing node is free, the precise analog of the
        // item tool's exact-match path returning before the guard.
        let is_new = existing.is_none();
        if is_new && ctx.recorded.load(Ordering::Relaxed) >= ctx.max_items {
            return Ok(ToolResult::failure(
                format!(
                    "per-run node budget reached ({} nodes); stop recording for this run.",
                    ctx.max_items
                ),
                elapsed(),
            ));
        }

        let (id, created_at, created_by) = match &existing {
            Some(node) => (node.id, node.created_at, node.created_by.clone()),
            None => (Uuid::new_v4(), Utc::now(), current_user_id()),
        };

        let sources: Vec<ItemSource> = params.sources.iter().map(map_source).collect();
        let coverage: Vec<SurfaceItem> = params
            .coverage
            .iter()
            .map(|c| SurfaceItem {
                reference: c.reference.trim().to_string(),
                covered: c.covered,
            })
            .collect();

        // Crash-safe order: write the backing Memory first, then the node row
        // pointing at it, then forget the node's PREVIOUS body — so at every
        // step the node references a live Memory. A crash between the last
        // two steps leaks one orphan; the per-run orphan sweep collects it.
        // Retag from `self_learned` to `deep_expertise` so the node body recalls
        // into the `## Deep Expertise` block alone and never pollutes the atomic
        // `## Self-Learned Knowledge` top-k (whose join would drop it anyway).
        let mut mem = memory_for_item(
            &ctx.agent_id,
            &ctx.plan_id,
            title,
            params.body.trim(),
            source_provenance(&sources[0]),
            self.config.provisional_importance,
        );
        for tag in &mut mem.tags {
            if tag == super::SELF_LEARNED_TAG {
                *tag = super::DEEP_EXPERTISE_TAG.to_string();
            }
        }
        let memory_id = match self.memory.remember(mem).await {
            Ok(id) => id,
            Err(e) => return Ok(ToolResult::failure(format!("write failed: {e}"), elapsed())),
        };

        let now = Utc::now();
        let node = DeepNode {
            id,
            agent_id: ctx.agent_id.clone(),
            plan_id: ctx.plan_id.clone(),
            parent_id,
            depth,
            title: title.to_string(),
            path_hash,
            body: params.body.trim().to_string(),
            sources,
            coverage,
            memory_id: Some(memory_id.0),
            status: NodeStatus::Modelled,
            confidence: params.confidence.unwrap_or_default().clamp(0.0, 1.0),
            created_at,
            created_by,
            verified_at: now,
            expires_at: Some(now + Duration::days(ctx.freshness_days as i64)),
            reviewed_at: None,
            reviewed_by: None,
            review_note: None,
            superseded_by: None,
        };
        if let Err(e) = self.store.upsert_node(&node) {
            return Ok(ToolResult::failure(format!("write failed: {e}"), elapsed()));
        }

        // Release the body this re-record replaced (see write-order comment
        // above) — without this every hourly re-verification leaked a stale
        // `deep_expertise` memory into recall.
        if let Some(prev) = existing.as_ref().and_then(|n| n.memory_id.as_deref()) {
            if Some(prev) != node.memory_id.as_deref() {
                if let Err(e) = self.memory.forget(&MemoryId(prev.to_string())).await {
                    warn!(
                        memory_id = prev,
                        error = %e,
                        "failed to forget replaced deep-expertise body; orphan sweep will retry"
                    );
                }
            }
        }

        if is_new {
            ctx.recorded.fetch_add(1, Ordering::Relaxed);
        }

        Ok(ToolResult::success(
            json!({
                "id": node.id,
                "action": if is_new { "created" } else { "updated" },
                "depth": depth,
            }),
            elapsed(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> Arc<dyn DeepExpertiseStore> {
        Arc::new(workforce_self_learning::sqlite::SqliteSelfLearningStore::in_memory().unwrap())
    }

    fn memory() -> Arc<dyn MemoryStore> {
        Arc::new(workforce_memory::in_memory::InMemoryStore::new())
    }

    fn root_call(title: &str) -> Value {
        json!({
            "title": title,
            "body": "A multi-paragraph synthesis.\n\nWith grounding.",
            "sources": [{ "skill": "notion", "reference": "page-1" }]
        })
    }

    #[tokio::test]
    async fn records_new_root_node() {
        let ctx = CurationCtx::new("agent".into(), "plan".into(), 50, 30);
        let tool = DeepNodeRecordTool::new(
            store(),
            memory(),
            SelfLearningConfig::default(),
            ctx.clone(),
        );
        let res = tool.execute(root_call("Discord surface")).await.unwrap();
        assert!(res.success);
        assert_eq!(res.output["action"], "created");
        assert_eq!(res.output["depth"], 0);
        assert_eq!(ctx.recorded.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn new_node_is_modelled() {
        let store = store();
        let ctx = CurationCtx::new("agent".into(), "plan".into(), 50, 30);
        let tool = DeepNodeRecordTool::new(
            Arc::clone(&store),
            memory(),
            SelfLearningConfig::default(),
            ctx,
        );
        let res = tool.execute(root_call("Discord surface")).await.unwrap();
        let id = Uuid::parse_str(res.output["id"].as_str().unwrap()).unwrap();
        let node = store.get_node(id).unwrap().unwrap();
        assert_eq!(node.status, NodeStatus::Modelled);
        assert_eq!(node.depth, 0);
        assert!(node.parent_id.is_none());
        assert!(node.memory_id.is_some());
    }

    #[tokio::test]
    async fn re_record_updates_in_place_without_budget() {
        let store = store();
        let ctx = CurationCtx::new("agent".into(), "plan".into(), 50, 30);
        let tool = DeepNodeRecordTool::new(
            Arc::clone(&store),
            memory(),
            SelfLearningConfig::default(),
            ctx.clone(),
        );
        let first = tool.execute(root_call("Discord surface")).await.unwrap();
        assert_eq!(first.output["action"], "created");
        let first_id = first.output["id"].as_str().unwrap().to_string();
        assert_eq!(ctx.recorded.load(Ordering::Relaxed), 1);

        let again = tool
            .execute(json!({
                "title": "Discord surface",
                "body": "A revised, deeper synthesis.\n\nNow with more detail.",
                "sources": [{ "reference": "page-1" }]
            }))
            .await
            .unwrap();
        assert!(again.success);
        assert_eq!(again.output["action"], "updated");
        // Same living node, identified by tree position.
        assert_eq!(again.output["id"].as_str().unwrap(), first_id);
        // An update must not consume the per-run budget.
        assert_eq!(ctx.recorded.load(Ordering::Relaxed), 1);

        // Only one node exists, and its body reflects the re-record.
        let node = store
            .get_node(Uuid::parse_str(&first_id).unwrap())
            .unwrap()
            .unwrap();
        assert!(node.body.contains("revised"));
    }

    /// Re-recording swaps the node's backing Memory; the replaced body must
    /// be forgotten, not left as a live `deep_expertise` orphan (the leak
    /// that put 337 stale bodies in driftwerk-prod's recall).
    #[tokio::test]
    async fn re_record_forgets_the_replaced_backing_memory() {
        use workforce_memory::types::MemoryId;

        let store = store();
        let mem = memory();
        let ctx = CurationCtx::new("agent".into(), "plan".into(), 50, 30);
        let tool = DeepNodeRecordTool::new(
            Arc::clone(&store),
            Arc::clone(&mem),
            SelfLearningConfig::default(),
            ctx,
        );

        let first = tool.execute(root_call("Discord surface")).await.unwrap();
        let node_id = Uuid::parse_str(first.output["id"].as_str().unwrap()).unwrap();
        let first_memory = store.get_node(node_id).unwrap().unwrap().memory_id.unwrap();

        let again = tool
            .execute(json!({
                "title": "Discord surface",
                "body": "A revised, deeper synthesis.\n\nNow with more detail.",
                "sources": [{ "reference": "page-1" }]
            }))
            .await
            .unwrap();
        assert!(again.success);

        let second_memory = store.get_node(node_id).unwrap().unwrap().memory_id.unwrap();
        assert_ne!(first_memory, second_memory);

        // The new body is live; the replaced one is gone from recall.
        assert!(mem
            .get(&MemoryId(second_memory))
            .await
            .unwrap()
            .is_some_and(|m| !m.deleted));
        assert!(mem
            .get(&MemoryId(first_memory))
            .await
            .unwrap()
            .is_none_or(|m| m.deleted));
    }

    #[tokio::test]
    async fn rejects_blank_title() {
        let ctx = CurationCtx::new("agent".into(), "plan".into(), 50, 30);
        let tool = DeepNodeRecordTool::new(store(), memory(), SelfLearningConfig::default(), ctx);
        let res = tool
            .execute(json!({
                "title": "   ",
                "body": "synthesis",
                "sources": [{ "reference": "page-1" }]
            }))
            .await
            .unwrap();
        assert!(!res.success);
        assert!(res.error.unwrap().contains("`title` must not be empty"));
    }

    #[tokio::test]
    async fn rejects_blank_body() {
        let ctx = CurationCtx::new("agent".into(), "plan".into(), 50, 30);
        let tool = DeepNodeRecordTool::new(store(), memory(), SelfLearningConfig::default(), ctx);
        let res = tool
            .execute(json!({
                "title": "Discord surface",
                "body": "  \n  ",
                "sources": [{ "reference": "page-1" }]
            }))
            .await
            .unwrap();
        assert!(!res.success);
        assert!(res.error.unwrap().contains("`body` must not be empty"));
    }

    #[tokio::test]
    async fn rejects_empty_sources() {
        let ctx = CurationCtx::new("agent".into(), "plan".into(), 50, 30);
        let tool = DeepNodeRecordTool::new(store(), memory(), SelfLearningConfig::default(), ctx);
        let res = tool
            .execute(json!({
                "title": "Discord surface",
                "body": "synthesis",
                "sources": []
            }))
            .await
            .unwrap();
        assert!(!res.success);
        assert!(res.error.unwrap().contains("primary source required"));
    }

    #[tokio::test]
    async fn rejects_blank_reference() {
        let ctx = CurationCtx::new("agent".into(), "plan".into(), 50, 30);
        let tool = DeepNodeRecordTool::new(store(), memory(), SelfLearningConfig::default(), ctx);
        let res = tool
            .execute(json!({
                "title": "Discord surface",
                "body": "synthesis",
                "sources": [{ "reference": "   " }]
            }))
            .await
            .unwrap();
        assert!(!res.success);
        assert!(res.error.unwrap().contains("primary source required"));
    }

    #[tokio::test]
    async fn child_node_gets_parent_depth_plus_one() {
        let store = store();
        let ctx = CurationCtx::new("agent".into(), "plan".into(), 50, 30);
        let tool = DeepNodeRecordTool::new(
            Arc::clone(&store),
            memory(),
            SelfLearningConfig::default(),
            ctx,
        );
        let root = tool.execute(root_call("Discord surface")).await.unwrap();
        let root_id = root.output["id"].as_str().unwrap();

        let child = tool
            .execute(json!({
                "title": "Slash commands",
                "body": "A child synthesis.\n\nDeeper still.",
                "parent_id": root_id,
                "sources": [{ "reference": "page-2" }]
            }))
            .await
            .unwrap();
        assert!(child.success);
        assert_eq!(child.output["action"], "created");
        assert_eq!(child.output["depth"], 1);

        let child_id = Uuid::parse_str(child.output["id"].as_str().unwrap()).unwrap();
        let node = store.get_node(child_id).unwrap().unwrap();
        assert_eq!(node.depth, 1);
        assert_eq!(node.parent_id, Some(Uuid::parse_str(root_id).unwrap()));
    }

    #[tokio::test]
    async fn rejects_unknown_parent() {
        let ctx = CurationCtx::new("agent".into(), "plan".into(), 50, 30);
        let tool = DeepNodeRecordTool::new(store(), memory(), SelfLearningConfig::default(), ctx);
        let res = tool
            .execute(json!({
                "title": "Slash commands",
                "body": "synthesis",
                "parent_id": Uuid::new_v4().to_string(),
                "sources": [{ "reference": "page-2" }]
            }))
            .await
            .unwrap();
        assert!(!res.success);
        assert!(res.error.unwrap().contains("not found"));
    }

    #[tokio::test]
    async fn rejects_malformed_parent_id() {
        let ctx = CurationCtx::new("agent".into(), "plan".into(), 50, 30);
        let tool = DeepNodeRecordTool::new(store(), memory(), SelfLearningConfig::default(), ctx);
        let res = tool
            .execute(json!({
                "title": "Slash commands",
                "body": "synthesis",
                "parent_id": "not-a-uuid",
                "sources": [{ "reference": "page-2" }]
            }))
            .await
            .unwrap();
        assert!(!res.success);
        assert!(res.error.unwrap().contains("not a valid id"));
    }

    #[tokio::test]
    async fn enforces_per_run_budget_for_new_nodes() {
        let ctx = CurationCtx::new("agent".into(), "plan".into(), 1, 30);
        let tool = DeepNodeRecordTool::new(store(), memory(), SelfLearningConfig::default(), ctx);
        let first = tool.execute(root_call("Discord surface")).await.unwrap();
        assert!(first.success);
        let second = tool.execute(root_call("Slack surface")).await.unwrap();
        assert!(!second.success);
        assert!(second.error.unwrap().contains("budget"));
    }
}
