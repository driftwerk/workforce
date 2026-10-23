use std::path::Path;
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use eyre::{eyre, Result, WrapErr};
use rusqlite::{params, params_from_iter, Connection, OptionalExtension};
use tracing::info;
use uuid::Uuid;

use crate::model::{
    DeepNode, ItemQuery, ItemSource, ItemStatus, KnowledgeItem, NodeParent, NodeQuery, NodeStatus,
    ReviewMark, SurfaceItem,
};
use crate::store::{DeepExpertiseStore, SelfLearningStore};

/// `PRAGMA user_version` of a database [`SqliteSelfLearningStore::repair`]
/// has brought up to date.
pub(crate) const REPAIRED_VERSION: i64 = 1;

/// Before claim grounding failed closed, a pass that checked nothing (or was
/// cut short) read as clean and promoted a deep-expertise node to `verified`
/// for good. Nothing stored tells those passes from real ones, so every
/// machine-verified node goes back to `modelled`, where the deepening loop
/// picks it up and the quality gate must earn `verified` again. `approved` is
/// a person's call and is left alone. Kept in step with the Postgres
/// migration `V003__demote_ungrounded_verified_nodes.sql`.
pub(crate) const DEMOTE_UNGROUNDED_VERIFIED: &str =
    "UPDATE deep_nodes SET status = 'modelled' WHERE status = 'verified'";

pub struct SqliteSelfLearningStore {
    conn: Mutex<Connection>,
}

