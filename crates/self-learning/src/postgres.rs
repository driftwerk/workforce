//! Postgres backend for [`SelfLearningStore`].
//!
//! Mirrors every method of the SQLite backend at
//! [`crate::sqlite::SqliteSelfLearningStore`]. Selected at runtime via
//! `[self_learning] backend = "postgres"` in `workforce.toml`; the
//! [`SelfLearningStore`] trait is stable so call sites are unchanged
//! regardless of which backend is in use.
//!
//! The async-to-sync bridge and per-store-pool architecture are the
//! same as `workforce-training`; see that crate's `postgres.rs` doc
//! comment for the full rationale.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use eyre::{Context, Result};
use tokio_postgres::Row;
use uuid::Uuid;
use workforce_postgres::{
    MigrationRun, MigrationRunner, MigrationSet, PostgresConfig, PostgresError, PostgresPool,
};

use crate::model::{
    DeepNode, ItemQuery, ItemSource, ItemStatus, KnowledgeItem, NodeParent, NodeQuery, NodeStatus,
    ReviewMark, SourceKind, SurfaceItem,
};
use crate::store::{DeepExpertiseStore, SelfLearningStore};

mod embedded {
    refinery::embed_migrations!("migrations");
}

pub struct PostgresSelfLearningStore {
    pool: PostgresPool,
}

impl PostgresSelfLearningStore {
    pub const SCHEMA: &'static str = "self_learning";

    pub fn build_pool(config: &PostgresConfig) -> Result<PostgresPool, PostgresError> {
        PostgresPool::from_config_with_session_init(
            config,
            &["SET search_path TO self_learning, public"],
        )
    }

    pub async fn new(config: &PostgresConfig) -> Result<Arc<Self>> {
        let pool = Self::build_pool(config).context("building self-learning-store pool")?;
        Self::with_pool(pool).await
    }

    pub async fn with_pool(pool: PostgresPool) -> Result<Arc<Self>> {
        let migration_set = MigrationSet::new(
            Self::SCHEMA,
            Box::new(EmbeddedSelfLearningMigrations) as Box<dyn MigrationRun>,
        );
        MigrationRunner::run(&pool, &[migration_set])
            .await
            .context("running self-learning-store migrations")?;
        Ok(Arc::new(Self { pool }))
    }

    fn block_on<F, T>(&self, fut: F) -> Result<T>
    where
        F: std::future::Future<Output = Result<T>>,
    {
        tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(fut))
    }
}

struct EmbeddedSelfLearningMigrations;

#[async_trait::async_trait]
impl MigrationRun for EmbeddedSelfLearningMigrations {
    async fn run(&self, client: &mut tokio_postgres::Client) -> Result<(), PostgresError> {
        embedded::migrations::runner()
            .run_async(client)
            .await
            .map_err(PostgresError::from)?;
        Ok(())
    }
}

const ITEM_COLUMNS: &str = "id, agent_id, plan_id, topic, claim, content_hash, memory_id, status, \
     confidence, high_stakes, source_kind, source_skill, source_reference, source_excerpt, \
     created_at, created_by, verified_at, expires_at, reviewed_at, reviewed_by, review_note, \
     superseded_by";

fn item_from_row(row: &Row) -> Result<KnowledgeItem> {
    let status: String = row.try_get("status")?;
    let source_kind: String = row.try_get("source_kind")?;
    Ok(KnowledgeItem {
        id: row.try_get("id")?,
        agent_id: row.try_get("agent_id")?,
        plan_id: row.try_get("plan_id")?,
        topic: row.try_get("topic")?,
        claim: row.try_get("claim")?,
        content_hash: row.try_get("content_hash")?,
        memory_id: row.try_get("memory_id")?,
        status: ItemStatus::parse(&status),
        confidence: row.try_get("confidence")?,
        high_stakes: row.try_get("high_stakes")?,
        source: ItemSource {
            kind: SourceKind::parse(&source_kind),
            skill: row.try_get("source_skill")?,
            reference: row.try_get("source_reference")?,
            excerpt: row.try_get("source_excerpt")?,
        },
        created_at: row.try_get("created_at")?,
        created_by: row.try_get("created_by")?,
        verified_at: row.try_get("verified_at")?,
        expires_at: row.try_get("expires_at")?,
        reviewed_at: row.try_get("reviewed_at")?,
        reviewed_by: row.try_get("reviewed_by")?,
        review_note: row.try_get("review_note")?,
        superseded_by: row.try_get("superseded_by")?,
    })
}

const NODE_COLUMNS: &str = "id, agent_id, plan_id, parent_id, depth, title, path_hash, body, \
     sources, coverage, memory_id, status, confidence, created_at, created_by, verified_at, \
     expires_at, reviewed_at, reviewed_by, review_note, superseded_by";

