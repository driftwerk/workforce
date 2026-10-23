//! `self_learning.record` — the one write tool present in a curation turn's
//! read-only registry. It writes only to the knowledge store (a Memory row +
//! a `SelfLearningStore` item), never to a source.
//!
//! Two invariants hold:
//!
//! * **#1 — every item cites a primary source.** A blank `reference` is
//!   rejected before any write (the model cannot record a sourceless claim).
//! * **callable only inside a curation turn.** The tool is built solely in the
//!   curation tool registry and carries its own [`CurationCtx`]; an ordinary
//!   turn never has it. (It cannot read the [`SELF_LEARNING_CURATION`]
//!   task-local — the agent runner executes tools on a task where that hub-side
//!   local is out of scope.)
//!
//! Reconciliation (update-vs-append) is a pure decision in [`reconcile`],
//! unit-tested in isolation; `execute` resolves the IO (lookups, recall) and
//! then dispatches on that decision.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use chrono::{Duration, Utc};
use eyre::Result;
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use workforce_agent::runner::CURRENT_USER_ID;
use workforce_agent::tools::{Tool, ToolResult};
use workforce_common::settings::SelfLearningConfig;
use workforce_core::types::AgentId;
use workforce_memory::store::MemoryStore;
use workforce_memory::types::{
    Memory, MemoryFilter, MemoryId, MemoryLayer, MemorySource, Visibility,
};
use workforce_policy::rules::{PolicyRule, PolicyScope, RuleAction};
use workforce_self_learning::{
    content_hash, ItemSource, ItemStatus, KnowledgeItem, SelfLearningStore, SourceKind,
};

use super::{PLAN_TAG_PREFIX, SELF_LEARNED_TAG, SOURCE_TAG_PREFIX, TOPIC_TAG_PREFIX};
pub use crate::hub_core::curation::CurationCtx;

/// Cosine similarity at which a new provisional claim counts as a *restatement*
/// of an existing one (and supersedes it). Deliberately stricter than the memory
/// store's generic `DEFAULT_NEAR_DUPLICATE_SIMILARITY` (0.92): "billing is
/// monthly" and "billing is weekly" sit near 0.92, so a looser bar would let a
/// contradiction masquerade as a restatement and silently retire the older
/// claim. At 0.97 only true rewordings collapse; genuine differences stay as
/// separate provisional items for a human to compare.
const CURATION_RESTATEMENT_SIMILARITY: f32 = 0.97;

tokio::task_local! {
    /// Scoped by the runner / CC backend around a curation turn so context
    /// assembly can suppress self-learned recall (curation must not read its own
    /// provisional output back as fact). The record tool no longer reads this —
    /// it carries its own [`CurationCtx`] — because the agent runner runs tools
    /// on a task where this local is out of scope.
    pub static SELF_LEARNING_CURATION: CurationCtx;
}

/// Outcome of the pure reconciliation decision (design §6.2 steps 2-5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordAction {
    /// Exact content-hash match already exists — bump `verified_at`, no new row.
    Unchanged,
    /// A near-duplicate **provisional** restatement exists — supersede it
    /// (old → `Superseded`, memory forgotten) and insert the new provisional.
    Supersede,
    /// The agent asserted `supersedes` against an approved target — record the
    /// new claim provisional with a conflict note, leaving the target approved.
    ConflictProvisional,
    /// No prior item — insert fresh provisional.
    Insert,
}

/// Pure reconciliation. Precedence follows the design's numbered steps:
/// an asserted supersedes-target wins, then an exact hash, then a near
/// provisional restatement, else a fresh insert.
///
/// `supersedes_target` is `Some` ONLY when the agent asserted `supersedes` AND
/// the target resolves to an existing item for this agent — the
/// "asserted-but-missing" error is handled in `execute` before this is called.
pub fn reconcile(
    existing_exact: Option<&KnowledgeItem>,
    near_provisional: Option<&KnowledgeItem>,
    supersedes_target: Option<&KnowledgeItem>,
) -> RecordAction {
    if supersedes_target.is_some() {
        RecordAction::ConflictProvisional
    } else if existing_exact.is_some() {
        RecordAction::Unchanged
    } else if near_provisional.is_some() {
        RecordAction::Supersede
    } else {
        RecordAction::Insert
    }
}