impl SqliteSelfLearningStore {
    pub fn new(path: impl AsRef<Path>) -> Result<Self> {
        let conn = Connection::open(path.as_ref())
            .wrap_err_with(|| format!("open self-learning DB at {:?}", path.as_ref()))?;
        // Multiple AgentService instances + the HTTP handler thread can all
        // hold their own connection to this same file. WAL lets readers and a
        // writer coexist without "database is locked", and busy_timeout makes
        // the contention case wait instead of erroring on the first conflict.
        conn.pragma_update(None, "journal_mode", "WAL")
            .wrap_err("enable WAL")?;
        conn.pragma_update(None, "busy_timeout", 5_000_i64)
            .wrap_err("set busy_timeout")?;
        // pragma_update is fire-and-forget. Some filesystems (NFS, certain
        // container mounts) silently refuse WAL and leave the DB in DELETE
        // mode. Verify so we don't ship a phantom concurrency story.
        let mode: String =
            conn.query_row("PRAGMA journal_mode", [], |row| row.get::<_, String>(0))?;
        if !mode.eq_ignore_ascii_case("wal") {
            tracing::warn!(
                path = %path.as_ref().display(),
                journal_mode = %mode,
                "Self-learning store fell back to non-WAL journal mode; \
                 concurrent writers may see `database is locked` errors",
            );
        }
        Self::init_schema(&conn)?;
        info!(path = %path.as_ref().display(), "Self-learning store opened");
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    pub fn in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        Self::init_schema(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn init_schema(conn: &Connection) -> Result<()> {
        conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS self_learning_items (
                id              TEXT PRIMARY KEY,
                agent_id        TEXT NOT NULL,
                plan_id         TEXT NOT NULL,
                topic           TEXT NOT NULL,
                claim           TEXT NOT NULL,
                content_hash    TEXT NOT NULL,
                memory_id       TEXT,
                status          TEXT NOT NULL DEFAULT 'provisional',
                confidence      REAL NOT NULL DEFAULT 0,
                high_stakes     INTEGER NOT NULL DEFAULT 0,
                source_kind     TEXT NOT NULL,
                source_skill    TEXT,
                source_reference TEXT NOT NULL,
                source_excerpt  TEXT,
                created_at      TEXT NOT NULL,
                created_by      TEXT NOT NULL,
                verified_at     TEXT NOT NULL,
                expires_at      TEXT,
                reviewed_at     TEXT,
                reviewed_by     TEXT,
                review_note     TEXT,
                superseded_by   TEXT,
                UNIQUE (agent_id, content_hash)
            );
            CREATE INDEX IF NOT EXISTS idx_self_learning_items_agent
                ON self_learning_items(agent_id);
            CREATE INDEX IF NOT EXISTS idx_self_learning_items_hash
                ON self_learning_items(content_hash);
            CREATE INDEX IF NOT EXISTS idx_self_learning_items_status
                ON self_learning_items(status);
            CREATE INDEX IF NOT EXISTS idx_self_learning_items_memory
                ON self_learning_items(memory_id);
            CREATE INDEX IF NOT EXISTS idx_self_learning_items_expires
                ON self_learning_items(expires_at);

            CREATE TABLE IF NOT EXISTS deep_nodes (
                id              TEXT PRIMARY KEY,
                agent_id        TEXT NOT NULL,
                plan_id         TEXT NOT NULL,
                parent_id       TEXT,
                depth           INTEGER NOT NULL DEFAULT 0,
                title           TEXT NOT NULL,
                path_hash       TEXT NOT NULL,
                body            TEXT NOT NULL,
                sources         TEXT NOT NULL DEFAULT '[]',
                coverage        TEXT NOT NULL DEFAULT '[]',
                memory_id       TEXT,
                status          TEXT NOT NULL DEFAULT 'seeded',
                confidence      REAL NOT NULL DEFAULT 0,
                created_at      TEXT NOT NULL,
                created_by      TEXT NOT NULL,
                verified_at     TEXT NOT NULL,
                expires_at      TEXT,
                reviewed_at     TEXT,
                reviewed_by     TEXT,
                review_note     TEXT,
                superseded_by   TEXT
            );
            CREATE INDEX IF NOT EXISTS idx_deep_nodes_path
                ON deep_nodes(agent_id, path_hash);
            CREATE INDEX IF NOT EXISTS idx_deep_nodes_parent
                ON deep_nodes(agent_id, parent_id);
            ",
        )
        .wrap_err("init self-learning schema")?;
        Self::repair(conn)
    }

    /// One-off data repairs, each run once per database under
    /// `PRAGMA user_version`. Several connections may open the same file at
    /// once, so the check and the repair share one immediate transaction.
    fn repair(conn: &Connection) -> Result<()> {
        let tx =
            rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
        let version: i64 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if version < REPAIRED_VERSION {
            let demoted = tx
                .execute(DEMOTE_UNGROUNDED_VERIFIED, [])
                .wrap_err("demote ungrounded verified nodes")?;
            tx.pragma_update(None, "user_version", REPAIRED_VERSION)?;
            if demoted > 0 {
                info!(
                    demoted,
                    "Deep-expertise nodes verified before grounding failed closed are back to \
                     modelled, to be verified again"
                );
            }
        }
        tx.commit().wrap_err("commit self-learning repair")?;
        Ok(())
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Connection>> {
        self.conn
            .lock()
            .map_err(|e| eyre!("self-learning DB poisoned: {e}"))
    }
}

const ITEM_COLUMNS: &str = "id, agent_id, plan_id, topic, claim, content_hash, memory_id, status, \
     confidence, high_stakes, source_kind, source_skill, source_reference, source_excerpt, \
     created_at, created_by, verified_at, expires_at, reviewed_at, reviewed_by, review_note, \
     superseded_by";

fn parse_ts(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or_else(|_| Utc::now())
}

fn parse_opt_ts(s: Option<String>) -> Option<DateTime<Utc>> {
    s.as_deref().map(parse_ts)
}

fn item_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<KnowledgeItem> {
    let mk_err = |e: uuid::Error| -> rusqlite::Error {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
    };
    let id_str: String = row.get("id")?;
    let superseded_by: Option<String> = row.get("superseded_by")?;
    let superseded_by = match superseded_by {
        Some(s) => Some(Uuid::parse_str(&s).map_err(mk_err)?),
        None => None,
    };
    Ok(KnowledgeItem {
        id: Uuid::parse_str(&id_str).map_err(mk_err)?,
        agent_id: row.get("agent_id")?,
        plan_id: row.get("plan_id")?,
        topic: row.get("topic")?,
        claim: row.get("claim")?,
        content_hash: row.get("content_hash")?,
        memory_id: row.get("memory_id")?,
        status: ItemStatus::parse(&row.get::<_, String>("status")?),
        confidence: row.get("confidence")?,
        high_stakes: row.get("high_stakes")?,
        source: ItemSource {
            kind: crate::model::SourceKind::parse(&row.get::<_, String>("source_kind")?),
            skill: row.get("source_skill")?,
            reference: row.get("source_reference")?,
            excerpt: row.get("source_excerpt")?,
        },
        created_at: parse_ts(&row.get::<_, String>("created_at")?),
        created_by: row.get("created_by")?,
        verified_at: parse_ts(&row.get::<_, String>("verified_at")?),
        expires_at: parse_opt_ts(row.get("expires_at")?),
        reviewed_at: parse_opt_ts(row.get("reviewed_at")?),
        reviewed_by: row.get("reviewed_by")?,
        review_note: row.get("review_note")?,
        superseded_by,
    })
}

const NODE_COLUMNS: &str = "id, agent_id, plan_id, parent_id, depth, title, path_hash, body, \
     sources, coverage, memory_id, status, confidence, created_at, created_by, verified_at, \
     expires_at, reviewed_at, reviewed_by, review_note, superseded_by";

fn node_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<DeepNode> {
    let mk_uuid_err = |e: uuid::Error| -> rusqlite::Error {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
    };
    let mk_json_err = |e: serde_json::Error| -> rusqlite::Error {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
    };
    let parse_opt_uuid = |s: Option<String>| -> rusqlite::Result<Option<Uuid>> {
        match s {
            Some(s) => Ok(Some(Uuid::parse_str(&s).map_err(mk_uuid_err)?)),
            None => Ok(None),
        }
    };
    let id_str: String = row.get("id")?;
    let sources: Vec<ItemSource> =
        serde_json::from_str(&row.get::<_, String>("sources")?).map_err(mk_json_err)?;
    let coverage: Vec<SurfaceItem> =
        serde_json::from_str(&row.get::<_, String>("coverage")?).map_err(mk_json_err)?;
    Ok(DeepNode {
        id: Uuid::parse_str(&id_str).map_err(mk_uuid_err)?,
        agent_id: row.get("agent_id")?,
        plan_id: row.get("plan_id")?,
        parent_id: parse_opt_uuid(row.get("parent_id")?)?,
        depth: row.get("depth")?,
        title: row.get("title")?,
        path_hash: row.get("path_hash")?,
        body: row.get("body")?,
        sources,
        coverage,
        memory_id: row.get("memory_id")?,
        status: NodeStatus::parse(&row.get::<_, String>("status")?),
        confidence: row.get("confidence")?,
        created_at: parse_ts(&row.get::<_, String>("created_at")?),
        created_by: row.get("created_by")?,
        verified_at: parse_ts(&row.get::<_, String>("verified_at")?),
        expires_at: parse_opt_ts(row.get("expires_at")?),
        reviewed_at: parse_opt_ts(row.get("reviewed_at")?),
        reviewed_by: row.get("reviewed_by")?,
        review_note: row.get("review_note")?,
        superseded_by: parse_opt_uuid(row.get("superseded_by")?)?,
    })
}

impl SelfLearningStore for SqliteSelfLearningStore {
    fn upsert_item(&self, item: &KnowledgeItem) -> Result<()> {
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO self_learning_items
             (id, agent_id, plan_id, topic, claim, content_hash, memory_id, status,
              confidence, high_stakes, source_kind, source_skill, source_reference,
              source_excerpt, created_at, created_by, verified_at, expires_at,
              reviewed_at, reviewed_by, review_note, superseded_by)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14,
                     ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22)
             ON CONFLICT(id) DO UPDATE SET
                agent_id = excluded.agent_id,
                plan_id = excluded.plan_id,
                topic = excluded.topic,
                claim = excluded.claim,
                content_hash = excluded.content_hash,
                memory_id = excluded.memory_id,
                status = excluded.status,
                confidence = excluded.confidence,
                high_stakes = excluded.high_stakes,
                source_kind = excluded.source_kind,
                source_skill = excluded.source_skill,
                source_reference = excluded.source_reference,
                source_excerpt = excluded.source_excerpt,
                created_at = excluded.created_at,
                created_by = excluded.created_by,
                verified_at = excluded.verified_at,
                expires_at = excluded.expires_at,
                reviewed_at = excluded.reviewed_at,
                reviewed_by = excluded.reviewed_by,
                review_note = excluded.review_note,
                superseded_by = excluded.superseded_by",
            params![
                item.id.to_string(),
                item.agent_id,
                item.plan_id,
                item.topic,
                item.claim,
                item.content_hash,
                item.memory_id,
                item.status.as_str(),
                item.confidence,
                item.high_stakes,
                item.source.kind.as_str(),
                item.source.skill,
                item.source.reference,
                item.source.excerpt,
                item.created_at.to_rfc3339(),
                item.created_by,
                item.verified_at.to_rfc3339(),
                item.expires_at.map(|t| t.to_rfc3339()),
                item.reviewed_at.map(|t| t.to_rfc3339()),
                item.reviewed_by,
                item.review_note,
                item.superseded_by.map(|u| u.to_string()),
            ],
        )
        .wrap_err("upsert item")?;
        Ok(())
    }