fn node_from_row(row: &Row) -> Result<DeepNode> {
    let depth: i32 = row.try_get("depth")?;
    let sources: String = row.try_get("sources")?;
    let coverage: String = row.try_get("coverage")?;
    let status: String = row.try_get("status")?;
    Ok(DeepNode {
        id: row.try_get("id")?,
        agent_id: row.try_get("agent_id")?,
        plan_id: row.try_get("plan_id")?,
        parent_id: row.try_get("parent_id")?,
        depth: depth as u32,
        title: row.try_get("title")?,
        path_hash: row.try_get("path_hash")?,
        body: row.try_get("body")?,
        sources: serde_json::from_str::<Vec<ItemSource>>(&sources)
            .context("decode node sources")?,
        coverage: serde_json::from_str::<Vec<SurfaceItem>>(&coverage)
            .context("decode node coverage")?,
        memory_id: row.try_get("memory_id")?,
        status: NodeStatus::parse(&status),
        confidence: row.try_get("confidence")?,
        created_at: row.try_get("created_at")?,
        created_by: row.try_get("created_by")?,
        verified_at: row.try_get("verified_at")?,
        expires_at: row.try_get("expires_at")?,
        reviewed_at: row.try_get("reviewed_at")?,
        reviewed_by: row.try_get("reviewed_by")?,
        review_note: row.try_get("review_note")?,
        superseded_by: row.try_get("superseded_by")?,
    })
}