/// Constitutional `Allow` rule for the record tool. The policy engine
/// default-denies unmatched tools, so without this the only write tool in the
/// curation registry would be denied at call time and curation would record
/// nothing. The tool only ever exists in a curation snapshot (built with a
/// [`CurationCtx`]), so an ordinary agent never has it.
pub fn default_policies() -> Vec<PolicyRule> {
    vec![PolicyRule {
        name: "self_learning.record-allow-default".to_string(),
        scope: PolicyScope::Global,
        tool: "self_learning.record".to_string(),
        action: RuleAction::Allow,
        conditions: Vec::new(),
        priority: 10,
        constitutional: true,
    }]
}

#[derive(Deserialize)]
struct RecordInput {
    topic: String,
    claim: String,
    #[serde(default)]
    confidence: f32,
    #[serde(default)]
    high_stakes: bool,
    #[serde(default)]
    supersedes: Option<String>,
    source: RecordSource,
}

#[derive(Deserialize)]
struct RecordSource {
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    skill: Option<String>,
    reference: String,
    #[serde(default)]
    excerpt: Option<String>,
}

/// `self_learning.record` — persists one atomic, primary-cited knowledge item.
pub struct SelfLearningRecordTool {
    store: Arc<dyn SelfLearningStore>,
    memory: Arc<dyn MemoryStore>,
    config: SelfLearningConfig,
    ctx: CurationCtx,
}

impl SelfLearningRecordTool {
    pub fn new(
        store: Arc<dyn SelfLearningStore>,
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

    /// Build the backing Memory for a fresh/superseding item (design §6.2 step
    /// 5): provisional importance so it recalls like training but ranks below it.
    fn build_memory(&self, ctx: &CurationCtx, input: &RecordInput, provenance: &str) -> Memory {
        memory_for_item(
            &ctx.agent_id,
            &ctx.plan_id,
            &input.topic,
            &input.claim,
            provenance,
            self.config.provisional_importance,
        )
    }

    /// Look for a near-duplicate provisional restatement on this agent + topic
    /// (design §6.2 step 4). Similarity is used for dedup ONLY; an approved
    /// item is never auto-touched. Returns the item to supersede, if any.
    async fn find_near_provisional(
        &self,
        ctx: &CurationCtx,
        input: &RecordInput,
    ) -> Option<KnowledgeItem> {
        let filter = MemoryFilter {
            owner: Some(AgentId(ctx.agent_id.clone())),
            tags: Some(vec![
                SELF_LEARNED_TAG.to_string(),
                format!("{TOPIC_TAG_PREFIX}{}", input.topic),
            ]),
            system_unscoped: true,
            ..Default::default()
        };
        let hit = self
            .memory
            .find_near_duplicate(&input.claim, filter, CURATION_RESTATEMENT_SIMILARITY)
            .await
            .ok()
            .flatten()?;
        let item = self
            .store
            .items_by_memory_ids(std::slice::from_ref(&hit.memory.id.0))
            .ok()?
            .into_iter()
            .next()?;
        (item.status == ItemStatus::Provisional).then_some(item)
    }

    /// Crash-safe insert (design §8 ordering): write the Memory first, then the
    /// item row pointing at it. A crash between them leaves an orphan Memory the
    /// startup sweep reclaims — never an item row pointing at nothing. A freshly
    /// recorded item is never itself superseded, so `superseded_by` is always
    /// `None` here; the forward link onto a retired item is set by the caller.
    async fn insert_item(
        &self,
        ctx: &CurationCtx,
        input: &RecordInput,
        source: ItemSource,
        hash: String,
        review_note: Option<String>,
    ) -> Result<Uuid> {
        let now = Utc::now();
        let mem = self.build_memory(ctx, input, source_provenance(&source));
        let memory_id = self.memory.remember(mem).await?;
        let item = KnowledgeItem {
            id: Uuid::new_v4(),
            agent_id: ctx.agent_id.clone(),
            plan_id: ctx.plan_id.clone(),
            topic: input.topic.clone(),
            claim: input.claim.clone(),
            content_hash: hash,
            memory_id: Some(memory_id.0),
            status: ItemStatus::Provisional,
            confidence: input.confidence.clamp(0.0, 1.0),
            high_stakes: input.high_stakes,
            source,
            created_at: now,
            created_by: current_user_id(),
            verified_at: now,
            expires_at: Some(now + Duration::days(ctx.freshness_days as i64)),
            reviewed_at: None,
            reviewed_by: None,
            review_note,
            superseded_by: None,
        };
        self.store.upsert_item(&item)?;
        Ok(item.id)
    }

    /// Retire a superseded provisional item: forget its Memory and point its
    /// `superseded_by` at the replacement. Run AFTER the replacement is inserted
    /// so the forward link references a row that exists; a crash between leaves
    /// the old item live (a harmless duplicate), never a dangling link.
    async fn retire_superseded(&self, old: &KnowledgeItem, replacement: Uuid) {
        if let Some(mid) = &old.memory_id {
            let _ = self.memory.forget(&MemoryId(mid.clone())).await;
        }
        let retired = KnowledgeItem {
            status: ItemStatus::Superseded,
            superseded_by: Some(replacement),
            memory_id: None,
            ..old.clone()
        };
        if let Err(e) = self.store.upsert_item(&retired) {
            tracing::warn!(item = %old.id, error = %e, "self-learning: failed to retire superseded item");
        }
    }
}

pub(super) fn current_user_id() -> String {
    CURRENT_USER_ID.try_with(|u| u.clone()).unwrap_or_default()
}

/// Short provenance label for an item's source — the skill name, else the kind.
/// Shared with the surface renderer and the review re-index path.
pub(super) fn source_provenance(source: &ItemSource) -> &str {
    source
        .skill
        .as_deref()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| source.kind.as_str())
}