    fn get_item(&self, id: Uuid) -> Result<Option<KnowledgeItem>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(&format!(
            "SELECT {ITEM_COLUMNS} FROM self_learning_items WHERE id = ?1"
        ))?;
        stmt.query_row(params![id.to_string()], item_from_row)
            .optional()
            .wrap_err("get item")
    }

    fn find_by_hash(&self, agent_id: &str, content_hash: &str) -> Result<Option<KnowledgeItem>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(&format!(
            "SELECT {ITEM_COLUMNS} FROM self_learning_items
             WHERE agent_id = ?1 AND content_hash = ?2 LIMIT 1"
        ))?;
        stmt.query_row(params![agent_id, content_hash], item_from_row)
            .optional()
            .wrap_err("find item by hash")
    }

    fn list_items(&self, agent_id: &str, filter: &ItemQuery) -> Result<Vec<KnowledgeItem>> {
        let conn = self.lock()?;
        let status = filter.status.map(|s| s.as_str().to_string());
        let limit: i64 = filter.limit.map(|n| n as i64).unwrap_or(-1);
        let mut stmt = conn.prepare(&format!(
            "SELECT {ITEM_COLUMNS} FROM self_learning_items
             WHERE agent_id = ?1
               AND (?2 IS NULL OR plan_id = ?2)
               AND (?3 IS NULL OR topic = ?3)
               AND (?4 IS NULL OR status = ?4)
             ORDER BY created_at DESC, id
             LIMIT ?5"
        ))?;
        let rows = stmt.query_map(
            params![agent_id, filter.plan_id, filter.topic, status, limit],
            item_from_row,
        )?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .wrap_err("list items")
    }

    fn items_by_memory_ids(&self, memory_ids: &[String]) -> Result<Vec<KnowledgeItem>> {
        if memory_ids.is_empty() {
            return Ok(Vec::new());
        }
        let conn = self.lock()?;
        let placeholders = (0..memory_ids.len())
            .map(|i| format!("?{}", i + 1))
            .collect::<Vec<_>>()
            .join(", ");
        let mut stmt = conn.prepare(&format!(
            "SELECT {ITEM_COLUMNS} FROM self_learning_items
             WHERE memory_id IN ({placeholders})"
        ))?;
        let rows = stmt.query_map(params_from_iter(memory_ids.iter()), item_from_row)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .wrap_err("list items by memory ids")
    }

    fn set_status(
        &self,
        id: Uuid,
        status: ItemStatus,
        review: Option<&ReviewMark>,
    ) -> Result<bool> {
        let conn = self.lock()?;
        let n = match review {
            Some(mark) => conn.execute(
                "UPDATE self_learning_items
                 SET status = ?1, reviewed_at = ?2, reviewed_by = ?3, review_note = ?4
                 WHERE id = ?5",
                params![
                    status.as_str(),
                    Utc::now().to_rfc3339(),
                    mark.reviewed_by,
                    mark.note,
                    id.to_string(),
                ],
            )?,
            None => conn.execute(
                "UPDATE self_learning_items SET status = ?1 WHERE id = ?2",
                params![status.as_str(), id.to_string()],
            )?,
        };
        Ok(n > 0)
    }

    fn set_memory_id(&self, id: Uuid, memory_id: Option<&str>) -> Result<()> {
        let conn = self.lock()?;
        conn.execute(
            "UPDATE self_learning_items SET memory_id = ?1 WHERE id = ?2",
            params![memory_id, id.to_string()],
        )?;
        Ok(())
    }

    fn list_all_memory_ids(&self) -> Result<Vec<String>> {
        let conn = self.lock()?;
        let mut stmt =
            conn.prepare("SELECT memory_id FROM self_learning_items WHERE memory_id IS NOT NULL")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .wrap_err("list all memory ids")
    }

    fn delete_item(&self, id: Uuid) -> Result<bool> {
        let conn = self.lock()?;
        let n = conn.execute(
            "DELETE FROM self_learning_items WHERE id = ?1",
            params![id.to_string()],
        )?;
        Ok(n > 0)
    }

    fn due_for_reverify(&self, agent_id: &str, now: DateTime<Utc>) -> Result<Vec<KnowledgeItem>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(&format!(
            "SELECT {ITEM_COLUMNS} FROM self_learning_items
             WHERE agent_id = ?1
               AND expires_at IS NOT NULL
               AND expires_at <= ?2
               AND status IN ('approved', 'provisional')
             ORDER BY expires_at, id"
        ))?;
        let rows = stmt.query_map(params![agent_id, now.to_rfc3339()], item_from_row)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .wrap_err("due for reverify")
    }
}