impl SelfLearningStore for PostgresSelfLearningStore {
    fn upsert_item(&self, item: &KnowledgeItem) -> Result<()> {
        let item = item.clone();
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let status = item.status.as_str().to_string();
            let source_kind = item.source.kind.as_str().to_string();
            conn.execute(
                "INSERT INTO self_learning_items \
                     (id, agent_id, plan_id, topic, claim, content_hash, memory_id, status, \
                      confidence, high_stakes, source_kind, source_skill, source_reference, \
                      source_excerpt, created_at, created_by, verified_at, expires_at, \
                      reviewed_at, reviewed_by, review_note, superseded_by) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, \
                         $15, $16, $17, $18, $19, $20, $21, $22) \
                 ON CONFLICT(id) DO UPDATE SET \
                    agent_id = excluded.agent_id, \
                    plan_id = excluded.plan_id, \
                    topic = excluded.topic, \
                    claim = excluded.claim, \
                    content_hash = excluded.content_hash, \
                    memory_id = excluded.memory_id, \
                    status = excluded.status, \
                    confidence = excluded.confidence, \
                    high_stakes = excluded.high_stakes, \
                    source_kind = excluded.source_kind, \
                    source_skill = excluded.source_skill, \
                    source_reference = excluded.source_reference, \
                    source_excerpt = excluded.source_excerpt, \
                    created_at = excluded.created_at, \
                    created_by = excluded.created_by, \
                    verified_at = excluded.verified_at, \
                    expires_at = excluded.expires_at, \
                    reviewed_at = excluded.reviewed_at, \
                    reviewed_by = excluded.reviewed_by, \
                    review_note = excluded.review_note, \
                    superseded_by = excluded.superseded_by",
                &[
                    &item.id,
                    &item.agent_id,
                    &item.plan_id,
                    &item.topic,
                    &item.claim,
                    &item.content_hash,
                    &item.memory_id,
                    &status,
                    &item.confidence,
                    &item.high_stakes,
                    &source_kind,
                    &item.source.skill,
                    &item.source.reference,
                    &item.source.excerpt,
                    &item.created_at,
                    &item.created_by,
                    &item.verified_at,
                    &item.expires_at,
                    &item.reviewed_at,
                    &item.reviewed_by,
                    &item.review_note,
                    &item.superseded_by,
                ],
            )
            .await
            .context("upsert item")?;
            Ok(())
        })
    }

    fn get_item(&self, id: Uuid) -> Result<Option<KnowledgeItem>> {
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let row = conn
                .query_opt(
                    &format!("SELECT {ITEM_COLUMNS} FROM self_learning_items WHERE id = $1"),
                    &[&id],
                )
                .await?;
            row.as_ref().map(item_from_row).transpose()
        })
    }

    fn find_by_hash(&self, agent_id: &str, content_hash: &str) -> Result<Option<KnowledgeItem>> {
        let agent_id = agent_id.to_string();
        let content_hash = content_hash.to_string();
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let row = conn
                .query_opt(
                    &format!(
                        "SELECT {ITEM_COLUMNS} FROM self_learning_items \
                         WHERE agent_id = $1 AND content_hash = $2 LIMIT 1"
                    ),
                    &[&agent_id, &content_hash],
                )
                .await?;
            row.as_ref().map(item_from_row).transpose()
        })
    }

    fn list_items(&self, agent_id: &str, filter: &ItemQuery) -> Result<Vec<KnowledgeItem>> {
        let agent_id = agent_id.to_string();
        let plan_id = filter.plan_id.clone();
        let topic = filter.topic.clone();
        let status = filter.status.map(|s| s.as_str().to_string());
        let limit = filter.limit;
        self.block_on(async move {
            let conn = self.pool.get().await?;
            // `LIMIT -1` is SQLite-only; in Postgres an absent limit is
            // expressed by `LIMIT ALL`.
            let limit_clause = match limit {
                Some(n) => format!("LIMIT {n}"),
                None => "LIMIT ALL".to_string(),
            };
            // Casts pin the parameter type so the NULL-guards don't trip
            // "could not determine data type of parameter".
            let rows = conn
                .query(
                    &format!(
                        "SELECT {ITEM_COLUMNS} FROM self_learning_items \
                         WHERE agent_id = $1 \
                           AND ($2::text IS NULL OR plan_id = $2) \
                           AND ($3::text IS NULL OR topic = $3) \
                           AND ($4::text IS NULL OR status = $4) \
                         ORDER BY created_at DESC, id \
                         {limit_clause}"
                    ),
                    &[&agent_id, &plan_id, &topic, &status],
                )
                .await?;
            rows.iter().map(item_from_row).collect()
        })
    }

    fn items_by_memory_ids(&self, memory_ids: &[String]) -> Result<Vec<KnowledgeItem>> {
        if memory_ids.is_empty() {
            return Ok(Vec::new());
        }
        let memory_ids = memory_ids.to_vec();
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let rows = conn
                .query(
                    &format!(
                        "SELECT {ITEM_COLUMNS} FROM self_learning_items \
                         WHERE memory_id = ANY($1)"
                    ),
                    &[&memory_ids],
                )
                .await?;
            rows.iter().map(item_from_row).collect()
        })
    }

    fn set_status(
        &self,
        id: Uuid,
        status: ItemStatus,
        review: Option<&ReviewMark>,
    ) -> Result<bool> {
        let status = status.as_str().to_string();
        let review = review.cloned();
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let n = match review {
                Some(mark) => {
                    let now: DateTime<Utc> = Utc::now();
                    conn.execute(
                        "UPDATE self_learning_items \
                         SET status = $1, reviewed_at = $2, reviewed_by = $3, review_note = $4 \
                         WHERE id = $5",
                        &[&status, &now, &mark.reviewed_by, &mark.note, &id],
                    )
                    .await?
                }
                None => {
                    conn.execute(
                        "UPDATE self_learning_items SET status = $1 WHERE id = $2",
                        &[&status, &id],
                    )
                    .await?
                }
            };
            Ok(n > 0)
        })
    }

    fn set_memory_id(&self, id: Uuid, memory_id: Option<&str>) -> Result<()> {
        let memory_id = memory_id.map(str::to_string);
        self.block_on(async move {
            let conn = self.pool.get().await?;
            conn.execute(
                "UPDATE self_learning_items SET memory_id = $1 WHERE id = $2",
                &[&memory_id, &id],
            )
            .await
            .context("set memory id")?;
            Ok(())
        })
    }

    fn list_all_memory_ids(&self) -> Result<Vec<String>> {
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let rows = conn
                .query(
                    "SELECT memory_id FROM self_learning_items WHERE memory_id IS NOT NULL",
                    &[],
                )
                .await?;
            rows.iter()
                .map(|r| r.try_get::<_, String>(0).context("read memory_id"))
                .collect()
        })
    }

    fn delete_item(&self, id: Uuid) -> Result<bool> {
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let n = conn
                .execute("DELETE FROM self_learning_items WHERE id = $1", &[&id])
                .await?;
            Ok(n > 0)
        })
    }

    fn due_for_reverify(&self, agent_id: &str, now: DateTime<Utc>) -> Result<Vec<KnowledgeItem>> {
        let agent_id = agent_id.to_string();
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let rows = conn
                .query(
                    &format!(
                        "SELECT {ITEM_COLUMNS} FROM self_learning_items \
                         WHERE agent_id = $1 \
                           AND expires_at IS NOT NULL \
                           AND expires_at <= $2 \
                           AND status IN ('approved', 'provisional') \
                         ORDER BY expires_at, id"
                    ),
                    &[&agent_id, &now],
                )
                .await?;
            rows.iter().map(item_from_row).collect()
        })
    }
}