/// The backing Memory for a knowledge item: body = claim, self-learned tags
/// (so it recalls in the `## Self-Learned Knowledge` block and is excluded from
/// generic recall), organisation-global, curated. `importance` is the only knob
/// that differs between provisional (record) and approved (review re-index).
pub(super) fn memory_for_item(
    agent_id: &str,
    plan_id: &str,
    topic: &str,
    claim: &str,
    provenance: &str,
    importance: f32,
) -> Memory {
    let mut m = Memory::new(
        claim.to_string(),
        MemoryLayer::Organisation,
        AgentId(agent_id.to_string()),
    );
    m.visibility = Visibility::Global;
    m.importance = importance;
    m.source = MemorySource::Curated;
    m.tags = vec![
        SELF_LEARNED_TAG.to_string(),
        format!("{PLAN_TAG_PREFIX}{plan_id}"),
        format!("{TOPIC_TAG_PREFIX}{topic}"),
        format!("{SOURCE_TAG_PREFIX}{provenance}"),
    ];
    m
}

#[async_trait]
impl Tool for SelfLearningRecordTool {
    fn name(&self) -> &str {
        "self_learning.record"
    }

    fn description(&self) -> &str {
        "Record one atomic, primary-cited fact you learned from a source during this curation \
         turn. Every fact REQUIRES a `source.reference` (page id, url, query, or entity) — a claim \
         with no resolvable source is rejected. Mark pricing/security/contractual/legal facts \
         `high_stakes`. To correct an out-of-date approved fact, pass `supersedes` with its item \
         id rather than recording a duplicate."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "topic": { "type": "string", "description": "Short topic this fact belongs to (groups related items)." },
                "claim": { "type": "string", "description": "The atomic fact, stated plainly." },
                "confidence": { "type": "number", "minimum": 0, "maximum": 1, "description": "Your self-assessed confidence, 0..1." },
                "high_stakes": { "type": "boolean", "description": "True for pricing/security/contractual/legal claims (stored but not surfaced until a human approves)." },
                "supersedes": { "type": "string", "description": "Optional item id of an approved fact this corrects." },
                "source": {
                    "type": "object",
                    "properties": {
                        "kind": { "type": "string", "enum": ["skill", "codebase", "knowledge"], "description": "Source category (default skill)." },
                        "skill": { "type": "string", "description": "Originating skill/surface, e.g. 'notion'." },
                        "reference": { "type": "string", "description": "Primary source reference: page id, url, query, or entity. REQUIRED." },
                        "excerpt": { "type": "string", "description": "Short verbatim support snippet." }
                    },
                    "required": ["reference"]
                }
            },
            "required": ["topic", "claim", "source"]
        })
    }

    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = Instant::now();
        let elapsed = || start.elapsed().as_millis() as u64;

        let ctx = self.ctx.clone();

        let params: RecordInput = match serde_json::from_value(input) {
            Ok(p) => p,
            Err(e) => {
                return Ok(ToolResult::failure(
                    format!("Invalid input: {e}"),
                    elapsed(),
                ))
            }
        };

        // Invariant #1: a primary source is required.
        if params.source.reference.trim().is_empty() {
            return Ok(ToolResult::failure(
                "primary source required: `source.reference` must not be empty.",
                elapsed(),
            ));
        }
        if params.claim.trim().is_empty() {
            return Ok(ToolResult::failure("`claim` must not be empty.", elapsed()));
        }

        // Resolve the supersedes assertion (design §6.2 step 2). Distinguish
        // "not requested" from "requested but unresolvable" — only the former
        // reaches `reconcile` as `None`; the latter fails the call here.
        let supersedes_target = match params.supersedes.as_deref() {
            None => None,
            Some(raw) => {
                let Ok(id) = Uuid::parse_str(raw.trim()) else {
                    return Ok(ToolResult::failure(
                        format!("`supersedes` is not a valid id: {raw}"),
                        elapsed(),
                    ));
                };
                match self.store.get_item(id) {
                    Ok(Some(target)) if target.agent_id == ctx.agent_id => Some(target),
                    Ok(_) => {
                        return Ok(ToolResult::failure(
                            format!("supersedes target {id} not found for this agent."),
                            elapsed(),
                        ));
                    }
                    Err(e) => {
                        return Ok(ToolResult::failure(format!("store error: {e}"), elapsed()))
                    }
                }
            }
        };

        let hash = content_hash(&ctx.agent_id, &ctx.plan_id, &params.claim);
        let existing_exact = match self.store.find_by_hash(&ctx.agent_id, &hash) {
            Ok(found) => found,
            Err(e) => return Ok(ToolResult::failure(format!("store error: {e}"), elapsed())),
        };

        // Exact match short-circuits before any recall — bump `verified_at`,
        // create no new row, and skip the per-turn budget.
        if let Some(mut existing) = existing_exact.clone() {
            if supersedes_target.is_none() {
                existing.verified_at = Utc::now();
                if let Err(e) = self.store.upsert_item(&existing) {
                    return Ok(ToolResult::failure(format!("store error: {e}"), elapsed()));
                }
                return Ok(ToolResult::success(
                    json!({ "id": existing.id, "action": "unchanged" }),
                    elapsed(),
                ));
            }
        }

        // Budget guard (design §6.2): count only rows we are about to create.
        if ctx.recorded.load(Ordering::Relaxed) >= ctx.max_items {
            return Ok(ToolResult::failure(
                format!(
                    "per-run item budget reached ({} items); stop recording for this run.",
                    ctx.max_items
                ),
                elapsed(),
            ));
        }

        let near_provisional = if supersedes_target.is_none() && existing_exact.is_none() {
            self.find_near_provisional(&ctx, &params).await
        } else {
            None
        };

        let action = reconcile(
            existing_exact.as_ref(),
            near_provisional.as_ref(),
            supersedes_target.as_ref(),
        );

        let source = ItemSource {
            kind: params
                .source
                .kind
                .as_deref()
                .map(SourceKind::parse)
                .unwrap_or(SourceKind::Skill),
            skill: params.source.skill.clone(),
            reference: params.source.reference.trim().to_string(),
            excerpt: params.source.excerpt.clone(),
        };

        // A conflict against an approved item records the new claim with a note
        // and leaves the target approved (the human confirms the swap); the
        // backward pointer lives in the note, not in `superseded_by`.
        let review_note = match action {
            RecordAction::ConflictProvisional => {
                let target = supersedes_target
                    .as_ref()
                    .expect("ConflictProvisional implies a resolved target");
                Some(format!(
                    "conflicts_with approved item {}; pending human confirmation of the swap.",
                    target.id
                ))
            }
            _ => None,
        };

        let new_id = match self
            .insert_item(&ctx, &params, source, hash, review_note)
            .await
        {
            Ok(id) => id,
            Err(e) => return Ok(ToolResult::failure(format!("write failed: {e}"), elapsed())),
        };
        ctx.recorded.fetch_add(1, Ordering::Relaxed);

        // A provisional restatement is retired only after its replacement
        // exists, so its forward `superseded_by` points at a live row.
        if action == RecordAction::Supersede {
            let old = near_provisional
                .as_ref()
                .expect("Supersede implies a near provisional");
            self.retire_superseded(old, new_id).await;
        }

        let action_label = match action {
            RecordAction::ConflictProvisional => "conflict",
            RecordAction::Supersede => "superseded",
            RecordAction::Insert => "created",
            RecordAction::Unchanged => "unchanged",
        };
        Ok(ToolResult::success(
            json!({ "id": new_id, "action": action_label }),
            elapsed(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(status: ItemStatus, claim: &str) -> KnowledgeItem {
        let now = Utc::now();
        KnowledgeItem {
            id: Uuid::new_v4(),
            agent_id: "agent".into(),
            plan_id: "plan".into(),
            topic: "billing".into(),
            claim: claim.into(),
            content_hash: content_hash("agent", "plan", claim),
            memory_id: Some(Uuid::new_v4().to_string()),
            status,
            confidence: 0.8,
            high_stakes: false,
            source: ItemSource {
                kind: SourceKind::Skill,
                skill: Some("notion".into()),
                reference: "page-1".into(),
                excerpt: None,
            },
            created_at: now,
            created_by: "id".into(),
            verified_at: now,
            expires_at: Some(now + Duration::days(30)),
            reviewed_at: None,
            reviewed_by: None,
            review_note: None,
            superseded_by: None,
        }
    }

    #[test]
    fn reconcile_exact_is_unchanged() {
        let exact = item(ItemStatus::Provisional, "billing is monthly");
        assert_eq!(reconcile(Some(&exact), None, None), RecordAction::Unchanged);
    }

    #[test]
    fn reconcile_near_provisional_supersedes() {
        let near = item(ItemStatus::Provisional, "billing happens monthly");
        assert_eq!(reconcile(None, Some(&near), None), RecordAction::Supersede);
    }

    #[test]
    fn reconcile_supersedes_target_is_conflict_provisional() {
        let approved = item(ItemStatus::Approved, "billing is weekly");
        assert_eq!(
            reconcile(None, None, Some(&approved)),
            RecordAction::ConflictProvisional,
        );
    }

    #[test]
    fn reconcile_none_is_insert() {
        assert_eq!(reconcile(None, None, None), RecordAction::Insert);
    }

    #[test]
    fn reconcile_supersedes_target_wins_over_exact_and_near() {
        // An asserted approved-conflict takes precedence even when an exact or
        // near provisional also exists — the human-confirmed swap path.
        let exact = item(ItemStatus::Provisional, "x");
        let near = item(ItemStatus::Provisional, "y");
        let approved = item(ItemStatus::Approved, "z");
        assert_eq!(
            reconcile(Some(&exact), Some(&near), Some(&approved)),
            RecordAction::ConflictProvisional,
        );
    }

    #[test]
    fn reconcile_exact_wins_over_near() {
        let exact = item(ItemStatus::Provisional, "x");
        let near = item(ItemStatus::Provisional, "y");
        assert_eq!(
            reconcile(Some(&exact), Some(&near), None),
            RecordAction::Unchanged,
        );
    }

    #[tokio::test]
    async fn record_succeeds_without_task_local_scope() {
        // The agent runner executes tools on a spawned task where the hub's
        // SELF_LEARNING_CURATION task-local is not in scope; the tool holds its
        // own CurationCtx so recording works regardless. Guards the regression.
        let store: Arc<dyn SelfLearningStore> = Arc::new(
            workforce_self_learning::sqlite::SqliteSelfLearningStore::new(":memory:").unwrap(),
        );
        let memory: Arc<dyn MemoryStore> =
            Arc::new(workforce_memory::in_memory::InMemoryStore::new());
        let ctx = CurationCtx::new("agent".into(), "plan".into(), 50, 30);
        let tool =
            SelfLearningRecordTool::new(store, memory, SelfLearningConfig::default(), ctx.clone());
        let res = tool
            .execute(json!({
                "topic": "billing",
                "claim": "billing is monthly",
                "source": { "reference": "page-1" }
            }))
            .await
            .unwrap();
        assert!(res.success);
        assert_eq!(ctx.recorded.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn record_rejects_blank_reference() {
        let store: Arc<dyn SelfLearningStore> = Arc::new(
            workforce_self_learning::sqlite::SqliteSelfLearningStore::new(":memory:").unwrap(),
        );
        let memory: Arc<dyn MemoryStore> =
            Arc::new(workforce_memory::in_memory::InMemoryStore::new());
        let ctx = CurationCtx::new("agent".into(), "plan".into(), 50, 30);
        let tool = SelfLearningRecordTool::new(store, memory, SelfLearningConfig::default(), ctx);
        let res = tool
            .execute(json!({
                "topic": "billing",
                "claim": "billing is monthly",
                "source": { "reference": "   " }
            }))
            .await
            .unwrap();
        assert!(!res.success);
        assert!(res.error.unwrap().contains("primary source required"));
    }

    #[tokio::test]
    async fn record_inserts_then_dedups_exact() {
        let store: Arc<dyn SelfLearningStore> = Arc::new(
            workforce_self_learning::sqlite::SqliteSelfLearningStore::new(":memory:").unwrap(),
        );
        let memory: Arc<dyn MemoryStore> =
            Arc::new(workforce_memory::in_memory::InMemoryStore::new());
        let ctx = CurationCtx::new("agent".into(), "plan".into(), 50, 30);
        let tool = SelfLearningRecordTool::new(
            Arc::clone(&store),
            memory,
            SelfLearningConfig::default(),
            ctx.clone(),
        );
        let call = |claim: &str| {
            json!({
                "topic": "billing",
                "claim": claim,
                "source": { "skill": "notion", "reference": "page-1" }
            })
        };
        let first = tool.execute(call("billing is monthly")).await.unwrap();
        assert!(first.success);
        assert_eq!(first.output["action"], "created");
        assert_eq!(ctx.recorded.load(Ordering::Relaxed), 1);

        let again = tool.execute(call("billing is monthly")).await.unwrap();
        assert!(again.success);
        assert_eq!(again.output["action"], "unchanged");
        // Exact match must not consume the budget.
        assert_eq!(ctx.recorded.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn record_enforces_per_run_budget() {
        let store: Arc<dyn SelfLearningStore> = Arc::new(
            workforce_self_learning::sqlite::SqliteSelfLearningStore::new(":memory:").unwrap(),
        );
        let memory: Arc<dyn MemoryStore> =
            Arc::new(workforce_memory::in_memory::InMemoryStore::new());
        let ctx = CurationCtx::new("agent".into(), "plan".into(), 1, 30);
        let tool = SelfLearningRecordTool::new(
            Arc::clone(&store),
            memory,
            SelfLearningConfig::default(),
            ctx,
        );
        let first = tool
            .execute(json!({
                "topic": "billing",
                "claim": "billing is monthly",
                "source": { "reference": "page-1" }
            }))
            .await
            .unwrap();
        assert!(first.success);
        let second = tool
            .execute(json!({
                "topic": "billing",
                "claim": "billing proration is daily",
                "source": { "reference": "page-2" }
            }))
            .await
            .unwrap();
        assert!(!second.success);
        assert!(second.error.unwrap().contains("budget"));
    }
}
