//! One-shot SQLite → Postgres migration for the self-learning store.
//!
//! Reads the `self_learning_items` and `deep_nodes` tables from a SQLite
//! self-learning DB file and writes them into a Postgres backend (after
//! running the refinery migrations against the destination). Driven by
//! the `workforce migrate-self-learning` CLI subcommand.
//!
//! Idempotent — every insert uses `ON CONFLICT (id) DO NOTHING`, so
//! re-running on a partially-migrated destination is safe.

use std::path::Path;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use eyre::{Context, Result};
use rusqlite::{Connection, OpenFlags};
use uuid::Uuid;
use workforce_postgres::{PostgresConfig, PostgresPool};

use crate::postgres::PostgresSelfLearningStore;

#[derive(Debug, Default, Clone)]
pub struct SelfLearningMigrationReport {
    pub items: usize,
    pub nodes: usize,
}

impl SelfLearningMigrationReport {
    pub fn total(&self) -> usize {
        self.items + self.nodes
    }
}

/// Migrate a SQLite self-learning database into a Postgres backend.
///
/// Builds a fresh self-learning pool from `pg_config` (with `search_path`
/// pinned to the self_learning schema), opens the SQLite file read-only,
/// runs the refinery migrations against the destination, then copies
/// every row.
pub async fn migrate_sqlite_to_postgres(
    sqlite_path: &Path,
    pg_config: &PostgresConfig,
) -> Result<SelfLearningMigrationReport> {
    let pool = PostgresSelfLearningStore::build_pool(pg_config)
        .context("building self-learning pool for migration destination")?;
    migrate_sqlite_to_postgres_with_pool(sqlite_path, pool).await
}

/// Variant taking a pre-built pool. Pool MUST have its `search_path`
/// pinned to the self_learning schema (use
/// [`PostgresSelfLearningStore::build_pool`]).
pub async fn migrate_sqlite_to_postgres_with_pool(
    sqlite_path: &Path,
    pool: PostgresPool,
) -> Result<SelfLearningMigrationReport> {
    // Run destination migrations first.
    let _store: Arc<PostgresSelfLearningStore> = PostgresSelfLearningStore::with_pool(pool.clone())
        .await
        .context("preparing Postgres self-learning destination")?;

    let sqlite = Connection::open_with_flags(sqlite_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("opening SQLite source at {}", sqlite_path.display()))?;

    let items = read_items(&sqlite)?;
    let nodes = read_nodes(&sqlite)?;
    drop(sqlite);

    let item_count = write_items(&pool, items).await?;
    let node_count = write_nodes(&pool, nodes).await?;

    Ok(SelfLearningMigrationReport {
        items: item_count,
        nodes: node_count,
    })
}

fn parse_ts(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s)
        .map(|d| d.with_timezone(&Utc))
        .unwrap_or_else(|_| Utc::now())
}

fn parse_opt_ts(s: Option<String>) -> Option<DateTime<Utc>> {
    s.as_deref().map(parse_ts)
}

fn parse_opt_uuid(s: Option<String>) -> Option<Uuid> {
    s.and_then(|s| Uuid::parse_str(&s).ok())
}

struct ItemMigRow {
    id: Uuid,
    agent_id: String,
    plan_id: String,
    topic: String,
    claim: String,
    content_hash: String,
    memory_id: Option<String>,
    status: String,
    confidence: f32,
    high_stakes: bool,
    source_kind: String,
    source_skill: Option<String>,
    source_reference: String,
    source_excerpt: Option<String>,
    created_at: DateTime<Utc>,
    created_by: String,
    verified_at: DateTime<Utc>,
    expires_at: Option<DateTime<Utc>>,
    reviewed_at: Option<DateTime<Utc>>,
    reviewed_by: Option<String>,
    review_note: Option<String>,
    superseded_by: Option<Uuid>,
}