impl DeepExpertiseStore for PostgresSelfLearningStore {
    fn upsert_node(&self, node: &DeepNode) -> Result<()> {
        let node = node.clone();
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let depth = node.depth as i32;
            let sources = serde_json::to_string(&node.sources).context("serialize node sources")?;
            let coverage =
                serde_json::to_string(&node.coverage).context("serialize node coverage")?;
            let status = node.status.as_str().to_string();
            conn.execute(
                "INSERT INTO deep_nodes \
                     (id, agent_id, plan_id, parent_id, depth, title, path_hash, body, sources, \
                      coverage, memory_id, status, confidence, created_at, created_by, \
                      verified_at, expires_at, reviewed_at, reviewed_by, review_note, \
                      superseded_by) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, \
                         $17, $18, $19, $20, $21) \
                 ON CONFLICT(id) DO UPDATE SET \
                    agent_id = excluded.agent_id, \
                    plan_id = excluded.plan_id, \
                    parent_id = excluded.parent_id, \
                    depth = excluded.depth, \
                    title = excluded.title, \
                    path_hash = excluded.path_hash, \
                    body = excluded.body, \
                    sources = excluded.sources, \
                    coverage = excluded.coverage, \
                    memory_id = excluded.memory_id, \
                    status = excluded.status, \
                    confidence = excluded.confidence, \
                    created_at = excluded.created_at, \
                    created_by = excluded.created_by, \
                    verified_at = excluded.verified_at, \
                    expires_at = excluded.expires_at, \
                    reviewed_at = excluded.reviewed_at, \
                    reviewed_by = excluded.reviewed_by, \
                    review_note = excluded.review_note, \
                    superseded_by = excluded.superseded_by",
                &[
                    &node.id,
                    &node.agent_id,
                    &node.plan_id,
                    &node.parent_id,
                    &depth,
                    &node.title,
                    &node.path_hash,
                    &node.body,
                    &sources,
                    &coverage,
                    &node.memory_id,
                    &status,
                    &node.confidence,
                    &node.created_at,
                    &node.created_by,
                    &node.verified_at,
                    &node.expires_at,
                    &node.reviewed_at,
                    &node.reviewed_by,
                    &node.review_note,
                    &node.superseded_by,
                ],
            )
            .await
            .context("upsert node")?;
            Ok(())
        })
    }

    fn get_node(&self, id: Uuid) -> Result<Option<DeepNode>> {
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let row = conn
                .query_opt(
                    &format!("SELECT {NODE_COLUMNS} FROM deep_nodes WHERE id = $1"),
                    &[&id],
                )
                .await?;
            row.as_ref().map(node_from_row).transpose()
        })
    }

    fn find_by_path(&self, agent_id: &str, path_hash: &str) -> Result<Option<DeepNode>> {
        let agent_id = agent_id.to_string();
        let path_hash = path_hash.to_string();
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let row = conn
                .query_opt(
                    &format!(
                        "SELECT {NODE_COLUMNS} FROM deep_nodes \
                         WHERE agent_id = $1 AND path_hash = $2 LIMIT 1"
                    ),
                    &[&agent_id, &path_hash],
                )
                .await?;
            row.as_ref().map(node_from_row).transpose()
        })
    }

    fn list_nodes(&self, agent_id: &str, filter: &NodeQuery) -> Result<Vec<DeepNode>> {
        let agent_id = agent_id.to_string();
        let plan_id = filter.plan_id.clone();
        let status = filter.status.map(|s| s.as_str().to_string());
        let (roots_only, of_parent): (i32, Option<Uuid>) = match filter.parent {
            NodeParent::Any => (0, None),
            NodeParent::Roots => (1, None),
            NodeParent::Of(id) => (0, Some(id)),
        };
        let limit = filter.limit;
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let limit_clause = match limit {
                Some(n) => format!("LIMIT {n}"),
                None => "LIMIT ALL".to_string(),
            };
            let rows = conn
                .query(
                    &format!(
                        "SELECT {NODE_COLUMNS} FROM deep_nodes \
                         WHERE agent_id = $1 \
                           AND ($2::text IS NULL OR plan_id = $2) \
                           AND ($3 = 0 OR parent_id IS NULL) \
                           AND ($4::uuid IS NULL OR parent_id = $4) \
                           AND ($5::text IS NULL OR status = $5) \
                         ORDER BY created_at DESC, id \
                         {limit_clause}"
                    ),
                    &[&agent_id, &plan_id, &roots_only, &of_parent, &status],
                )
                .await?;
            rows.iter().map(node_from_row).collect()
        })
    }

    fn nodes_by_memory_ids(&self, memory_ids: &[String]) -> Result<Vec<DeepNode>> {
        if memory_ids.is_empty() {
            return Ok(Vec::new());
        }
        let memory_ids = memory_ids.to_vec();
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let rows = conn
                .query(
                    &format!("SELECT {NODE_COLUMNS} FROM deep_nodes WHERE memory_id = ANY($1)"),
                    &[&memory_ids],
                )
                .await?;
            rows.iter().map(node_from_row).collect()
        })
    }

    fn set_status(
        &self,
        id: Uuid,
        status: NodeStatus,
        review: Option<&ReviewMark>,
    ) -> Result<bool> {
        let status = status.as_str().to_string();
        let review = review.cloned();
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let n = match review {
                Some(mark) => {
                    let now: DateTime<Utc> = Utc::now();
                    conn.execute(
                        "UPDATE deep_nodes \
                         SET status = $1, reviewed_at = $2, reviewed_by = $3, review_note = $4 \
                         WHERE id = $5",
                        &[&status, &now, &mark.reviewed_by, &mark.note, &id],
                    )
                    .await?
                }
                None => {
                    conn.execute(
                        "UPDATE deep_nodes SET status = $1 WHERE id = $2",
                        &[&status, &id],
                    )
                    .await?
                }
            };
            Ok(n > 0)
        })
    }

    fn set_memory_id(&self, id: Uuid, memory_id: Option<&str>) -> Result<()> {
        let memory_id = memory_id.map(str::to_string);
        self.block_on(async move {
            let conn = self.pool.get().await?;
            conn.execute(
                "UPDATE deep_nodes SET memory_id = $1 WHERE id = $2",
                &[&memory_id, &id],
            )
            .await
            .context("set node memory id")?;
            Ok(())
        })
    }

    fn list_all_memory_ids(&self) -> Result<Vec<String>> {
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let rows = conn
                .query(
                    "SELECT memory_id FROM deep_nodes WHERE memory_id IS NOT NULL",
                    &[],
                )
                .await?;
            rows.iter()
                .map(|r| r.try_get::<_, String>(0).context("read memory_id"))
                .collect()
        })
    }

    fn delete_subtree(&self, id: Uuid) -> Result<u64> {
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let n = conn
                .execute(
                    "WITH RECURSIVE subtree(id) AS ( \
                         SELECT id FROM deep_nodes WHERE id = $1 \
                         UNION ALL \
                         SELECT n.id FROM deep_nodes n \
                         JOIN subtree s ON n.parent_id = s.id \
                     ) \
                     DELETE FROM deep_nodes WHERE id IN (SELECT id FROM subtree)",
                    &[&id],
                )
                .await?;
            Ok(n)
        })
    }

    fn due_for_reverify(&self, agent_id: &str, now: DateTime<Utc>) -> Result<Vec<DeepNode>> {
        let agent_id = agent_id.to_string();
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let rows = conn
                .query(
                    &format!(
                        "SELECT {NODE_COLUMNS} FROM deep_nodes \
                         WHERE agent_id = $1 \
                           AND expires_at IS NOT NULL \
                           AND expires_at <= $2 \
                           AND status IN ('verified', 'approved', 'modelled') \
                         ORDER BY expires_at, id"
                    ),
                    &[&agent_id, &now],
                )
                .await?;
            rows.iter().map(node_from_row).collect()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{content_hash, node_path_hash};
    use chrono::Duration;
    use workforce_postgres::test_support::{now_micros, with_test_pool};

    async fn with_embedded_pg<F, Fut>(test: F)
    where
        F: FnOnce(Arc<PostgresSelfLearningStore>) -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        with_test_pool(PostgresSelfLearningStore::build_pool, |pool| async move {
            let store = PostgresSelfLearningStore::with_pool(pool)
                .await
                .expect("construct store");
            test(store).await;
        })
        .await;
    }

    fn make_item(agent: &str, plan: &str, topic: &str, claim: &str) -> KnowledgeItem {
        let now = now_micros();
        KnowledgeItem {
            id: Uuid::new_v4(),
            agent_id: agent.into(),
            plan_id: plan.into(),
            topic: topic.into(),
            claim: claim.into(),
            content_hash: content_hash(agent, plan, claim),
            memory_id: Some(Uuid::new_v4().to_string()),
            status: ItemStatus::Provisional,
            confidence: 0.8,
            high_stakes: false,
            source: ItemSource {
                kind: SourceKind::Skill,
                skill: Some("notion".into()),
                reference: "page-123".into(),
                excerpt: Some("verbatim".into()),
            },
            created_at: now,
            created_by: "identity-1".into(),
            verified_at: now,
            expires_at: Some(now + Duration::days(30)),
            reviewed_at: None,
            reviewed_by: None,
            review_note: None,
            superseded_by: None,
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn parity_upsert_get_round_trip() {
        with_embedded_pg(|store| async move {
            let s: Arc<dyn SelfLearningStore> = store;
            let item = make_item("mark", "billing", "pricing", "billing is monthly");
            s.upsert_item(&item).unwrap();
            let got = s.get_item(item.id).unwrap().unwrap();
            assert_eq!(got, item);
            assert!(s.get_item(Uuid::new_v4()).unwrap().is_none());
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn parity_find_by_hash_and_unique() {
        with_embedded_pg(|store| async move {
            let s: Arc<dyn SelfLearningStore> = store;
            let item = make_item("mark", "billing", "pricing", "billing is monthly");
            s.upsert_item(&item).unwrap();
            assert_eq!(
                s.find_by_hash("mark", &item.content_hash).unwrap(),
                Some(item.clone())
            );
            assert!(s
                .find_by_hash("alice", &item.content_hash)
                .unwrap()
                .is_none());

            let mut dup = make_item("mark", "billing", "pricing", "billing is monthly");
            dup.id = Uuid::new_v4();
            assert!(s.upsert_item(&dup).is_err());
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn parity_list_items_filters() {
        with_embedded_pg(|store| async move {
            let s: Arc<dyn SelfLearningStore> = store;
            let mut a = make_item("mark", "billing", "pricing", "billing is monthly");
            a.status = ItemStatus::Approved;
            let b = make_item("mark", "billing", "refunds", "refunds are net");
            let c = make_item("mark", "product", "pricing", "tiers exist");
            s.upsert_item(&a).unwrap();
            s.upsert_item(&b).unwrap();
            s.upsert_item(&c).unwrap();

            assert_eq!(
                s.list_items("mark", &ItemQuery::default()).unwrap().len(),
                3
            );
            assert_eq!(
                s.list_items(
                    "mark",
                    &ItemQuery {
                        plan_id: Some("billing".into()),
                        ..Default::default()
                    }
                )
                .unwrap()
                .len(),
                2
            );
            assert_eq!(
                s.list_items(
                    "mark",
                    &ItemQuery {
                        topic: Some("pricing".into()),
                        ..Default::default()
                    }
                )
                .unwrap()
                .len(),
                2
            );
            let by_status = s
                .list_items(
                    "mark",
                    &ItemQuery {
                        status: Some(ItemStatus::Approved),
                        ..Default::default()
                    },
                )
                .unwrap();
            assert_eq!(by_status.len(), 1);
            assert_eq!(by_status[0].id, a.id);
            assert_eq!(
                s.list_items(
                    "mark",
                    &ItemQuery {
                        limit: Some(1),
                        ..Default::default()
                    }
                )
                .unwrap()
                .len(),
                1
            );
            assert!(s
                .list_items("alice", &ItemQuery::default())
                .unwrap()
                .is_empty());
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn parity_items_by_memory_ids() {
        with_embedded_pg(|store| async move {
            let s: Arc<dyn SelfLearningStore> = store;
            let a = make_item("mark", "billing", "pricing", "a");
            let b = make_item("mark", "billing", "pricing", "b");
            s.upsert_item(&a).unwrap();
            s.upsert_item(&b).unwrap();
            let ids = vec![
                a.memory_id.clone().unwrap(),
                b.memory_id.clone().unwrap(),
                "ghost".into(),
            ];
            let mut got = s.items_by_memory_ids(&ids).unwrap();
            got.sort_by(|x, y| x.claim.cmp(&y.claim));
            assert_eq!(got.len(), 2);
            assert_eq!(got[0].id, a.id);
            assert_eq!(got[1].id, b.id);
            assert!(s.items_by_memory_ids(&[]).unwrap().is_empty());
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn parity_set_status_and_memory_id() {
        with_embedded_pg(|store| async move {
            let s: Arc<dyn SelfLearningStore> = store;
            let item = make_item("mark", "billing", "pricing", "billing is monthly");
            s.upsert_item(&item).unwrap();

            let mark = ReviewMark {
                reviewed_by: "boss".into(),
                note: Some("wrong".into()),
            };
            assert!(s
                .set_status(item.id, ItemStatus::Rejected, Some(&mark))
                .unwrap());
            let got = s.get_item(item.id).unwrap().unwrap();
            assert_eq!(got.status, ItemStatus::Rejected);
            assert_eq!(got.reviewed_by.as_deref(), Some("boss"));
            assert!(got.reviewed_at.is_some());
            assert!(!s
                .set_status(Uuid::new_v4(), ItemStatus::Approved, None)
                .unwrap());

            s.set_memory_id(item.id, None).unwrap();
            assert!(s.get_item(item.id).unwrap().unwrap().memory_id.is_none());
            assert!(s.list_all_memory_ids().unwrap().is_empty());
            s.set_memory_id(item.id, Some("new-mem")).unwrap();
            assert_eq!(
                s.list_all_memory_ids().unwrap(),
                vec!["new-mem".to_string()]
            );
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn parity_delete_and_due_for_reverify() {
        with_embedded_pg(|store| async move {
            let s: Arc<dyn SelfLearningStore> = store;
            let now = now_micros();

            let mut at = make_item("mark", "billing", "pricing", "at boundary");
            at.expires_at = Some(now);
            let mut future = make_item("mark", "billing", "pricing", "future");
            future.expires_at = Some(now + Duration::days(1));
            let mut rejected = make_item("mark", "billing", "pricing", "rejected");
            rejected.status = ItemStatus::Rejected;
            rejected.expires_at = Some(now - Duration::days(1));
            for it in [&at, &future, &rejected] {
                s.upsert_item(it).unwrap();
            }

            let due = s.due_for_reverify("mark", now).unwrap();
            let claims: Vec<&str> = due.iter().map(|i| i.claim.as_str()).collect();
            assert_eq!(claims, vec!["at boundary"]);

            assert!(s.delete_item(at.id).unwrap());
            assert!(s.get_item(at.id).unwrap().is_none());
            assert!(!s.delete_item(at.id).unwrap());
        })
        .await;
    }

    fn make_node(agent: &str, plan: &str, parent: Option<Uuid>, title: &str) -> DeepNode {
        let now = now_micros();
        DeepNode {
            id: Uuid::new_v4(),
            agent_id: agent.into(),
            plan_id: plan.into(),
            parent_id: parent,
            depth: parent.map(|_| 1).unwrap_or(0),
            title: title.into(),
            path_hash: node_path_hash(agent, plan, parent, title),
            body: format!("synthesized body for {title}"),
            sources: vec![ItemSource {
                kind: SourceKind::Codebase,
                skill: None,
                reference: "src/lib.rs#L1".into(),
                excerpt: Some("fn main".into()),
            }],
            coverage: vec![
                SurfaceItem {
                    reference: "symbol::a".into(),
                    covered: true,
                },
                SurfaceItem {
                    reference: "symbol::b".into(),
                    covered: false,
                },
            ],
            memory_id: Some(Uuid::new_v4().to_string()),
            status: NodeStatus::Modelled,
            confidence: 0.7,
            created_at: now,
            created_by: "identity-1".into(),
            verified_at: now,
            expires_at: Some(now + Duration::days(30)),
            reviewed_at: None,
            reviewed_by: None,
            review_note: None,
            superseded_by: None,
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn parity_node_upsert_get_and_find_by_path() {
        with_embedded_pg(|store| async move {
            let s: Arc<dyn DeepExpertiseStore> = store;
            let node = make_node("mark", "discord", None, "Discord gateway");
            s.upsert_node(&node).unwrap();
            let got = s.get_node(node.id).unwrap().unwrap();
            assert_eq!(got, node);
            assert!(s.get_node(Uuid::new_v4()).unwrap().is_none());

            assert_eq!(
                s.find_by_path("mark", &node.path_hash).unwrap(),
                Some(node.clone())
            );
            assert!(s.find_by_path("alice", &node.path_hash).unwrap().is_none());
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn parity_node_upsert_updates_in_place() {
        with_embedded_pg(|store| async move {
            let s: Arc<dyn DeepExpertiseStore> = store;
            let mut node = make_node("mark", "discord", None, "Discord gateway");
            s.upsert_node(&node).unwrap();
            node.confidence = 0.95;
            node.status = NodeStatus::Verified;
            node.coverage[1].covered = true;
            s.upsert_node(&node).unwrap();
            assert_eq!(s.get_node(node.id).unwrap().unwrap(), node);
            assert_eq!(
                s.list_nodes("mark", &NodeQuery::default()).unwrap().len(),
                1
            );
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn parity_list_nodes_filters() {
        with_embedded_pg(|store| async move {
            let s: Arc<dyn DeepExpertiseStore> = store;
            let mut root = make_node("mark", "discord", None, "Discord gateway");
            root.status = NodeStatus::Approved;
            s.upsert_node(&root).unwrap();
            let child_a = make_node("mark", "discord", Some(root.id), "Heartbeat");
            let child_b = make_node("mark", "discord", Some(root.id), "Reconnect");
            let other_plan = make_node("mark", "slack", None, "Slack events");
            s.upsert_node(&child_a).unwrap();
            s.upsert_node(&child_b).unwrap();
            s.upsert_node(&other_plan).unwrap();

            assert_eq!(
                s.list_nodes("mark", &NodeQuery::default()).unwrap().len(),
                4
            );
            assert_eq!(
                s.list_nodes(
                    "mark",
                    &NodeQuery {
                        plan_id: Some("discord".into()),
                        ..Default::default()
                    }
                )
                .unwrap()
                .len(),
                3
            );
            assert_eq!(
                s.list_nodes(
                    "mark",
                    &NodeQuery {
                        parent: NodeParent::Roots,
                        ..Default::default()
                    }
                )
                .unwrap()
                .len(),
                2
            );
            let children = s
                .list_nodes(
                    "mark",
                    &NodeQuery {
                        parent: NodeParent::Of(root.id),
                        ..Default::default()
                    },
                )
                .unwrap();
            assert_eq!(children.len(), 2);
            assert!(children.iter().all(|n| n.parent_id == Some(root.id)));
            let approved = s
                .list_nodes(
                    "mark",
                    &NodeQuery {
                        status: Some(NodeStatus::Approved),
                        ..Default::default()
                    },
                )
                .unwrap();
            assert_eq!(approved.len(), 1);
            assert_eq!(approved[0].id, root.id);
            assert_eq!(
                s.list_nodes(
                    "mark",
                    &NodeQuery {
                        limit: Some(1),
                        ..Default::default()
                    }
                )
                .unwrap()
                .len(),
                1
            );
            assert!(s
                .list_nodes("alice", &NodeQuery::default())
                .unwrap()
                .is_empty());
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn parity_node_status_memory_and_by_memory_ids() {
        with_embedded_pg(|store| async move {
            let s: Arc<dyn DeepExpertiseStore> = store;
            let a = make_node("mark", "discord", None, "a");
            let b = make_node("mark", "discord", None, "b");
            s.upsert_node(&a).unwrap();
            s.upsert_node(&b).unwrap();

            let mark = ReviewMark {
                reviewed_by: "boss".into(),
                note: Some("looks right".into()),
            };
            assert!(s
                .set_status(a.id, NodeStatus::Approved, Some(&mark))
                .unwrap());
            let got = s.get_node(a.id).unwrap().unwrap();
            assert_eq!(got.status, NodeStatus::Approved);
            assert_eq!(got.reviewed_by.as_deref(), Some("boss"));
            assert!(got.reviewed_at.is_some());
            assert!(!s
                .set_status(Uuid::new_v4(), NodeStatus::Approved, None)
                .unwrap());

            let ids = vec![
                a.memory_id.clone().unwrap(),
                b.memory_id.clone().unwrap(),
                "ghost".into(),
            ];
            let mut by_mem = s.nodes_by_memory_ids(&ids).unwrap();
            by_mem.sort_by(|x, y| x.title.cmp(&y.title));
            assert_eq!(by_mem.len(), 2);
            assert_eq!(by_mem[0].id, a.id);
            assert!(s.nodes_by_memory_ids(&[]).unwrap().is_empty());

            s.set_memory_id(a.id, None).unwrap();
            assert!(s.get_node(a.id).unwrap().unwrap().memory_id.is_none());
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn parity_delete_subtree_and_due_for_reverify() {
        with_embedded_pg(|store| async move {
            let s: Arc<dyn DeepExpertiseStore> = store;
            let now = now_micros();

            let root = make_node("mark", "discord", None, "Discord gateway");
            let child = make_node("mark", "discord", Some(root.id), "Heartbeat");
            let grandchild = make_node("mark", "discord", Some(child.id), "Ack timing");
            let sibling = make_node("mark", "discord", None, "Slack events");
            for n in [&root, &child, &grandchild, &sibling] {
                s.upsert_node(n).unwrap();
            }
            assert_eq!(s.delete_subtree(root.id).unwrap(), 3);
            assert!(s.get_node(root.id).unwrap().is_none());
            assert!(s.get_node(grandchild.id).unwrap().is_none());
            assert!(s.get_node(sibling.id).unwrap().is_some());
            assert_eq!(s.delete_subtree(root.id).unwrap(), 0);

            let mut at = make_node("mark", "freshness", None, "at boundary");
            at.expires_at = Some(now);
            let mut future = make_node("mark", "freshness", None, "future");
            future.expires_at = Some(now + Duration::days(1));
            let mut seeded = make_node("mark", "freshness", None, "seeded");
            seeded.status = NodeStatus::Seeded;
            seeded.expires_at = Some(now - Duration::days(1));
            for n in [&at, &future, &seeded] {
                s.upsert_node(n).unwrap();
            }
            let due = s.due_for_reverify("mark", now).unwrap();
            let titles: Vec<&str> = due.iter().map(|n| n.title.as_str()).collect();
            assert_eq!(titles, vec!["at boundary"]);
        })
        .await;
    }
}