impl DeepExpertiseStore for SqliteSelfLearningStore {
    fn upsert_node(&self, node: &DeepNode) -> Result<()> {
        let conn = self.lock()?;
        let sources = serde_json::to_string(&node.sources).wrap_err("serialize node sources")?;
        let coverage = serde_json::to_string(&node.coverage).wrap_err("serialize node coverage")?;
        conn.execute(
            "INSERT INTO deep_nodes
             (id, agent_id, plan_id, parent_id, depth, title, path_hash, body, sources,
              coverage, memory_id, status, confidence, created_at, created_by, verified_at,
              expires_at, reviewed_at, reviewed_by, review_note, superseded_by)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16,
                     ?17, ?18, ?19, ?20, ?21)
             ON CONFLICT(id) DO UPDATE SET
                agent_id = excluded.agent_id,
                plan_id = excluded.plan_id,
                parent_id = excluded.parent_id,
                depth = excluded.depth,
                title = excluded.title,
                path_hash = excluded.path_hash,
                body = excluded.body,
                sources = excluded.sources,
                coverage = excluded.coverage,
                memory_id = excluded.memory_id,
                status = excluded.status,
                confidence = excluded.confidence,
                created_at = excluded.created_at,
                created_by = excluded.created_by,
                verified_at = excluded.verified_at,
                expires_at = excluded.expires_at,
                reviewed_at = excluded.reviewed_at,
                reviewed_by = excluded.reviewed_by,
                review_note = excluded.review_note,
                superseded_by = excluded.superseded_by",
            params![
                node.id.to_string(),
                node.agent_id,
                node.plan_id,
                node.parent_id.map(|u| u.to_string()),
                node.depth,
                node.title,
                node.path_hash,
                node.body,
                sources,
                coverage,
                node.memory_id,
                node.status.as_str(),
                node.confidence,
                node.created_at.to_rfc3339(),
                node.created_by,
                node.verified_at.to_rfc3339(),
                node.expires_at.map(|t| t.to_rfc3339()),
                node.reviewed_at.map(|t| t.to_rfc3339()),
                node.reviewed_by,
                node.review_note,
                node.superseded_by.map(|u| u.to_string()),
            ],
        )
        .wrap_err("upsert node")?;
        Ok(())
    }

    fn get_node(&self, id: Uuid) -> Result<Option<DeepNode>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(&format!(
            "SELECT {NODE_COLUMNS} FROM deep_nodes WHERE id = ?1"
        ))?;
        stmt.query_row(params![id.to_string()], node_from_row)
            .optional()
            .wrap_err("get node")
    }

    fn find_by_path(&self, agent_id: &str, path_hash: &str) -> Result<Option<DeepNode>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(&format!(
            "SELECT {NODE_COLUMNS} FROM deep_nodes
             WHERE agent_id = ?1 AND path_hash = ?2 LIMIT 1"
        ))?;
        stmt.query_row(params![agent_id, path_hash], node_from_row)
            .optional()
            .wrap_err("find node by path")
    }

    fn list_nodes(&self, agent_id: &str, filter: &NodeQuery) -> Result<Vec<DeepNode>> {
        let conn = self.lock()?;
        let status = filter.status.map(|s| s.as_str().to_string());
        let (roots_only, of_parent): (i64, Option<String>) = match filter.parent {
            NodeParent::Any => (0, None),
            NodeParent::Roots => (1, None),
            NodeParent::Of(id) => (0, Some(id.to_string())),
        };
        let limit: i64 = filter.limit.map(|n| n as i64).unwrap_or(-1);
        let mut stmt = conn.prepare(&format!(
            "SELECT {NODE_COLUMNS} FROM deep_nodes
             WHERE agent_id = ?1
               AND (?2 IS NULL OR plan_id = ?2)
               AND (?3 = 0 OR parent_id IS NULL)
               AND (?4 IS NULL OR parent_id = ?4)
               AND (?5 IS NULL OR status = ?5)
             ORDER BY created_at DESC, id
             LIMIT ?6"
        ))?;
        let rows = stmt.query_map(
            params![
                agent_id,
                filter.plan_id,
                roots_only,
                of_parent,
                status,
                limit
            ],
            node_from_row,
        )?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .wrap_err("list nodes")
    }

    fn nodes_by_memory_ids(&self, memory_ids: &[String]) -> Result<Vec<DeepNode>> {
        if memory_ids.is_empty() {
            return Ok(Vec::new());
        }
        let conn = self.lock()?;
        let placeholders = (0..memory_ids.len())
            .map(|i| format!("?{}", i + 1))
            .collect::<Vec<_>>()
            .join(", ");
        let mut stmt = conn.prepare(&format!(
            "SELECT {NODE_COLUMNS} FROM deep_nodes WHERE memory_id IN ({placeholders})"
        ))?;
        let rows = stmt.query_map(params_from_iter(memory_ids.iter()), node_from_row)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .wrap_err("list nodes by memory ids")
    }

    fn set_status(
        &self,
        id: Uuid,
        status: NodeStatus,
        review: Option<&ReviewMark>,
    ) -> Result<bool> {
        let conn = self.lock()?;
        let n = match review {
            Some(mark) => conn.execute(
                "UPDATE deep_nodes
                 SET status = ?1, reviewed_at = ?2, reviewed_by = ?3, review_note = ?4
                 WHERE id = ?5",
                params![
                    status.as_str(),
                    Utc::now().to_rfc3339(),
                    mark.reviewed_by,
                    mark.note,
                    id.to_string(),
                ],
            )?,
            None => conn.execute(
                "UPDATE deep_nodes SET status = ?1 WHERE id = ?2",
                params![status.as_str(), id.to_string()],
            )?,
        };
        Ok(n > 0)
    }

    fn set_memory_id(&self, id: Uuid, memory_id: Option<&str>) -> Result<()> {
        let conn = self.lock()?;
        conn.execute(
            "UPDATE deep_nodes SET memory_id = ?1 WHERE id = ?2",
            params![memory_id, id.to_string()],
        )?;
        Ok(())
    }

    fn list_all_memory_ids(&self) -> Result<Vec<String>> {
        let conn = self.lock()?;
        let mut stmt =
            conn.prepare("SELECT memory_id FROM deep_nodes WHERE memory_id IS NOT NULL")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .wrap_err("list all node memory ids")
    }

    fn delete_subtree(&self, id: Uuid) -> Result<u64> {
        let conn = self.lock()?;
        let n = conn.execute(
            "WITH RECURSIVE subtree(id) AS (
                 SELECT id FROM deep_nodes WHERE id = ?1
                 UNION ALL
                 SELECT n.id FROM deep_nodes n
                 JOIN subtree s ON n.parent_id = s.id
             )
             DELETE FROM deep_nodes WHERE id IN (SELECT id FROM subtree)",
            params![id.to_string()],
        )?;
        Ok(n as u64)
    }

    fn due_for_reverify(&self, agent_id: &str, now: DateTime<Utc>) -> Result<Vec<DeepNode>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(&format!(
            "SELECT {NODE_COLUMNS} FROM deep_nodes
             WHERE agent_id = ?1
               AND expires_at IS NOT NULL
               AND expires_at <= ?2
               AND status IN ('verified', 'approved', 'modelled')
             ORDER BY expires_at, id"
        ))?;
        let rows = stmt.query_map(params![agent_id, now.to_rfc3339()], node_from_row)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .wrap_err("nodes due for reverify")
    }
}

