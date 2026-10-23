//! Human review transitions for self-learned items: approve / reject / mark for
//! re-verification / delete. Factored out of the REST handlers so the
//! memory-and-status ordering is unit-testable against in-memory stores.
//!
//! Ordering follows design §8: on approve, write the new (higher-importance)
//! Memory BEFORE repointing and forgetting the old, so a crash leaves a harmless
//! duplicate rather than a void link; on reject / reverify / delete, clear the
//! row's link (or drop the row) BEFORE the forget, so a crash leaves a
//! reclaimable orphan the startup sweep reaps, never a row pointing at nothing.

use uuid::Uuid;

use workforce_common::settings::SelfLearningConfig;
use workforce_memory::store::MemoryStore;
use workforce_memory::types::MemoryId;
use workforce_self_learning::{ItemStatus, KnowledgeItem, ReviewMark, SelfLearningStore};

use super::record::{memory_for_item, source_provenance};

/// A review failure, typed so the HTTP layer maps it cleanly: a missing or
/// cross-agent item is a 404 (and is deliberately indistinguishable, so review
/// can't probe for another agent's items), while a store/memory failure is a
/// 500 — never a 400 echoing the raw backend error.
#[derive(Debug)]
pub enum ReviewError {
    NotFound,
    Store(eyre::Report),
}

type ReviewResult<T> = Result<T, ReviewError>;

fn store_err(e: impl Into<eyre::Report>) -> ReviewError {
    ReviewError::Store(e.into())
}

/// Fetch an item, enforcing it belongs to `agent_id` — the URL scope is the
/// auth scope, so review can never reach across agents (a cross-agent item is
/// reported as `NotFound`, not "belongs to someone else").
fn owned_item(
    store: &dyn SelfLearningStore,
    agent_id: &str,
    item_id: Uuid,
) -> ReviewResult<KnowledgeItem> {
    let item = store
        .get_item(item_id)
        .map_err(store_err)?
        .filter(|item| item.agent_id == agent_id)
        .ok_or(ReviewError::NotFound)?;
    Ok(item)
}

/// Approve: re-index the item's Memory at `approved_importance` and mark it
/// Approved. Safe to re-run (re-indexes again).
pub async fn approve(
    store: &dyn SelfLearningStore,
    memory: &dyn MemoryStore,
    cfg: &SelfLearningConfig,
    agent_id: &str,
    item_id: Uuid,
    reviewed_by: &str,
) -> ReviewResult<KnowledgeItem> {
    let item = owned_item(store, agent_id, item_id)?;
    let mem = memory_for_item(
        &item.agent_id,
        &item.plan_id,
        &item.topic,
        &item.claim,
        source_provenance(&item.source),
        cfg.approved_importance,
    );
    let new_id = memory.remember(mem).await.map_err(store_err)?;
    store
        .set_memory_id(item_id, Some(&new_id.0))
        .map_err(store_err)?;
    if let Some(old) = item.memory_id.as_deref() {
        if old != new_id.0 {
            let _ = memory.forget(&MemoryId(old.to_string())).await;
        }
    }
    store
        .set_status(
            item_id,
            ItemStatus::Approved,
            Some(&ReviewMark {
                reviewed_by: reviewed_by.to_string(),
                note: None,
            }),
        )
        .map_err(store_err)?;
    owned_item(store, agent_id, item_id)
}

/// Reject: forget the Memory and record why (it never re-surfaces; the row is
/// kept so a future curation turn is told "do not re-learn").
pub async fn reject(
    store: &dyn SelfLearningStore,
    memory: &dyn MemoryStore,
    agent_id: &str,
    item_id: Uuid,
    reviewed_by: &str,
    note: Option<String>,
) -> ReviewResult<KnowledgeItem> {
    let item = owned_item(store, agent_id, item_id)?;
    forget_and_set(
        store,
        memory,
        &item,
        ItemStatus::Rejected,
        ReviewMark {
            reviewed_by: reviewed_by.to_string(),
            note,
        },
    )
    .await?;
    owned_item(store, agent_id, item_id)
}

/// Mark stale so it stops surfacing and the next run of its plan re-verifies it
/// against source.
pub async fn mark_for_reverify(
    store: &dyn SelfLearningStore,
    memory: &dyn MemoryStore,
    agent_id: &str,
    item_id: Uuid,
    reviewed_by: &str,
) -> ReviewResult<KnowledgeItem> {
    let item = owned_item(store, agent_id, item_id)?;
    forget_and_set(
        store,
        memory,
        &item,
        ItemStatus::Stale,
        ReviewMark {
            reviewed_by: reviewed_by.to_string(),
            note: None,
        },
    )
    .await?;
    owned_item(store, agent_id, item_id)
}

/// Delete: drop the row, then forget its Memory.
pub async fn delete(
    store: &dyn SelfLearningStore,
    memory: &dyn MemoryStore,
    agent_id: &str,
    item_id: Uuid,
) -> ReviewResult<()> {
    let item = owned_item(store, agent_id, item_id)?;
    store.delete_item(item_id).map_err(store_err)?;
    if let Some(mid) = item.memory_id.as_deref() {
        let _ = memory.forget(&MemoryId(mid.to_string())).await;
    }
    Ok(())
}