fn read_items(sqlite: &Connection) -> Result<Vec<ItemMigRow>> {
    let mut stmt = sqlite.prepare(
        "SELECT id, agent_id, plan_id, topic, claim, content_hash, memory_id, status,
                confidence, high_stakes, source_kind, source_skill, source_reference,
                source_excerpt, created_at, created_by, verified_at, expires_at,
                reviewed_at, reviewed_by, review_note, superseded_by
         FROM self_learning_items",
    )?;
    let rows = stmt
        .query_map([], |r| {
            let id_str: String = r.get("id")?;
            Ok(ItemMigRow {
                id: Uuid::parse_str(&id_str).unwrap_or_else(|_| Uuid::new_v4()),
                agent_id: r.get("agent_id")?,
                plan_id: r.get("plan_id")?,
                topic: r.get("topic")?,
                claim: r.get("claim")?,
                content_hash: r.get("content_hash")?,
                memory_id: r.get("memory_id")?,
                status: r.get("status")?,
                confidence: r.get("confidence")?,
                high_stakes: r.get("high_stakes")?,
                source_kind: r.get("source_kind")?,
                source_skill: r.get("source_skill")?,
                source_reference: r.get("source_reference")?,
                source_excerpt: r.get("source_excerpt")?,
                created_at: parse_ts(&r.get::<_, String>("created_at")?),
                created_by: r.get("created_by")?,
                verified_at: parse_ts(&r.get::<_, String>("verified_at")?),
                expires_at: parse_opt_ts(r.get("expires_at")?),
                reviewed_at: parse_opt_ts(r.get("reviewed_at")?),
                reviewed_by: r.get("reviewed_by")?,
                review_note: r.get("review_note")?,
                superseded_by: parse_opt_uuid(r.get("superseded_by")?),
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

async fn write_items(pool: &PostgresPool, rows: Vec<ItemMigRow>) -> Result<usize> {
    if rows.is_empty() {
        return Ok(0);
    }
    let conn = pool.get().await?;
    for r in &rows {
        conn.execute(
            "INSERT INTO self_learning_items
             (id, agent_id, plan_id, topic, claim, content_hash, memory_id, status,
              confidence, high_stakes, source_kind, source_skill, source_reference,
              source_excerpt, created_at, created_by, verified_at, expires_at,
              reviewed_at, reviewed_by, review_note, superseded_by)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14,
                     $15, $16, $17, $18, $19, $20, $21, $22)
             ON CONFLICT (id) DO NOTHING",
            &[
                &r.id,
                &r.agent_id,
                &r.plan_id,
                &r.topic,
                &r.claim,
                &r.content_hash,
                &r.memory_id,
                &r.status,
                &r.confidence,
                &r.high_stakes,
                &r.source_kind,
                &r.source_skill,
                &r.source_reference,
                &r.source_excerpt,
                &r.created_at,
                &r.created_by,
                &r.verified_at,
                &r.expires_at,
                &r.reviewed_at,
                &r.reviewed_by,
                &r.review_note,
                &r.superseded_by,
            ],
        )
        .await
        .context("insert self_learning_items row")?;
    }
    Ok(rows.len())
}

struct NodeMigRow {
    id: Uuid,
    agent_id: String,
    plan_id: String,
    parent_id: Option<Uuid>,
    depth: i32,
    title: String,
    path_hash: String,
    body: String,
    sources: String,
    coverage: String,
    memory_id: Option<String>,
    status: String,
    confidence: f32,
    created_at: DateTime<Utc>,
    created_by: String,
    verified_at: DateTime<Utc>,
    expires_at: Option<DateTime<Utc>>,
    reviewed_at: Option<DateTime<Utc>>,
    reviewed_by: Option<String>,
    review_note: Option<String>,
    superseded_by: Option<Uuid>,
}

/// A source not yet repaired (see [`crate::sqlite::DEMOTE_UNGROUNDED_VERIFIED`])
/// is opened read-only, so its rows are repaired on the way across instead.
fn read_nodes(sqlite: &Connection) -> Result<Vec<NodeMigRow>> {
    let version: i64 = sqlite.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    let repair = |status: String| match status.as_str() {
        "verified" if version < crate::sqlite::REPAIRED_VERSION => "modelled".to_string(),
        _ => status,
    };
    let mut stmt = sqlite.prepare(
        "SELECT id, agent_id, plan_id, parent_id, depth, title, path_hash, body, sources,
                coverage, memory_id, status, confidence, created_at, created_by, verified_at,
                expires_at, reviewed_at, reviewed_by, review_note, superseded_by
         FROM deep_nodes",
    )?;
    let rows = stmt
        .query_map([], |r| {
            let id_str: String = r.get("id")?;
            Ok(NodeMigRow {
                id: Uuid::parse_str(&id_str).unwrap_or_else(|_| Uuid::new_v4()),
                agent_id: r.get("agent_id")?,
                plan_id: r.get("plan_id")?,
                parent_id: parse_opt_uuid(r.get("parent_id")?),
                depth: r.get("depth")?,
                title: r.get("title")?,
                path_hash: r.get("path_hash")?,
                body: r.get("body")?,
                sources: r.get("sources")?,
                coverage: r.get("coverage")?,
                memory_id: r.get("memory_id")?,
                status: repair(r.get("status")?),
                confidence: r.get("confidence")?,
                created_at: parse_ts(&r.get::<_, String>("created_at")?),
                created_by: r.get("created_by")?,
                verified_at: parse_ts(&r.get::<_, String>("verified_at")?),
                expires_at: parse_opt_ts(r.get("expires_at")?),
                reviewed_at: parse_opt_ts(r.get("reviewed_at")?),
                reviewed_by: r.get("reviewed_by")?,
                review_note: r.get("review_note")?,
                superseded_by: parse_opt_uuid(r.get("superseded_by")?),
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

async fn write_nodes(pool: &PostgresPool, rows: Vec<NodeMigRow>) -> Result<usize> {
    if rows.is_empty() {
        return Ok(0);
    }
    let conn = pool.get().await?;
    for r in &rows {
        conn.execute(
            "INSERT INTO deep_nodes
             (id, agent_id, plan_id, parent_id, depth, title, path_hash, body, sources,
              coverage, memory_id, status, confidence, created_at, created_by, verified_at,
              expires_at, reviewed_at, reviewed_by, review_note, superseded_by)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16,
                     $17, $18, $19, $20, $21)
             ON CONFLICT (id) DO NOTHING",
            &[
                &r.id,
                &r.agent_id,
                &r.plan_id,
                &r.parent_id,
                &r.depth,
                &r.title,
                &r.path_hash,
                &r.body,
                &r.sources,
                &r.coverage,
                &r.memory_id,
                &r.status,
                &r.confidence,
                &r.created_at,
                &r.created_by,
                &r.verified_at,
                &r.expires_at,
                &r.reviewed_at,
                &r.reviewed_by,
                &r.review_note,
                &r.superseded_by,
            ],
        )
        .await
        .context("insert deep_nodes row")?;
    }
    Ok(rows.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{
        content_hash, node_path_hash, DeepNode, ItemSource, ItemStatus, KnowledgeItem, NodeStatus,
        SourceKind, SurfaceItem,
    };
    use crate::sqlite::SqliteSelfLearningStore;
    use crate::store::{DeepExpertiseStore, SelfLearningStore};
    use chrono::Duration;
    use workforce_postgres::test_support::{now_micros, with_test_pool};

    #[tokio::test(flavor = "multi_thread")]
    async fn migrates_self_learning_round_trip() {
        with_test_pool(PostgresSelfLearningStore::build_pool, |pool| async move {
            let tmp = tempfile::NamedTempFile::new().unwrap();
            let sqlite = SqliteSelfLearningStore::new(tmp.path()).unwrap();

            let now = now_micros();
            let item = KnowledgeItem {
                id: Uuid::new_v4(),
                agent_id: "mark".into(),
                plan_id: "billing".into(),
                topic: "pricing".into(),
                claim: "billing is monthly".into(),
                content_hash: content_hash("mark", "billing", "billing is monthly"),
                memory_id: Some(Uuid::new_v4().to_string()),
                status: ItemStatus::Approved,
                confidence: 0.8,
                high_stakes: true,
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
                superseded_by: Some(Uuid::new_v4()),
            };
            SelfLearningStore::upsert_item(&sqlite, &item).unwrap();

            let node = DeepNode {
                id: Uuid::new_v4(),
                agent_id: "mark".into(),
                plan_id: "discord".into(),
                parent_id: None,
                depth: 0,
                title: "Discord gateway".into(),
                path_hash: node_path_hash("mark", "discord", None, "Discord gateway"),
                body: "synthesized body".into(),
                sources: vec![ItemSource {
                    kind: SourceKind::Codebase,
                    skill: None,
                    reference: "src/lib.rs#L1".into(),
                    excerpt: Some("fn main".into()),
                }],
                coverage: vec![SurfaceItem {
                    reference: "symbol::a".into(),
                    covered: true,
                }],
                memory_id: Some(Uuid::new_v4().to_string()),
                status: NodeStatus::Modelled,
                confidence: 0.7,
                created_at: now,
                created_by: "identity-1".into(),
                verified_at: now,
                expires_at: None,
                reviewed_at: None,
                reviewed_by: None,
                review_note: None,
                superseded_by: None,
            };
            DeepExpertiseStore::upsert_node(&sqlite, &node).unwrap();
            // Checkpoint WAL into the main file before the read-only reopen.
            drop(sqlite);

            let report = migrate_sqlite_to_postgres_with_pool(tmp.path(), pool.clone())
                .await
                .expect("migration succeeds");
            assert_eq!(report.items, 1);
            assert_eq!(report.nodes, 1);

            let dest = PostgresSelfLearningStore::with_pool(pool.clone())
                .await
                .unwrap();
            let item_store: Arc<dyn SelfLearningStore> = dest.clone();
            let node_store: Arc<dyn DeepExpertiseStore> = dest;

            let fetched_item = item_store.get_item(item.id).unwrap().expect("item present");
            assert_eq!(fetched_item, item);
            let fetched_node = node_store.get_node(node.id).unwrap().expect("node present");
            assert_eq!(fetched_node, node);

            // Idempotency: re-running on the same destination is a no-op,
            // not a duplicate-key error, and still reports the source counts.
            let report2 = migrate_sqlite_to_postgres_with_pool(tmp.path(), pool)
                .await
                .expect("idempotent");
            assert_eq!(report2.items, 1);
            assert_eq!(report2.nodes, 1);
        })
        .await;
    }

    /// A source the repair never ran on is read read-only, so its verified
    /// nodes are demoted on the way across rather than re-imported as earned.
    #[test]
    fn copying_an_unrepaired_source_demotes_its_verified_nodes() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let now = Utc::now();
        let node = |title: &str, status: NodeStatus| DeepNode {
            id: Uuid::new_v4(),
            agent_id: "mark".into(),
            plan_id: "discord".into(),
            parent_id: None,
            depth: 0,
            title: title.into(),
            path_hash: node_path_hash("mark", "discord", None, title),
            body: "body".into(),
            sources: vec![],
            coverage: vec![],
            memory_id: None,
            status,
            confidence: 0.5,
            created_at: now,
            created_by: "mark".into(),
            verified_at: now,
            expires_at: None,
            reviewed_at: None,
            reviewed_by: None,
            review_note: None,
            superseded_by: None,
        };
        let statuses = |unrepaired: bool| {
            {
                let s = SqliteSelfLearningStore::new(tmp.path()).unwrap();
                s.upsert_node(&node("v", NodeStatus::Verified)).unwrap();
                s.upsert_node(&node("a", NodeStatus::Approved)).unwrap();
            }
            let conn = Connection::open(tmp.path()).unwrap();
            if unrepaired {
                conn.pragma_update(None, "user_version", 0_i64).unwrap();
            }
            let mut statuses: Vec<String> = read_nodes(&conn)
                .unwrap()
                .into_iter()
                .map(|n| n.status)
                .collect();
            conn.execute("DELETE FROM deep_nodes", []).unwrap();
            statuses.sort();
            statuses
        };
        assert_eq!(statuses(true), ["approved", "modelled"]);
        assert_eq!(statuses(false), ["approved", "verified"]);
    }
}