#[cfg(test)]
#[allow(unused_must_use)]
mod tests {
    use super::*;
    use crate::model::{content_hash, node_path_hash, SourceKind};
    use chrono::Duration;

    fn store() -> SqliteSelfLearningStore {
        SqliteSelfLearningStore::in_memory().unwrap()
    }

    fn make_item(agent: &str, plan: &str, topic: &str, claim: &str) -> KnowledgeItem {
        let now = Utc::now();
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

    #[test]
    fn upsert_get_round_trip() {
        let s = store();
        let item = make_item("mark", "billing", "pricing", "billing is monthly");
        s.upsert_item(&item).unwrap();
        let got = s.get_item(item.id).unwrap().unwrap();
        assert_eq!(got, item);
    }

    #[test]
    fn upsert_is_idempotent_by_id() {
        let s = store();
        let mut item = make_item("mark", "billing", "pricing", "billing is monthly");
        s.upsert_item(&item).unwrap();
        item.confidence = 0.5;
        item.status = ItemStatus::Approved;
        s.upsert_item(&item).unwrap();
        let got = s.get_item(item.id).unwrap().unwrap();
        assert_eq!(got.confidence, 0.5);
        assert_eq!(got.status, ItemStatus::Approved);
        // Still exactly one row.
        let all = s.list_items("mark", &ItemQuery::default()).unwrap();
        assert_eq!(all.len(), 1);
    }

    #[test]
    fn find_by_hash_dedups() {
        let s = store();
        let item = make_item("mark", "billing", "pricing", "billing is monthly");
        s.upsert_item(&item).unwrap();
        let hit = s.find_by_hash("mark", &item.content_hash).unwrap();
        assert_eq!(hit, Some(item.clone()));
        // Same claim under a different agent does not collide.
        assert!(s
            .find_by_hash("alice", &item.content_hash)
            .unwrap()
            .is_none());
        assert!(s.find_by_hash("mark", "nope").unwrap().is_none());
    }

    #[test]
    fn unique_agent_hash_enforced() {
        let s = store();
        let a = make_item("mark", "billing", "pricing", "billing is monthly");
        let mut b = make_item("mark", "billing", "pricing", "billing is monthly");
        // Different id, same (agent_id, content_hash) ⇒ UNIQUE violation.
        b.id = Uuid::new_v4();
        s.upsert_item(&a).unwrap();
        assert!(s.upsert_item(&b).is_err());
    }

    #[test]
    fn list_items_filters() {
        let s = store();
        let mut a = make_item("mark", "billing", "pricing", "billing is monthly");
        a.status = ItemStatus::Approved;
        let b = make_item("mark", "billing", "refunds", "refunds are net");
        let c = make_item("mark", "product", "pricing", "tiers exist");
        s.upsert_item(&a).unwrap();
        s.upsert_item(&b).unwrap();
        s.upsert_item(&c).unwrap();

        // No filter ⇒ all three for this agent.
        assert_eq!(
            s.list_items("mark", &ItemQuery::default()).unwrap().len(),
            3
        );

        // Plan filter.
        let by_plan = s
            .list_items(
                "mark",
                &ItemQuery {
                    plan_id: Some("billing".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(by_plan.len(), 2);

        // Topic filter.
        let by_topic = s
            .list_items(
                "mark",
                &ItemQuery {
                    topic: Some("pricing".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(by_topic.len(), 2);

        // Status filter.
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

        // Limit.
        let limited = s
            .list_items(
                "mark",
                &ItemQuery {
                    limit: Some(1),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(limited.len(), 1);

        // Other agent isolated.
        assert!(s
            .list_items("alice", &ItemQuery::default())
            .unwrap()
            .is_empty());
    }

    #[test]
    fn items_by_memory_ids_joins() {
        let s = store();
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

        // Empty slice short-circuits.
        assert!(s.items_by_memory_ids(&[]).unwrap().is_empty());
    }

    #[test]
    fn set_status_records_review_mark() {
        let s = store();
        let item = make_item("mark", "billing", "pricing", "billing is monthly");
        s.upsert_item(&item).unwrap();

        let mark = ReviewMark {
            reviewed_by: "boss".into(),
            note: Some("wrong".into()),
        };
        assert!(
            SelfLearningStore::set_status(&s, item.id, ItemStatus::Rejected, Some(&mark)).unwrap()
        );
        let got = s.get_item(item.id).unwrap().unwrap();
        assert_eq!(got.status, ItemStatus::Rejected);
        assert_eq!(got.reviewed_by.as_deref(), Some("boss"));
        assert_eq!(got.review_note.as_deref(), Some("wrong"));
        assert!(got.reviewed_at.is_some());

        // Without a mark, only status moves; review fields are untouched.
        assert!(SelfLearningStore::set_status(&s, item.id, ItemStatus::Stale, None).unwrap());
        let staled = s.get_item(item.id).unwrap().unwrap();
        assert_eq!(staled.status, ItemStatus::Stale);
        assert_eq!(staled.reviewed_by.as_deref(), Some("boss"));

        // Missing id ⇒ false.
        assert!(
            !SelfLearningStore::set_status(&s, Uuid::new_v4(), ItemStatus::Approved, None).unwrap()
        );
    }

    #[test]
    fn set_memory_id_clears_link() {
        let s = store();
        let item = make_item("mark", "billing", "pricing", "billing is monthly");
        s.upsert_item(&item).unwrap();
        assert!(item.memory_id.is_some());

        SelfLearningStore::set_memory_id(&s, item.id, None).unwrap();
        let cleared = s.get_item(item.id).unwrap().unwrap();
        assert!(cleared.memory_id.is_none());
        // Cleared rows drop out of the still-referenced set.
        assert!(SelfLearningStore::list_all_memory_ids(&s)
            .unwrap()
            .is_empty());

        SelfLearningStore::set_memory_id(&s, item.id, Some("new-mem")).unwrap();
        let relinked = s.get_item(item.id).unwrap().unwrap();
        assert_eq!(relinked.memory_id.as_deref(), Some("new-mem"));
        assert_eq!(
            SelfLearningStore::list_all_memory_ids(&s).unwrap(),
            vec!["new-mem".to_string()]
        );
    }

    #[test]
    fn delete_item_removes_row() {
        let s = store();
        let item = make_item("mark", "billing", "pricing", "billing is monthly");
        s.upsert_item(&item).unwrap();
        assert!(s.delete_item(item.id).unwrap());
        assert!(s.get_item(item.id).unwrap().is_none());
        assert!(!s.delete_item(item.id).unwrap());
    }

    #[test]
    fn due_for_reverify_boundary() {
        let s = store();
        let now = Utc::now();

        // Exactly at expiry, provisional ⇒ due (inclusive <=).
        let mut at = make_item("mark", "billing", "pricing", "at boundary");
        at.expires_at = Some(now);
        // Past expiry, approved ⇒ due.
        let mut past = make_item("mark", "billing", "pricing", "past");
        past.status = ItemStatus::Approved;
        past.expires_at = Some(now - Duration::seconds(1));
        // Future expiry ⇒ not due.
        let mut future = make_item("mark", "billing", "pricing", "future");
        future.expires_at = Some(now + Duration::days(1));
        // Past expiry but rejected ⇒ not surfaceable, not due.
        let mut rejected = make_item("mark", "billing", "pricing", "rejected");
        rejected.status = ItemStatus::Rejected;
        rejected.expires_at = Some(now - Duration::days(1));
        // No expiry ⇒ never due.
        let mut perpetual = make_item("mark", "billing", "pricing", "perpetual");
        perpetual.expires_at = None;

        for it in [&at, &past, &future, &rejected, &perpetual] {
            s.upsert_item(it).unwrap();
        }

        let due = SelfLearningStore::due_for_reverify(&s, "mark", now).unwrap();
        let claims: Vec<&str> = due.iter().map(|i| i.claim.as_str()).collect();
        assert!(claims.contains(&"at boundary"));
        assert!(claims.contains(&"past"));
        assert!(!claims.contains(&"future"));
        assert!(!claims.contains(&"rejected"));
        assert!(!claims.contains(&"perpetual"));
        assert_eq!(due.len(), 2);
    }

    fn make_node(agent: &str, plan: &str, parent: Option<Uuid>, title: &str) -> DeepNode {
        let now = Utc::now();
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

    #[test]
    fn node_upsert_get_round_trip() {
        let s = store();
        let node = make_node("mark", "discord", None, "Discord gateway");
        s.upsert_node(&node).unwrap();
        let got = s.get_node(node.id).unwrap().unwrap();
        assert_eq!(got, node);
        assert!(s.get_node(Uuid::new_v4()).unwrap().is_none());
    }

    #[test]
    fn node_find_by_path_returns_node() {
        let s = store();
        let node = make_node("mark", "discord", None, "Discord gateway");
        s.upsert_node(&node).unwrap();
        assert_eq!(
            s.find_by_path("mark", &node.path_hash).unwrap(),
            Some(node.clone())
        );
        // Same path under a different agent does not collide.
        assert!(s.find_by_path("alice", &node.path_hash).unwrap().is_none());
        assert!(s.find_by_path("mark", "nope").unwrap().is_none());
    }

    #[test]
    fn node_upsert_updates_in_place_by_id() {
        let s = store();
        let mut node = make_node("mark", "discord", None, "Discord gateway");
        s.upsert_node(&node).unwrap();
        node.confidence = 0.95;
        node.status = NodeStatus::Verified;
        node.body = "revised body".into();
        node.coverage[1].covered = true;
        s.upsert_node(&node).unwrap();
        let got = s.get_node(node.id).unwrap().unwrap();
        assert_eq!(got, node);
        // Still exactly one row for this agent.
        assert_eq!(
            s.list_nodes("mark", &NodeQuery::default()).unwrap().len(),
            1
        );
    }

    #[test]
    fn list_nodes_filters() {
        let s = store();
        let mut root = make_node("mark", "discord", None, "Discord gateway");
        root.status = NodeStatus::Approved;
        s.upsert_node(&root).unwrap();
        let child_a = make_node("mark", "discord", Some(root.id), "Heartbeat");
        let child_b = make_node("mark", "discord", Some(root.id), "Reconnect");
        let other_plan = make_node("mark", "slack", None, "Slack events");
        s.upsert_node(&child_a).unwrap();
        s.upsert_node(&child_b).unwrap();
        s.upsert_node(&other_plan).unwrap();

        // No filter ⇒ all four for this agent.
        assert_eq!(
            s.list_nodes("mark", &NodeQuery::default()).unwrap().len(),
            4
        );

        // Plan filter.
        let by_plan = s
            .list_nodes(
                "mark",
                &NodeQuery {
                    plan_id: Some("discord".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(by_plan.len(), 3);

        // Roots only.
        let roots = s
            .list_nodes(
                "mark",
                &NodeQuery {
                    parent: NodeParent::Roots,
                    ..Default::default()
                },
            )
            .unwrap();
        let root_titles: Vec<&str> = roots.iter().map(|n| n.title.as_str()).collect();
        assert_eq!(roots.len(), 2);
        assert!(root_titles.contains(&"Discord gateway"));
        assert!(root_titles.contains(&"Slack events"));

        // Children of a specific node.
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

        // Status filter.
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

        // Limit.
        let limited = s
            .list_nodes(
                "mark",
                &NodeQuery {
                    limit: Some(1),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(limited.len(), 1);

        // Other agent isolated.
        assert!(s
            .list_nodes("alice", &NodeQuery::default())
            .unwrap()
            .is_empty());
    }

    #[test]
    fn node_set_status_records_review_mark() {
        let s = store();
        let node = make_node("mark", "discord", None, "Discord gateway");
        s.upsert_node(&node).unwrap();

        let mark = ReviewMark {
            reviewed_by: "boss".into(),
            note: Some("looks right".into()),
        };
        assert!(
            DeepExpertiseStore::set_status(&s, node.id, NodeStatus::Approved, Some(&mark)).unwrap()
        );
        let got = s.get_node(node.id).unwrap().unwrap();
        assert_eq!(got.status, NodeStatus::Approved);
        assert_eq!(got.reviewed_by.as_deref(), Some("boss"));
        assert_eq!(got.review_note.as_deref(), Some("looks right"));
        assert!(got.reviewed_at.is_some());

        // Without a mark, only status moves; review fields are untouched.
        assert!(DeepExpertiseStore::set_status(&s, node.id, NodeStatus::Stale, None).unwrap());
        let staled = s.get_node(node.id).unwrap().unwrap();
        assert_eq!(staled.status, NodeStatus::Stale);
        assert_eq!(staled.reviewed_by.as_deref(), Some("boss"));

        // Missing id ⇒ false.
        assert!(
            !DeepExpertiseStore::set_status(&s, Uuid::new_v4(), NodeStatus::Approved, None)
                .unwrap()
        );
    }

    #[test]
    fn nodes_by_memory_ids_joins() {
        let s = store();
        let a = make_node("mark", "discord", None, "a");
        let b = make_node("mark", "discord", None, "b");
        s.upsert_node(&a).unwrap();
        s.upsert_node(&b).unwrap();

        let ids = vec![
            a.memory_id.clone().unwrap(),
            b.memory_id.clone().unwrap(),
            "ghost".into(),
        ];
        let mut got = s.nodes_by_memory_ids(&ids).unwrap();
        got.sort_by(|x, y| x.title.cmp(&y.title));
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].id, a.id);
        assert_eq!(got[1].id, b.id);

        // Empty slice short-circuits.
        assert!(s.nodes_by_memory_ids(&[]).unwrap().is_empty());
    }

    #[test]
    fn delete_subtree_removes_descendants() {
        let s = store();
        let root = make_node("mark", "discord", None, "Discord gateway");
        let child = make_node("mark", "discord", Some(root.id), "Heartbeat");
        let grandchild = make_node("mark", "discord", Some(child.id), "Ack timing");
        // An unrelated sibling root that must survive.
        let sibling = make_node("mark", "discord", None, "Slack events");
        for n in [&root, &child, &grandchild, &sibling] {
            s.upsert_node(n).unwrap();
        }

        let deleted = s.delete_subtree(root.id).unwrap();
        assert_eq!(deleted, 3);
        assert!(s.get_node(root.id).unwrap().is_none());
        assert!(s.get_node(child.id).unwrap().is_none());
        assert!(s.get_node(grandchild.id).unwrap().is_none());
        // The sibling is untouched.
        assert!(s.get_node(sibling.id).unwrap().is_some());

        // Deleting a now-absent subtree removes nothing.
        assert_eq!(s.delete_subtree(root.id).unwrap(), 0);
    }

    #[test]
    fn node_due_for_reverify_boundary() {
        let s = store();
        let now = Utc::now();

        // At expiry, modelled ⇒ due (inclusive <=).
        let mut at = make_node("mark", "discord", None, "at boundary");
        at.expires_at = Some(now);
        // Past expiry, verified ⇒ due.
        let mut past = make_node("mark", "discord", None, "past");
        past.status = NodeStatus::Verified;
        past.expires_at = Some(now - Duration::seconds(1));
        // Past expiry, approved ⇒ due.
        let mut approved = make_node("mark", "discord", None, "approved");
        approved.status = NodeStatus::Approved;
        approved.expires_at = Some(now - Duration::seconds(1));
        // Future expiry ⇒ not due.
        let mut future = make_node("mark", "discord", None, "future");
        future.expires_at = Some(now + Duration::days(1));
        // Past expiry but seeded ⇒ not surfaceable, not due.
        let mut seeded = make_node("mark", "discord", None, "seeded");
        seeded.status = NodeStatus::Seeded;
        seeded.expires_at = Some(now - Duration::days(1));
        // No expiry ⇒ never due.
        let mut perpetual = make_node("mark", "discord", None, "perpetual");
        perpetual.expires_at = None;

        for n in [&at, &past, &approved, &future, &seeded, &perpetual] {
            s.upsert_node(n).unwrap();
        }

        let due = DeepExpertiseStore::due_for_reverify(&s, "mark", now).unwrap();
        let titles: Vec<&str> = due.iter().map(|n| n.title.as_str()).collect();
        assert!(titles.contains(&"at boundary"));
        assert!(titles.contains(&"past"));
        assert!(titles.contains(&"approved"));
        assert!(!titles.contains(&"future"));
        assert!(!titles.contains(&"seeded"));
        assert!(!titles.contains(&"perpetual"));
        assert_eq!(due.len(), 3);
    }

    /// Nodes a pre-fix gate promoted on a pass that checked nothing are
    /// indistinguishable from real ones, so a database's first open under the
    /// fix sends every machine-verified node back to `modelled`, once — never
    /// a person's approval, and never a node verified since.
    #[test]
    fn opening_an_unrepaired_database_demotes_its_verified_nodes_once() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let with_status = |title: &str, status: NodeStatus| DeepNode {
            status,
            ..make_node("mark", "discord", None, title)
        };
        let verified = with_status("verified", NodeStatus::Verified);
        let approved = with_status("approved", NodeStatus::Approved);
        {
            let s = SqliteSelfLearningStore::new(tmp.path()).unwrap();
            s.upsert_node(&verified).unwrap();
            s.upsert_node(&approved).unwrap();
            s.lock()
                .unwrap()
                .pragma_update(None, "user_version", 0_i64)
                .unwrap();
        }
        let status = |s: &SqliteSelfLearningStore, id| s.get_node(id).unwrap().unwrap().status;

        let s = SqliteSelfLearningStore::new(tmp.path()).unwrap();
        assert_eq!(status(&s, verified.id), NodeStatus::Modelled);
        assert_eq!(status(&s, approved.id), NodeStatus::Approved);

        DeepExpertiseStore::set_status(&s, verified.id, NodeStatus::Verified, None).unwrap();
        drop(s);
        let s = SqliteSelfLearningStore::new(tmp.path()).unwrap();
        assert_eq!(
            status(&s, verified.id),
            NodeStatus::Verified,
            "a node verified after the repair keeps it"
        );
    }
}