/// Shared reject/reverify body: clear the link, forget the Memory, set status.
async fn forget_and_set(
    store: &dyn SelfLearningStore,
    memory: &dyn MemoryStore,
    item: &KnowledgeItem,
    status: ItemStatus,
    mark: ReviewMark,
) -> ReviewResult<()> {
    store.set_memory_id(item.id, None).map_err(store_err)?;
    if let Some(mid) = item.memory_id.as_deref() {
        let _ = memory.forget(&MemoryId(mid.to_string())).await;
    }
    store
        .set_status(item.id, status, Some(&mark))
        .map_err(store_err)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, Utc};
    use workforce_memory::in_memory::InMemoryStore;
    use workforce_self_learning::sqlite::SqliteSelfLearningStore;
    use workforce_self_learning::{content_hash, ItemSource, SourceKind};

    fn cfg() -> SelfLearningConfig {
        SelfLearningConfig::default()
    }

    fn provisional(agent: &str, claim: &str, memory_id: Option<String>) -> KnowledgeItem {
        let now = Utc::now();
        KnowledgeItem {
            id: Uuid::new_v4(),
            agent_id: agent.into(),
            plan_id: "p".into(),
            topic: "billing".into(),
            claim: claim.into(),
            content_hash: content_hash(agent, "p", claim),
            memory_id,
            status: ItemStatus::Provisional,
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

    async fn seeded() -> (SqliteSelfLearningStore, InMemoryStore, KnowledgeItem) {
        let store = SqliteSelfLearningStore::in_memory().unwrap();
        let memory = InMemoryStore::new();
        let mem = memory_for_item("a", "p", "billing", "billing is monthly", "notion", 0.5);
        let mid = memory.remember(mem).await.unwrap();
        let item = provisional("a", "billing is monthly", Some(mid.0.clone()));
        store.upsert_item(&item).unwrap();
        (store, memory, item)
    }

    #[tokio::test]
    async fn approve_reindexes_and_marks_approved() {
        let (store, memory, item) = seeded().await;
        let old_mid = item.memory_id.clone().unwrap();

        let out = approve(&store, &memory, &cfg(), "a", item.id, "boss")
            .await
            .unwrap();
        assert_eq!(out.status, ItemStatus::Approved);
        assert_eq!(out.reviewed_by.as_deref(), Some("boss"));
        // Re-indexed: new memory id, old one forgotten, importance raised.
        let new_mid = out.memory_id.clone().unwrap();
        assert_ne!(new_mid, old_mid);
        assert!(memory
            .get(&MemoryId(old_mid))
            .await
            .unwrap()
            .is_none_or(|m| m.deleted));
        let live = memory.get(&MemoryId(new_mid)).await.unwrap().unwrap();
        assert_eq!(live.importance, cfg().approved_importance);
    }

    #[tokio::test]
    async fn reject_forgets_memory_and_records_note() {
        let (store, memory, item) = seeded().await;
        let mid = item.memory_id.clone().unwrap();
        let out = reject(&store, &memory, "a", item.id, "boss", Some("wrong".into()))
            .await
            .unwrap();
        assert_eq!(out.status, ItemStatus::Rejected);
        assert_eq!(out.review_note.as_deref(), Some("wrong"));
        assert!(out.memory_id.is_none());
        assert!(memory
            .get(&MemoryId(mid))
            .await
            .unwrap()
            .is_none_or(|m| m.deleted));
    }

    #[tokio::test]
    async fn reverify_marks_stale_and_unlinks() {
        let (store, memory, item) = seeded().await;
        let out = mark_for_reverify(&store, &memory, "a", item.id, "boss")
            .await
            .unwrap();
        assert_eq!(out.status, ItemStatus::Stale);
        assert!(out.memory_id.is_none());
    }

    #[tokio::test]
    async fn delete_removes_row_and_memory() {
        let (store, memory, item) = seeded().await;
        let mid = item.memory_id.clone().unwrap();
        delete(&store, &memory, "a", item.id).await.unwrap();
        assert!(store.get_item(item.id).unwrap().is_none());
        assert!(memory
            .get(&MemoryId(mid))
            .await
            .unwrap()
            .is_none_or(|m| m.deleted));
    }

    #[tokio::test]
    async fn cross_agent_review_is_refused() {
        let (store, memory, item) = seeded().await;
        // Item belongs to "a"; a request scoped to "b" must not touch it.
        assert!(approve(&store, &memory, &cfg(), "b", item.id, "boss")
            .await
            .is_err());
        assert!(reject(&store, &memory, "b", item.id, "boss", None)
            .await
            .is_err());
        assert!(delete(&store, &memory, "b", item.id).await.is_err());
        assert_eq!(
            store.get_item(item.id).unwrap().unwrap().status,
            ItemStatus::Provisional
        );
    }
}
