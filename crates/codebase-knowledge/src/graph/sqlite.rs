//! SQLite implementation of [`CodeGraphBackend`].
//!
//! Moved out of the old `graph/mod.rs` wrapper as part of Phase 3 of
//! the multi-store migration roadmap. The SQL and locking semantics
//! are unchanged — every previously-passing test still exercises this
//! file via the [`crate::graph::CodeGraph`] wrapper.

use async_trait::async_trait;
use eyre::{Result, WrapErr};
use rusqlite::{Connection, OptionalExtension};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{info, warn};

use super::backend::{CodeGraphBackend, EdgeDirection};
use super::entities::{summarisable_where_sql, CodeEntity, EntityId};
use super::relationships::Relationship;
use super::schema::Schema;
use super::vectors::{summary_cursor, FillStage, PendingFile, StoredSummary, VectorFill};
use super::{GraphStats, IndexState, RepoLineage, RepositoryRow};
use crate::lease::{now_ms, AcquireOutcome, IndexLease, IndexPhase, STALE_THRESHOLD};
use std::str::FromStr;

/// SQLite-backed code graph.
pub struct SqliteCodeGraphBackend {
    conn: Arc<Mutex<Connection>>,
    identity: String,
}

/// The graph's identity, as its migration made it.
fn identity_of(conn: &Connection) -> Result<String> {
    Ok(conn.query_row("SELECT id FROM graph_identity", [], |row| row.get(0))?)
}

impl SqliteCodeGraphBackend {
    /// Open or create a code graph database at the given path.
    pub fn open(path: &Path) -> Result<Self> {
        std::fs::create_dir_all(path.parent().unwrap_or(Path::new(".")))?;
        let conn = Connection::open(path)
            .wrap_err_with(|| format!("Failed to open codegraph database at {}", path.display()))?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON; PRAGMA busy_timeout=30000;",
        )?;
        Schema::migrate(&conn)?;

        // Verify we can actually write to the database file (catches
        // permission errors early).
        if let Err(e) = conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS _write_test (x INTEGER); DROP TABLE IF EXISTS _write_test;",
        ) {
            return Err(eyre::eyre!(
                "Code graph database at {} is not writable (check file ownership/permissions): {}",
                path.display(),
                e
            ));
        }

        info!("Code graph database opened at {}", path.display());
        // Built behind the graph's back, so that opening it does not wait
        // (see [`super::schema::FILL_INDEX`]): readers go on meanwhile,
        // and a writer waits its turn as for any other write.
        let built_at = path.to_path_buf();
        let building = std::thread::Builder::new().name("cke-fill-index".to_string());
        let spawned = building.spawn(move || {
            let began = std::time::Instant::now();
            let built = Connection::open(&built_at).and_then(|conn| {
                conn.execute_batch("PRAGMA busy_timeout=120000;")?;
                conn.execute_batch(super::schema::FILL_INDEX)
            });
            match built {
                Ok(()) => tracing::debug!(took = ?began.elapsed(), "the index code-vector fills page by is there"),
                Err(e) => tracing::warn!(error = %e, "the index code-vector fills page by could not be built; fills page by the primary key until it is"),
            }
        });
        if let Err(e) = spawned {
            tracing::warn!(error = %e, "could not start building the fill index");
        }
        Ok(Self {
            identity: identity_of(&conn)?,
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Create an in-memory SQLite database (for testing).
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch("PRAGMA foreign_keys=ON;")?;
        Schema::migrate(&conn)?;
        conn.execute_batch(super::schema::FILL_INDEX)?;
        Ok(Self {
            identity: identity_of(&conn)?,
            conn: Arc::new(Mutex::new(conn)),
        })
    }
}

#[async_trait]
impl CodeGraphBackend for SqliteCodeGraphBackend {
    async fn register_repo(&self, id: &str, name: &str, root_path: &str) -> Result<()> {
        let conn = self.conn.lock().await;
        conn.execute(
            "INSERT OR IGNORE INTO repositories (id, name, root_path, created_at)
             VALUES (?1, ?2, ?3, datetime('now'))",
            rusqlite::params![id, name, root_path],
        )?;
        Ok(())
    }

    async fn upsert_entities(&self, entities: &[CodeEntity]) -> Result<usize> {
        let conn = self.conn.lock().await;
        conn.execute_batch("PRAGMA defer_foreign_keys = ON;")?;
        let tx = conn.unchecked_transaction()?;
        let mut count = 0;
        for entity in entities {
            tx.execute(
                "INSERT OR REPLACE INTO entities
                 (id, repo_id, kind, name, qualified_name, file_path, start_line, end_line,
                  signature, visibility, parent_id, doc_comment, content_hash, language, indexed_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, datetime('now'))",
                rusqlite::params![
                    entity.id.as_str(),
                    entity.repo_id,
                    entity.kind.as_str(),
                    entity.name,
                    entity.qualified_name,
                    entity.file_path,
                    entity.start_line,
                    entity.end_line,
                    entity.signature,
                    entity.visibility.as_str(),
                    entity.parent_id.as_ref().map(|id| id.as_str()),
                    entity.doc_comment,
                    entity.content_hash,
                    entity.language.as_str(),
                ],
            )?;
            count += 1;
        }
        tx.commit()?;
        Ok(count)
    }

    async fn get_entity(&self, id: &EntityId) -> Result<Option<CodeEntity>> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare(
            "SELECT id, repo_id, kind, name, qualified_name, file_path, start_line, end_line,
                    signature, visibility, parent_id, doc_comment, content_hash, language
             FROM entities WHERE id = ?1",
        )?;
        let result = stmt.query_row(rusqlite::params![id.as_str()], |row| {
            Ok(CodeEntity::from_row(row))
        });
        match result {
            Ok(entity) => Ok(Some(entity)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    async fn find_entities_by_name(
        &self,
        repo_id: &str,
        name: &str,
        limit: usize,
    ) -> Result<Vec<CodeEntity>> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare(
            "SELECT id, repo_id, kind, name, qualified_name, file_path, start_line, end_line,
                    signature, visibility, parent_id, doc_comment, content_hash, language
             FROM entities
             WHERE repo_id = ?1 AND (name = ?2 OR qualified_name LIKE ?3)
             LIMIT ?4",
        )?;
        let pattern = format!("%{}", name);
        let entities = stmt
            .query_map(rusqlite::params![repo_id, name, pattern, limit], |row| {
                Ok(CodeEntity::from_row(row))
            })?
            .filter_map(|r| r.ok())
            .collect();
        Ok(entities)
    }

    async fn find_entities_in_file(
        &self,
        repo_id: &str,
        file_path: &str,
    ) -> Result<Vec<CodeEntity>> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare(
            "SELECT id, repo_id, kind, name, qualified_name, file_path, start_line, end_line,
                    signature, visibility, parent_id, doc_comment, content_hash, language
             FROM entities
             WHERE repo_id = ?1 AND file_path = ?2
             ORDER BY start_line",
        )?;
        let entities = stmt
            .query_map(rusqlite::params![repo_id, file_path], |row| {
                Ok(CodeEntity::from_row(row))
            })?
            .filter_map(|r| r.ok())
            .collect();
        Ok(entities)
    }

    async fn find_entities_by_kind(
        &self,
        repo_id: &str,
        kind: &str,
        limit: usize,
    ) -> Result<Vec<CodeEntity>> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare(
            "SELECT id, repo_id, kind, name, qualified_name, file_path, start_line, end_line,
                    signature, visibility, parent_id, doc_comment, content_hash, language
             FROM entities
             WHERE repo_id = ?1 AND kind = ?2
             ORDER BY qualified_name
             LIMIT ?3",
        )?;
        let entities = stmt
            .query_map(rusqlite::params![repo_id, kind, limit], |row| {
                Ok(CodeEntity::from_row(row))
            })?
            .filter_map(|r| r.ok())
            .collect();
        Ok(entities)
    }

    async fn search_entities(
        &self,
        repo_id: &str,
        pattern: &str,
        limit: usize,
    ) -> Result<Vec<CodeEntity>> {
        let conn = self.conn.lock().await;
        let like_pattern = format!("%{}%", pattern);
        let mut stmt = conn.prepare(
            "SELECT id, repo_id, kind, name, qualified_name, file_path, start_line, end_line,
                    signature, visibility, parent_id, doc_comment, content_hash, language
             FROM entities
             WHERE repo_id = ?1 AND (
                 name LIKE ?2 OR qualified_name LIKE ?2 OR
                 doc_comment LIKE ?2 OR signature LIKE ?2
             )
             ORDER BY
                 CASE WHEN name = ?3 THEN 0
                      WHEN name LIKE ?4 THEN 1
                      WHEN qualified_name LIKE ?2 THEN 2
                      ELSE 3 END,
                 name
             LIMIT ?5",
        )?;
        let exact = pattern;
        let prefix_pattern = format!("{}%", pattern);
        let entities = stmt
            .query_map(
                rusqlite::params![repo_id, like_pattern, exact, prefix_pattern, limit],
                |row| Ok(CodeEntity::from_row(row)),
            )?
            .filter_map(|r| r.ok())
            .collect();
        Ok(entities)
    }

    async fn all_entities(&self, repo_id: &str) -> Result<Vec<CodeEntity>> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare(
            "SELECT id, repo_id, kind, name, qualified_name, file_path, start_line, end_line,
                    signature, visibility, parent_id, doc_comment, content_hash, language
             FROM entities WHERE repo_id = ?1 ORDER BY file_path, start_line",
        )?;
        let entities = stmt
            .query_map(rusqlite::params![repo_id], |row| {
                Ok(CodeEntity::from_row(row))
            })?
            .filter_map(|r| r.ok())
            .collect();
        Ok(entities)
    }

    async fn get_entity_indexed_at(&self, entity_id: &EntityId) -> Result<Option<String>> {
        let conn = self.conn.lock().await;
        let result = conn.query_row(
            "SELECT indexed_at FROM entities WHERE id = ?1",
            rusqlite::params![entity_id.as_str()],
            |row| row.get(0),
        );
        match result {
            Ok(ts) => Ok(Some(ts)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    async fn upsert_relationships(&self, relationships: &[Relationship]) -> Result<usize> {
        let conn = self.conn.lock().await;
        conn.execute_batch("PRAGMA defer_foreign_keys = ON;")?;
        let tx = conn.unchecked_transaction()?;
        let mut count = 0;
        for rel in relationships {
            tx.execute(
                "INSERT OR REPLACE INTO relationships
                 (id, source_id, target_id, kind, confidence, metadata, indexed_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, datetime('now'))",
                rusqlite::params![
                    rel.id,
                    rel.source.as_str(),
                    rel.target.as_str(),
                    rel.kind.as_str(),
                    rel.confidence,
                    rel.metadata
                        .as_ref()
                        .map(|m| serde_json::to_string(m).unwrap_or_default()),
                ],
            )?;
            count += 1;
        }
        tx.commit()?;
        Ok(count)
    }

    async fn count_relationships_by_kind(&self, repo_id: &str, kind: &str) -> Result<usize> {
        let conn = self.conn.lock().await;
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM relationships r
             JOIN entities e ON e.id = r.source_id
             WHERE e.repo_id = ?1 AND r.kind = ?2",
            rusqlite::params![repo_id, kind],
            |row| row.get(0),
        )?;
        Ok(count as usize)
    }

    async fn count_dependents(&self, entity_id: &EntityId) -> Result<usize> {
        let conn = self.conn.lock().await;
        let count: i64 = conn.query_row(
            "SELECT COUNT(DISTINCT source_id) FROM relationships
             WHERE target_id = ?1 AND kind IN ('called_by', 'imports', 'depends_on', 'implements')",
            rusqlite::params![entity_id.as_str()],
            |row| row.get(0),
        )?;
        Ok(count as usize)
    }

    async fn neighbours(
        &self,
        current_id: &EntityId,
        kind: &str,
        direction: EdgeDirection,
    ) -> Result<Vec<CodeEntity>> {
        let conn = self.conn.lock().await;
        // For Outgoing, current is the source and we return the target.
        // For Incoming, current is the target and we return the source.
        let (where_col, join_col) = match direction {
            EdgeDirection::Outgoing => ("source_id", "target_id"),
            EdgeDirection::Incoming => ("target_id", "source_id"),
        };
        let sql = format!(
            "SELECT e.id, e.repo_id, e.kind, e.name, e.qualified_name, e.file_path,
                    e.start_line, e.end_line, e.signature, e.visibility, e.parent_id,
                    e.doc_comment, e.content_hash, e.language
             FROM entities e
             JOIN relationships r ON r.{join_col} = e.id
             WHERE r.{where_col} = ?1 AND r.kind = ?2"
        );
        let mut stmt = conn.prepare(&sql)?;
        let entities = stmt
            .query_map(rusqlite::params![current_id.as_str(), kind], |row| {
                Ok(CodeEntity::from_row(row))
            })?
            .filter_map(|r| r.ok())
            .collect();
        Ok(entities)
    }

    async fn upsert_summary(
        &self,
        entity_id: &EntityId,
        granularity: &str,
        text: &str,
        source_hash: &str,
        model_used: &str,
    ) -> Result<()> {
        let conn = self.conn.lock().await;
        conn.execute(
            "INSERT OR REPLACE INTO summaries
             (entity_id, granularity, text, source_hash, model_used, generated_at, is_stale)
             VALUES (?1, ?2, ?3, ?4, ?5, datetime('now'), 0)",
            rusqlite::params![
                entity_id.as_str(),
                granularity,
                text,
                source_hash,
                model_used
            ],
        )?;
        Ok(())
    }

    async fn get_summary(
        &self,
        entity_id: &EntityId,
        granularity: &str,
    ) -> Result<Option<(String, bool)>> {
        let conn = self.conn.lock().await;
        let result = conn.query_row(
            "SELECT text, is_stale FROM summaries WHERE entity_id = ?1 AND granularity = ?2",
            rusqlite::params![entity_id.as_str(), granularity],
            |row| {
                let text: String = row.get(0)?;
                let is_stale: bool = row.get::<_, i64>(1)? != 0;
                Ok((text, is_stale))
            },
        );
        match result {
            Ok(pair) => Ok(Some(pair)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    async fn get_stale_summaries(&self, repo_id: &str) -> Result<Vec<(EntityId, String)>> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare(
            "SELECT s.entity_id, s.granularity
             FROM summaries s
             JOIN entities e ON e.id = s.entity_id
             WHERE e.repo_id = ?1 AND s.is_stale = 1",
        )?;
        let results = stmt
            .query_map(rusqlite::params![repo_id], |row| {
                let id: String = row.get(0)?;
                let granularity: String = row.get(1)?;
                Ok((EntityId(id), granularity))
            })?
            .filter_map(|r| r.ok())
            .collect();
        Ok(results)
    }

    async fn summary_hashes(
        &self,
        repo_id: &str,
    ) -> Result<HashMap<(EntityId, String), (String, bool)>> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare(
            "SELECT s.entity_id, s.granularity, s.source_hash, s.is_stale
             FROM summaries s
             JOIN entities e ON e.id = s.entity_id
             WHERE e.repo_id = ?1",
        )?;
        let rows = stmt.query_map(rusqlite::params![repo_id], |row| {
            let id: String = row.get(0)?;
            let granularity: String = row.get(1)?;
            let source_hash: String = row.get(2)?;
            let is_stale: bool = row.get::<_, i64>(3)? != 0;
            Ok(((EntityId(id), granularity), (source_hash, is_stale)))
        })?;
        let mut map = HashMap::new();
        for r in rows {
            let (k, v) = r?;
            map.insert(k, v);
        }
        Ok(map)
    }

    async fn count_stale_summaries(&self, repo_id: &str) -> Result<usize> {
        let conn = self.conn.lock().await;
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM summaries s
             JOIN entities e ON e.id = s.entity_id
             WHERE e.repo_id = ?1 AND s.is_stale = 1",
            rusqlite::params![repo_id],
            |row| row.get(0),
        )?;
        Ok(count as usize)
    }

    async fn mark_summaries_stale(&self, entity_ids: &[EntityId]) -> Result<usize> {
        if entity_ids.is_empty() {
            return Ok(0);
        }
        let conn = self.conn.lock().await;
        let tx = conn.unchecked_transaction()?;
        let mut total = 0;
        for entity_id in entity_ids {
            let count = tx.execute(
                "UPDATE summaries SET is_stale = 1 WHERE entity_id = ?1 AND is_stale = 0",
                rusqlite::params![entity_id.as_str()],
            )?;
            total += count;
        }
        tx.commit()?;
        Ok(total)
    }

    async fn update_file_index(
        &self,
        repo_id: &str,
        file_path: &str,
        content_hash: &str,
        language: &str,
        entity_count: usize,
    ) -> Result<()> {
        let conn = self.conn.lock().await;
        conn.execute(
            "INSERT OR REPLACE INTO file_index
             (repo_id, file_path, content_hash, language, entity_count, last_indexed_at)
             VALUES (?1, ?2, ?3, ?4, ?5, datetime('now'))",
            rusqlite::params![repo_id, file_path, content_hash, language, entity_count],
        )?;
        Ok(())
    }

    async fn get_file_hash(&self, repo_id: &str, file_path: &str) -> Result<Option<String>> {
        let conn = self.conn.lock().await;
        let result = conn.query_row(
            "SELECT content_hash FROM file_index WHERE repo_id = ?1 AND file_path = ?2",
            rusqlite::params![repo_id, file_path],
            |row| row.get(0),
        );
        match result {
            Ok(hash) => Ok(Some(hash)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    async fn get_file_indexed_at(&self, repo_id: &str, file_path: &str) -> Result<Option<String>> {
        let conn = self.conn.lock().await;
        let result = conn.query_row(
            "SELECT last_indexed_at FROM file_index WHERE repo_id = ?1 AND file_path = ?2",
            rusqlite::params![repo_id, file_path],
            |row| row.get(0),
        );
        match result {
            Ok(ts) => Ok(Some(ts)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    async fn delete_file_from_index(&self, repo_id: &str, file_path: &str) -> Result<()> {
        let conn = self.conn.lock().await;
        conn.execute(
            "DELETE FROM file_index WHERE repo_id = ?1 AND file_path = ?2",
            rusqlite::params![repo_id, file_path],
        )?;
        Ok(())
    }

    async fn get_file_paths(&self, repo_id: &str) -> Result<Vec<String>> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare(
            "SELECT DISTINCT file_path FROM entities WHERE repo_id = ?1 ORDER BY file_path",
        )?;
        let paths = stmt
            .query_map(rusqlite::params![repo_id], |row| row.get(0))?
            .filter_map(|r| r.ok())
            .collect();
        Ok(paths)
    }

    async fn update_index_state_full(
        &self,
        repo_id: &str,
        commit: &str,
        total_entities: usize,
        total_relationships: usize,
    ) -> Result<()> {
        let conn = self.conn.lock().await;
        conn.execute(
            "INSERT OR REPLACE INTO index_state
             (repo_id, last_commit, last_full_index_at, last_incremental_at,
              total_entities, total_relationships, indexing_in_progress)
             VALUES (?1, ?2, datetime('now'), datetime('now'), ?3, ?4, 0)",
            rusqlite::params![repo_id, commit, total_entities, total_relationships],
        )?;
        // A fill begun while the repo was being indexed went by part of it.
        conn.execute(
            "DELETE FROM vector_fill WHERE repo_id = ?1",
            rusqlite::params![repo_id],
        )?;
        Ok(())
    }

    async fn update_index_state_incremental(
        &self,
        repo_id: &str,
        commit: &str,
        total_entities: usize,
        total_relationships: usize,
    ) -> Result<()> {
        let conn = self.conn.lock().await;
        conn.execute(
            "UPDATE index_state
             SET last_commit = ?2, last_incremental_at = datetime('now'),
                 total_entities = ?3, total_relationships = ?4, indexing_in_progress = 0
             WHERE repo_id = ?1",
            rusqlite::params![repo_id, commit, total_entities, total_relationships],
        )?;
        Ok(())
    }

    fn identity(&self) -> &str {
        &self.identity
    }

    async fn entities_page(
        &self,
        repo_id: &str,
        after: &str,
        upto: Option<&str>,
        limit: usize,
    ) -> Result<Vec<CodeEntity>> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare(
            "SELECT id, repo_id, kind, name, qualified_name, file_path, start_line, end_line,
                    signature, visibility, parent_id, doc_comment, content_hash, language
             FROM entities
             WHERE repo_id = ?1 AND id > ?2 AND (?3 IS NULL OR id <= ?3)
             ORDER BY id LIMIT ?4",
        )?;
        let entities = stmt
            .query_map(
                rusqlite::params![repo_id, after, upto, limit as i64],
                |row| Ok(CodeEntity::from_row(row)),
            )?
            .collect::<rusqlite::Result<_>>()?;
        Ok(entities)
    }

    async fn summaries_page(
        &self,
        repo_id: &str,
        after: &str,
        upto: Option<&str>,
        limit: usize,
    ) -> Result<Vec<StoredSummary>> {
        let (entity, granularity) = summary_cursor(after);
        let upto = upto.map(summary_cursor);
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare(
            "SELECT s.entity_id, s.granularity, e.file_path, s.text, s.is_stale
             FROM summaries s JOIN entities e ON e.id = s.entity_id
             WHERE e.repo_id = ?1 AND (s.entity_id, s.granularity) > (?2, ?3)
               AND (?4 IS NULL OR (s.entity_id, s.granularity) <= (?4, ?5))
             ORDER BY s.entity_id, s.granularity LIMIT ?6",
        )?;
        let summaries = stmt
            .query_map(
                rusqlite::params![
                    repo_id,
                    entity,
                    granularity,
                    upto.map(|(entity, _)| entity),
                    upto.map(|(_, granularity)| granularity),
                    limit as i64
                ],
                stored_summary,
            )?
            .collect::<rusqlite::Result<_>>()?;
        Ok(summaries)
    }

    async fn summaries_in_file(
        &self,
        repo_id: &str,
        file_path: &str,
    ) -> Result<Vec<StoredSummary>> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare(
            "SELECT s.entity_id, s.granularity, e.file_path, s.text, s.is_stale
             FROM summaries s JOIN entities e ON e.id = s.entity_id
             WHERE e.repo_id = ?1 AND e.file_path = ?2
             ORDER BY s.entity_id, s.granularity",
        )?;
        let summaries = stmt
            .query_map(rusqlite::params![repo_id, file_path], stored_summary)?
            .collect::<rusqlite::Result<_>>()?;
        Ok(summaries)
    }

    async fn vector_fill(&self, backend: &str, repo_id: &str) -> Result<Option<VectorFill>> {
        let conn = self.conn.lock().await;
        let fill = conn
            .query_row(
                "SELECT generation, collection, stage, cursor, done, total
                 FROM vector_fill WHERE backend = ?1 AND repo_id = ?2",
                rusqlite::params![backend, repo_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, i64>(4)?,
                        row.get::<_, i64>(5)?,
                    ))
                },
            )
            .optional()?;
        Ok(
            fill.and_then(|(generation, collection, stage, cursor, done, total)| {
                Some(VectorFill {
                    generation,
                    collection,
                    stage: FillStage::parse(&stage)?,
                    cursor,
                    done: done as u64,
                    total: total as u64,
                })
            }),
        )
    }

    async fn advance_vector_fill(
        &self,
        backend: &str,
        repo_id: &str,
        from: Option<&VectorFill>,
        to: &VectorFill,
    ) -> Result<bool> {
        let conn = self.conn.lock().await;
        let (done, total) = (to.done as i64, to.total as i64);
        let moved = match from {
            Some(from) => conn.execute(
                "UPDATE vector_fill
                 SET generation = ?2, collection = ?3, stage = ?4, cursor = ?5, done = ?6, total = ?7
                 WHERE repo_id = ?1 AND generation = ?8 AND stage = ?9 AND cursor = ?10
                   AND backend = ?11",
                rusqlite::params![
                    repo_id,
                    to.generation,
                    to.collection,
                    to.stage.as_str(),
                    to.cursor,
                    done,
                    total,
                    from.generation,
                    from.stage.as_str(),
                    from.cursor,
                    backend,
                ],
            )?,
            None => conn.execute(
                "INSERT INTO vector_fill
                     (repo_id, generation, collection, stage, cursor, done, total, backend)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT (backend, repo_id) DO UPDATE
                 SET generation = excluded.generation, collection = excluded.collection,
                     stage = excluded.stage, cursor = excluded.cursor, done = excluded.done,
                     total = excluded.total
                 WHERE vector_fill.stage = 'done'",
                rusqlite::params![
                    repo_id,
                    to.generation,
                    to.collection,
                    to.stage.as_str(),
                    to.cursor,
                    done,
                    total,
                    backend,
                ],
            )?,
        };
        Ok(moved > 0)
    }

    async fn clear_vector_fill(&self, backend: &str, repo_id: &str) -> Result<()> {
        let conn = self.conn.lock().await;
        conn.execute(
            "DELETE FROM vector_fill WHERE backend = ?1 AND repo_id = ?2",
            rusqlite::params![backend, repo_id],
        )?;
        Ok(())
    }

    async fn note_vectors_pending(
        &self,
        backends: &[&str],
        repo_id: &str,
        files: &[String],
    ) -> Result<()> {
        let conn = self.conn.lock().await;
        let tx = conn.unchecked_transaction()?;
        for backend in backends {
            for file in files {
                tx.execute(
                    "INSERT INTO vector_pending (backend, repo_id, file_path) VALUES (?1, ?2, ?3)
                     ON CONFLICT (backend, repo_id, file_path)
                     DO UPDATE SET seq = vector_pending.seq + 1",
                    rusqlite::params![backend, repo_id, file],
                )?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    async fn vectors_pending(
        &self,
        backend: &str,
        repo_id: &str,
        limit: usize,
    ) -> Result<Vec<PendingFile>> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare(
            "SELECT file_path, seq FROM vector_pending WHERE backend = ?1 AND repo_id = ?2
             ORDER BY file_path LIMIT ?3",
        )?;
        let pending = stmt
            .query_map(rusqlite::params![backend, repo_id, limit as i64], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })?
            .collect::<rusqlite::Result<_>>()?;
        Ok(pending)
    }

    async fn count_vectors_pending(&self, backend: &str, repo_id: &str) -> Result<usize> {
        let conn = self.conn.lock().await;
        let pending: i64 = conn.query_row(
            "SELECT count(*) FROM vector_pending WHERE backend = ?1 AND repo_id = ?2",
            rusqlite::params![backend, repo_id],
            |row| row.get(0),
        )?;
        Ok(pending as usize)
    }

    async fn clear_vectors_pending(
        &self,
        backend: &str,
        repo_id: &str,
        file_path: &str,
        seq: i64,
    ) -> Result<()> {
        let conn = self.conn.lock().await;
        conn.execute(
            "DELETE FROM vector_pending
             WHERE backend = ?1 AND repo_id = ?2 AND file_path = ?3 AND seq = ?4",
            rusqlite::params![backend, repo_id, file_path, seq],
        )?;
        Ok(())
    }

    async fn vectors_complete_in(&self, repo_id: &str) -> Result<Option<String>> {
        let conn = self.conn.lock().await;
        let identity = conn
            .query_row(
                "SELECT vectors_complete_in FROM index_state WHERE repo_id = ?1",
                rusqlite::params![repo_id],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?;
        Ok(identity.flatten())
    }

    async fn set_vectors_complete_in(&self, repo_id: &str, identity: Option<&str>) -> Result<()> {
        let conn = self.conn.lock().await;
        conn.execute(
            "UPDATE index_state SET vectors_complete_in = ?2 WHERE repo_id = ?1",
            rusqlite::params![repo_id, identity],
        )?;
        Ok(())
    }

    async fn get_index_state(&self, repo_id: &str) -> Result<Option<IndexState>> {
        let conn = self.conn.lock().await;
        let result = conn.query_row(
            "SELECT repo_id, last_commit, last_full_index_at, last_incremental_at,
                    total_entities, total_relationships, indexing_in_progress
             FROM index_state WHERE repo_id = ?1",
            rusqlite::params![repo_id],
            |row| {
                Ok(IndexState {
                    repo_id: row.get(0)?,
                    last_commit: row.get(1)?,
                    last_full_index_at: row.get(2)?,
                    last_incremental_at: row.get(3)?,
                    total_entities: row.get::<_, i64>(4)? as usize,
                    total_relationships: row.get::<_, i64>(5)? as usize,
                    indexing_in_progress: row.get::<_, i64>(6)? != 0,
                })
            },
        );
        match result {
            Ok(state) => Ok(Some(state)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    async fn set_indexing_in_progress(&self, repo_id: &str, in_progress: bool) -> Result<()> {
        let conn = self.conn.lock().await;
        conn.execute(
            "UPDATE index_state SET indexing_in_progress = ?2 WHERE repo_id = ?1",
            rusqlite::params![repo_id, if in_progress { 1i64 } else { 0i64 }],
        )?;
        Ok(())
    }

    async fn acquire_lease(
        &self,
        repo_id: &str,
        worker_id: &str,
        target_commit: Option<&str>,
        phase: IndexPhase,
    ) -> Result<AcquireOutcome> {
        let conn = self.conn.lock().await;
        let tx = conn.unchecked_transaction()?;
        let now = now_ms();
        let stale_cutoff = now - (STALE_THRESHOLD.as_millis() as i64);

        // Steal stale leases atomically: only take ownership if the
        // current row is stale enough that another acquirer should
        // assume the original worker is gone.
        let stolen = tx.execute(
            "UPDATE index_lease
             SET worker_id = ?2, target_commit = ?3, phase = ?4,
                 acquired_at_ms = ?5, last_heartbeat_at_ms = ?5,
                 entities_total = NULL, entities_processed = NULL,
                 summaries_total = NULL, summaries_processed = NULL
             WHERE repo_id = ?1 AND last_heartbeat_at_ms < ?6",
            rusqlite::params![
                repo_id,
                worker_id,
                target_commit,
                phase.as_str(),
                now,
                stale_cutoff
            ],
        )?;
        if stolen > 0 {
            tx.commit()?;
            info!(repo_id, worker_id, "Stole stale indexing lease");
            return Ok(AcquireOutcome::Acquired);
        }

        // Try to insert a fresh lease. If a row already exists (any
        // freshness), `INSERT OR IGNORE` does nothing and we report it
        // back as Held.
        let inserted = tx.execute(
            "INSERT OR IGNORE INTO index_lease
             (repo_id, worker_id, target_commit, phase,
              acquired_at_ms, last_heartbeat_at_ms,
              entities_total, entities_processed,
              summaries_total, summaries_processed)
             VALUES (?1, ?2, ?3, ?4, ?5, ?5, NULL, NULL, NULL, NULL)",
            rusqlite::params![repo_id, worker_id, target_commit, phase.as_str(), now],
        )?;
        if inserted > 0 {
            tx.commit()?;
            return Ok(AcquireOutcome::Acquired);
        }

        // Row exists and was fresh — read it back so the caller can
        // surface the holder's identity.
        let held = read_lease(&tx, repo_id)?;
        tx.commit()?;
        Ok(match held {
            Some(l) => AcquireOutcome::Held(l),
            // Race: someone released between our INSERT OR IGNORE failing
            // and our SELECT. Treat as a successful acquisition — the
            // table is now empty for this repo and the caller's first
            // heartbeat will recreate the row.
            None => AcquireOutcome::Acquired,
        })
    }

    async fn heartbeat_lease(
        &self,
        repo_id: &str,
        worker_id: &str,
        phase: IndexPhase,
        entities_total: Option<u64>,
        entities_processed: Option<u64>,
        summaries_total: Option<u64>,
        summaries_processed: Option<u64>,
    ) -> Result<()> {
        let conn = self.conn.lock().await;
        conn.execute(
            "UPDATE index_lease
             SET last_heartbeat_at_ms = ?3, phase = ?4,
                 entities_total = COALESCE(?5, entities_total),
                 entities_processed = COALESCE(?6, entities_processed),
                 summaries_total = COALESCE(?7, summaries_total),
                 summaries_processed = COALESCE(?8, summaries_processed)
             WHERE repo_id = ?1 AND worker_id = ?2",
            rusqlite::params![
                repo_id,
                worker_id,
                now_ms(),
                phase.as_str(),
                entities_total.map(|n| n as i64),
                entities_processed.map(|n| n as i64),
                summaries_total.map(|n| n as i64),
                summaries_processed.map(|n| n as i64),
            ],
        )?;
        Ok(())
    }

    async fn release_lease(&self, repo_id: &str, worker_id: &str) -> Result<()> {
        let conn = self.conn.lock().await;
        conn.execute(
            "DELETE FROM index_lease WHERE repo_id = ?1 AND worker_id = ?2",
            rusqlite::params![repo_id, worker_id],
        )?;
        Ok(())
    }

    async fn get_lease(&self, repo_id: &str) -> Result<Option<IndexLease>> {
        let conn = self.conn.lock().await;
        read_lease(&conn, repo_id)
    }

    async fn count_summarisable_entities(&self, repo_id: &str) -> Result<u64> {
        let conn = self.conn.lock().await;
        let sql = format!(
            "SELECT COUNT(*) FROM entities WHERE repo_id = ?1 AND {}",
            summarisable_where_sql()
        );
        let count: i64 = conn.query_row(&sql, rusqlite::params![repo_id], |row| row.get(0))?;
        Ok(count as u64)
    }

    async fn count_summaries_present(&self, repo_id: &str) -> Result<u64> {
        let conn = self.conn.lock().await;
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM summaries s
             JOIN entities e ON e.id = s.entity_id
             WHERE e.repo_id = ?1",
            rusqlite::params![repo_id],
            |row| row.get(0),
        )?;
        Ok(count as u64)
    }

    async fn delete_repo_data(&self, repo_id: &str, keep_pending: &[&str]) -> Result<usize> {
        let conn = self.conn.lock().await;
        let tx = conn.unchecked_transaction()?;

        tx.execute(
            "DELETE FROM relationships WHERE source_id IN
             (SELECT id FROM entities WHERE repo_id = ?1)",
            rusqlite::params![repo_id],
        )?;

        tx.execute(
            "DELETE FROM summaries WHERE entity_id IN
             (SELECT id FROM entities WHERE repo_id = ?1)",
            rusqlite::params![repo_id],
        )?;

        tx.execute(
            "DELETE FROM file_index WHERE repo_id = ?1",
            rusqlite::params![repo_id],
        )?;

        let count = tx.execute(
            "DELETE FROM entities WHERE repo_id = ?1",
            rusqlite::params![repo_id],
        )?;

        tx.execute(
            "DELETE FROM index_state WHERE repo_id = ?1",
            rusqlite::params![repo_id],
        )?;
        tx.execute(
            "DELETE FROM vector_fill WHERE repo_id = ?1",
            rusqlite::params![repo_id],
        )?;
        let kept = serde_json::to_string(keep_pending)?;
        tx.execute(
            "DELETE FROM vector_pending
             WHERE repo_id = ?1 AND backend NOT IN (SELECT value FROM json_each(?2))",
            rusqlite::params![repo_id, kept],
        )?;

        tx.commit()?;

        if count > 0 {
            info!(
                repo_id = %repo_id,
                "Purged {} entities (and associated relationships, summaries, file_index) for full re-index",
                count
            );
        }

        Ok(count)
    }

    async fn drop_repo(&self, repo_id: &str) -> Result<usize> {
        let conn = self.conn.lock().await;
        let tx = conn.unchecked_transaction()?;

        tx.execute(
            "DELETE FROM relationships WHERE source_id IN
             (SELECT id FROM entities WHERE repo_id = ?1)",
            rusqlite::params![repo_id],
        )?;
        tx.execute(
            "DELETE FROM summaries WHERE entity_id IN
             (SELECT id FROM entities WHERE repo_id = ?1)",
            rusqlite::params![repo_id],
        )?;
        tx.execute(
            "DELETE FROM file_index WHERE repo_id = ?1",
            rusqlite::params![repo_id],
        )?;
        let count = tx.execute(
            "DELETE FROM entities WHERE repo_id = ?1",
            rusqlite::params![repo_id],
        )?;
        tx.execute(
            "DELETE FROM index_state WHERE repo_id = ?1",
            rusqlite::params![repo_id],
        )?;
        // Its code vectors' fills and pending files, in every database:
        // nothing of the repo is left to put right.
        for table in ["vector_fill", "vector_pending"] {
            tx.execute(
                &format!("DELETE FROM {table} WHERE repo_id = ?1"),
                rusqlite::params![repo_id],
            )?;
        }
        // Drop any lease before the repositories row — the lease is
        // FK-bound to it.
        tx.execute(
            "DELETE FROM index_lease WHERE repo_id = ?1",
            rusqlite::params![repo_id],
        )?;
        tx.execute(
            "DELETE FROM repositories WHERE id = ?1",
            rusqlite::params![repo_id],
        )?;
        tx.commit()?;

        info!(
            repo_id = %repo_id,
            "Dropped repo {} from graph ({} entities)",
            repo_id, count
        );
        Ok(count)
    }

    async fn list_repos(&self) -> Result<Vec<String>> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare("SELECT id FROM repositories ORDER BY id")?;
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(0))?
            .filter_map(|r| r.ok())
            .collect();
        Ok(rows)
    }

    async fn list_repositories(&self) -> Result<Vec<RepositoryRow>> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare(
            "SELECT id, name, root_path, last_indexed_commit, last_indexed_at, created_at, lineage
             FROM repositories ORDER BY id",
        )?;
        let rows = stmt
            .query_map([], row_to_repository)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    async fn list_repositories_by_lineage(
        &self,
        lineage: RepoLineage,
    ) -> Result<Vec<RepositoryRow>> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare(
            "SELECT id, name, root_path, last_indexed_commit, last_indexed_at, created_at, lineage
             FROM repositories WHERE lineage = ?1 ORDER BY id",
        )?;
        let rows = stmt
            .query_map([lineage.as_str()], row_to_repository)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    async fn set_repo_lineage(&self, repo_id: &str, lineage: RepoLineage) -> Result<()> {
        let conn = self.conn.lock().await;
        conn.execute(
            "UPDATE repositories SET lineage = ?1 WHERE id = ?2",
            rusqlite::params![lineage.as_str(), repo_id],
        )?;
        Ok(())
    }

    async fn list_active_leases(&self) -> Result<Vec<IndexLease>> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare(
            "SELECT repo_id, worker_id, target_commit, phase,
                    acquired_at_ms, last_heartbeat_at_ms,
                    entities_total, entities_processed,
                    summaries_total, summaries_processed
             FROM index_lease
             ORDER BY acquired_at_ms ASC",
        )?;
        let rows = stmt
            .query_map([], |row| {
                Ok(IndexLease {
                    repo_id: row.get(0)?,
                    worker_id: row.get(1)?,
                    target_commit: row.get(2)?,
                    phase: IndexPhase::parse(&row.get::<_, String>(3)?)
                        .unwrap_or(IndexPhase::Crawling),
                    acquired_at_ms: row.get(4)?,
                    last_heartbeat_at_ms: row.get(5)?,
                    entities_total: row.get::<_, Option<i64>>(6)?.map(|n| n as u64),
                    entities_processed: row.get::<_, Option<i64>>(7)?.map(|n| n as u64),
                    summaries_total: row.get::<_, Option<i64>>(8)?.map(|n| n as u64),
                    summaries_processed: row.get::<_, Option<i64>>(9)?.map(|n| n as u64),
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    async fn set_repo_lineage_batch(&self, updates: &[(String, RepoLineage)]) -> Result<usize> {
        if updates.is_empty() {
            return Ok(0);
        }
        let conn = self.conn.lock().await;
        let tx = conn.unchecked_transaction()?;
        let mut touched = 0usize;
        {
            let mut stmt = tx.prepare("UPDATE repositories SET lineage = ?1 WHERE id = ?2")?;
            for (repo_id, lineage) in updates {
                touched += stmt.execute(rusqlite::params![lineage.as_str(), repo_id])?;
            }
        }
        tx.commit()?;
        Ok(touched)
    }

    async fn delete_file_entities(&self, repo_id: &str, file_path: &str) -> Result<usize> {
        let conn = self.conn.lock().await;
        let tx = conn.unchecked_transaction()?;

        tx.execute(
            "DELETE FROM relationships WHERE source_id IN
             (SELECT id FROM entities WHERE repo_id = ?1 AND file_path = ?2)
             OR target_id IN
             (SELECT id FROM entities WHERE repo_id = ?1 AND file_path = ?2)",
            rusqlite::params![repo_id, file_path],
        )?;

        tx.execute(
            "DELETE FROM summaries WHERE entity_id IN
             (SELECT id FROM entities WHERE repo_id = ?1 AND file_path = ?2)",
            rusqlite::params![repo_id, file_path],
        )?;

        let count = tx.execute(
            "DELETE FROM entities WHERE repo_id = ?1 AND file_path = ?2",
            rusqlite::params![repo_id, file_path],
        )?;

        tx.commit()?;
        Ok(count)
    }

    async fn rename_file(&self, repo_id: &str, old_path: &str, new_path: &str) -> Result<usize> {
        let conn = self.conn.lock().await;
        let tx = conn.unchecked_transaction()?;
        let count = tx.execute(
            "UPDATE entities SET file_path = ?3 WHERE repo_id = ?1 AND file_path = ?2",
            rusqlite::params![repo_id, old_path, new_path],
        )?;
        tx.execute(
            "UPDATE file_index SET file_path = ?3 WHERE repo_id = ?1 AND file_path = ?2",
            rusqlite::params![repo_id, old_path, new_path],
        )?;
        tx.commit()?;
        Ok(count)
    }

    async fn get_stats(&self, repo_id: &str) -> Result<GraphStats> {
        let conn = self.conn.lock().await;
        let entity_count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM entities WHERE repo_id = ?1",
            rusqlite::params![repo_id],
            |row| row.get(0),
        )?;
        let relationship_count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM relationships WHERE source_id IN
             (SELECT id FROM entities WHERE repo_id = ?1)",
            rusqlite::params![repo_id],
            |row| row.get(0),
        )?;
        let file_count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM file_index WHERE repo_id = ?1",
            rusqlite::params![repo_id],
            |row| row.get(0),
        )?;
        Ok(GraphStats {
            entity_count: entity_count as usize,
            relationship_count: relationship_count as usize,
            file_count: file_count as usize,
        })
    }

    async fn checkpoint(&self) -> Result<()> {
        let conn = self.conn.lock().await;
        match conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);") {
            Ok(_) => {
                info!("Code graph WAL checkpoint completed");
                Ok(())
            }
            Err(e) => {
                warn!("Code graph WAL checkpoint failed: {}", e);
                Err(e.into())
            }
        }
    }
}

/// Map a `repositories` row to the public `RepositoryRow` shape.
/// Unknown `lineage` strings round-trip to `RepoLineage::Unknown` —
/// a future migration adding a new variant is preferable to a panic
/// when an older binary reads a row tagged by a newer one.
fn row_to_repository(row: &rusqlite::Row<'_>) -> rusqlite::Result<RepositoryRow> {
    let lineage_str: String = row.get(6)?;
    Ok(RepositoryRow {
        id: row.get(0)?,
        name: row.get(1)?,
        root_path: row.get(2)?,
        last_indexed_commit: row.get(3)?,
        last_indexed_at: row.get(4)?,
        created_at: row.get(5)?,
        lineage: RepoLineage::from_str(&lineage_str).unwrap_or(RepoLineage::Unknown),
    })
}

/// Read a lease row by `repo_id`. Accepts both `&Connection` and
/// `&Transaction` since the latter derefs to the former.
fn read_lease(conn: &Connection, repo_id: &str) -> Result<Option<IndexLease>> {
    let result = conn.query_row(
        "SELECT repo_id, worker_id, target_commit, phase,
                acquired_at_ms, last_heartbeat_at_ms,
                entities_total, entities_processed,
                summaries_total, summaries_processed
         FROM index_lease WHERE repo_id = ?1",
        rusqlite::params![repo_id],
        |row| {
            Ok(IndexLease {
                repo_id: row.get(0)?,
                worker_id: row.get(1)?,
                target_commit: row.get(2)?,
                phase: IndexPhase::parse(&row.get::<_, String>(3)?).unwrap_or(IndexPhase::Crawling),
                acquired_at_ms: row.get(4)?,
                last_heartbeat_at_ms: row.get(5)?,
                entities_total: row.get::<_, Option<i64>>(6)?.map(|n| n as u64),
                entities_processed: row.get::<_, Option<i64>>(7)?.map(|n| n as u64),
                summaries_total: row.get::<_, Option<i64>>(8)?.map(|n| n as u64),
                summaries_processed: row.get::<_, Option<i64>>(9)?.map(|n| n as u64),
            })
        },
    );
    match result {
        Ok(l) => Ok(Some(l)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

#[cfg(test)]
mod lease_tests {
    use super::*;
    use crate::lease::{IndexPhase, STALE_THRESHOLD};

    async fn fresh_backend() -> SqliteCodeGraphBackend {
        let backend = SqliteCodeGraphBackend::open_in_memory().expect("open backend");
        // Lease rows reference repositories(id); seed one for the tests.
        backend
            .register_repo("r", "r", "/r")
            .await
            .expect("register repo");
        backend
    }

    #[tokio::test]
    async fn acquire_returns_acquired_when_no_lease_exists() {
        let backend = fresh_backend().await;
        let outcome = backend
            .acquire_lease("r", "w1", Some("abc"), IndexPhase::Crawling)
            .await
            .unwrap();
        assert!(matches!(outcome, AcquireOutcome::Acquired));

        let lease = backend.get_lease("r").await.unwrap().unwrap();
        assert_eq!(lease.worker_id, "w1");
        assert_eq!(lease.phase, IndexPhase::Crawling);
        assert_eq!(lease.target_commit.as_deref(), Some("abc"));
    }

    #[tokio::test]
    async fn second_acquire_returns_held_when_first_is_fresh() {
        let backend = fresh_backend().await;
        backend
            .acquire_lease("r", "w1", None, IndexPhase::Crawling)
            .await
            .unwrap();
        let outcome = backend
            .acquire_lease("r", "w2", None, IndexPhase::Crawling)
            .await
            .unwrap();
        match outcome {
            AcquireOutcome::Held(l) => assert_eq!(l.worker_id, "w1"),
            AcquireOutcome::Acquired => panic!("expected Held"),
        }
    }

    #[tokio::test]
    async fn stale_lease_is_stolen() {
        let backend = fresh_backend().await;
        backend
            .acquire_lease("r", "w1", None, IndexPhase::Crawling)
            .await
            .unwrap();

        // Backdate the heartbeat past the stale threshold.
        let stale_ts = now_ms() - (STALE_THRESHOLD.as_millis() as i64) - 5_000;
        let conn = backend.conn.lock().await;
        conn.execute(
            "UPDATE index_lease SET last_heartbeat_at_ms = ?1 WHERE repo_id = 'r'",
            rusqlite::params![stale_ts],
        )
        .unwrap();
        drop(conn);

        let outcome = backend
            .acquire_lease("r", "w2", None, IndexPhase::Crawling)
            .await
            .unwrap();
        assert!(matches!(outcome, AcquireOutcome::Acquired));
        let lease = backend.get_lease("r").await.unwrap().unwrap();
        assert_eq!(lease.worker_id, "w2");
    }

    #[tokio::test]
    async fn heartbeat_updates_timestamp_and_progress() {
        let backend = fresh_backend().await;
        backend
            .acquire_lease("r", "w1", None, IndexPhase::Crawling)
            .await
            .unwrap();
        let before = backend.get_lease("r").await.unwrap().unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        backend
            .heartbeat_lease(
                "r",
                "w1",
                IndexPhase::GeneratingSummaries,
                Some(100),
                Some(50),
                Some(40),
                Some(10),
            )
            .await
            .unwrap();

        let after = backend.get_lease("r").await.unwrap().unwrap();
        assert!(after.last_heartbeat_at_ms > before.last_heartbeat_at_ms);
        assert_eq!(after.phase, IndexPhase::GeneratingSummaries);
        assert_eq!(after.entities_total, Some(100));
        assert_eq!(after.summaries_processed, Some(10));
    }

    #[tokio::test]
    async fn heartbeat_is_silent_noop_when_worker_id_mismatches() {
        let backend = fresh_backend().await;
        backend
            .acquire_lease("r", "w1", None, IndexPhase::Crawling)
            .await
            .unwrap();
        backend
            .heartbeat_lease("r", "w2", IndexPhase::Parsing, None, None, None, None)
            .await
            .unwrap();
        let lease = backend.get_lease("r").await.unwrap().unwrap();
        assert_eq!(lease.worker_id, "w1");
        assert_eq!(lease.phase, IndexPhase::Crawling);
    }

    #[tokio::test]
    async fn release_drops_only_matching_worker() {
        let backend = fresh_backend().await;
        backend
            .acquire_lease("r", "w1", None, IndexPhase::Crawling)
            .await
            .unwrap();

        backend.release_lease("r", "w2").await.unwrap();
        assert!(backend.get_lease("r").await.unwrap().is_some());

        backend.release_lease("r", "w1").await.unwrap();
        assert!(backend.get_lease("r").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn list_active_leases_returns_empty_when_none_held() {
        let backend = fresh_backend().await;
        let leases = backend.list_active_leases().await.unwrap();
        assert!(leases.is_empty());
    }

    #[tokio::test]
    async fn list_active_leases_returns_held_leases_ordered_by_acquisition() {
        let backend = SqliteCodeGraphBackend::open_in_memory().unwrap();
        for id in ["a", "b", "c"] {
            backend.register_repo(id, id, id).await.unwrap();
        }
        for id in ["c", "a", "b"] {
            backend
                .acquire_lease(id, &format!("w-{id}"), None, IndexPhase::Crawling)
                .await
                .unwrap();
        }
        let leases = backend.list_active_leases().await.unwrap();
        assert_eq!(leases.len(), 3);
        // Sorted by acquired_at_ms, so insertion order ('c', 'a', 'b')
        // is preserved.
        assert_eq!(leases[0].repo_id, "c");
        assert_eq!(leases[1].repo_id, "a");
        assert_eq!(leases[2].repo_id, "b");
    }
}

#[cfg(test)]
mod lineage_tests {
    use super::*;

    async fn fresh_backend() -> SqliteCodeGraphBackend {
        SqliteCodeGraphBackend::open_in_memory().expect("open backend")
    }

    #[tokio::test]
    async fn list_repositories_returns_unknown_by_default() {
        let backend = fresh_backend().await;
        backend.register_repo("r", "r", "/r").await.unwrap();
        let rows = backend.list_repositories().await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "r");
        assert_eq!(rows[0].lineage, RepoLineage::Unknown);
    }

    #[tokio::test]
    async fn set_repo_lineage_roundtrips() {
        let backend = fresh_backend().await;
        backend.register_repo("r", "r", "/r").await.unwrap();
        backend
            .set_repo_lineage("r", RepoLineage::Canonical)
            .await
            .unwrap();
        let rows = backend.list_repositories().await.unwrap();
        assert_eq!(rows[0].lineage, RepoLineage::Canonical);
    }

    #[tokio::test]
    async fn list_repositories_by_lineage_filters() {
        let backend = fresh_backend().await;
        backend.register_repo("a", "a", "/a").await.unwrap();
        backend.register_repo("b", "b", "/b").await.unwrap();
        backend.register_repo("c", "c", "/c").await.unwrap();
        backend
            .set_repo_lineage("a", RepoLineage::Canonical)
            .await
            .unwrap();
        backend
            .set_repo_lineage("b", RepoLineage::MigrationOrphan)
            .await
            .unwrap();
        let canonical = backend
            .list_repositories_by_lineage(RepoLineage::Canonical)
            .await
            .unwrap();
        assert_eq!(canonical.len(), 1);
        assert_eq!(canonical[0].id, "a");
        let orphans = backend
            .list_repositories_by_lineage(RepoLineage::MigrationOrphan)
            .await
            .unwrap();
        assert_eq!(orphans.len(), 1);
        assert_eq!(orphans[0].id, "b");
        let unknown = backend
            .list_repositories_by_lineage(RepoLineage::Unknown)
            .await
            .unwrap();
        assert_eq!(unknown.len(), 1);
        assert_eq!(unknown[0].id, "c");
    }

    #[tokio::test]
    async fn set_repo_lineage_batch_returns_touched_count() {
        let backend = fresh_backend().await;
        backend.register_repo("a", "a", "/a").await.unwrap();
        backend.register_repo("b", "b", "/b").await.unwrap();
        let touched = backend
            .set_repo_lineage_batch(&[
                ("a".into(), RepoLineage::Canonical),
                ("b".into(), RepoLineage::MigrationOrphan),
                ("missing".into(), RepoLineage::Canonical),
            ])
            .await
            .unwrap();
        assert_eq!(touched, 2);
    }

    #[tokio::test]
    async fn set_repo_lineage_batch_empty_is_zero() {
        let backend = fresh_backend().await;
        let touched = backend.set_repo_lineage_batch(&[]).await.unwrap();
        assert_eq!(touched, 0);
    }
}

#[cfg(test)]
mod summarisable_tests {
    use super::*;
    use crate::graph::entities::{
        mk_entity, CodeVisibility, EntityKind, Language, TRIVIAL_FN_MAX_SPAN,
    };

    /// The SQL `count_summarisable_entities` and the Rust
    /// `CodeEntity::is_summarisable` predicate MUST agree on every entity, or
    /// the completeness gate's `summaries_missing()` never reaches zero and the
    /// indexer re-runs the summary phase on every trigger, forever.
    #[tokio::test]
    async fn count_matches_is_summarisable_predicate() {
        let backend = SqliteCodeGraphBackend::open_in_memory().unwrap();
        backend.register_repo("r", "r", "/r").await.unwrap();

        let entities = vec![
            // summarisable (6)
            mk_entity(
                "pub_fn",
                EntityKind::Function,
                "a.rs",
                Some("fn pub_fn()"),
                CodeVisibility::Public,
                None,
                Language::Rust,
                5,
            ),
            mk_entity(
                "method",
                EntityKind::Method,
                "a.rs",
                Some("fn method()"),
                CodeVisibility::Public,
                None,
                Language::Rust,
                5,
            ),
            mk_entity(
                "code_file",
                EntityKind::File,
                "a.rs",
                None,
                CodeVisibility::Public,
                None,
                Language::Rust,
                0,
            ),
            mk_entity(
                "module",
                EntityKind::Module,
                "a.rs",
                None,
                CodeVisibility::Public,
                None,
                Language::Rust,
                0,
            ),
            mk_entity(
                "priv_doc",
                EntityKind::Function,
                "a.rs",
                Some("fn priv_doc()"),
                CodeVisibility::Private,
                Some("doc"),
                Language::Rust,
                TRIVIAL_FN_MAX_SPAN,
            ),
            mk_entity(
                "priv_big",
                EntityKind::Function,
                "a.rs",
                Some("fn priv_big()"),
                CodeVisibility::Private,
                None,
                Language::Rust,
                TRIVIAL_FN_MAX_SPAN + 1,
            ),
            // whitespace-only doc is non-empty → "documented" on BOTH sides
            // (exact-empty check), so this private+tiny fn stays summarisable.
            // Guards the Rust/SQL agreement against whitespace divergence.
            mk_entity(
                "priv_ws_doc",
                EntityKind::Function,
                "a.rs",
                Some("fn priv_ws_doc()"),
                CodeVisibility::Private,
                Some("\n"),
                Language::Rust,
                TRIVIAL_FN_MAX_SPAN,
            ),
            // NOT summarisable (6)
            // empty-string doc is empty on both sides → trivial private helper.
            mk_entity(
                "priv_empty_doc",
                EntityKind::Function,
                "a.rs",
                Some("fn priv_empty_doc()"),
                CodeVisibility::Private,
                Some(""),
                Language::Rust,
                TRIVIAL_FN_MAX_SPAN,
            ),
            mk_entity(
                "readme",
                EntityKind::File,
                "README.md",
                None,
                CodeVisibility::Public,
                None,
                Language::Markdown,
                0,
            ),
            mk_entity(
                "cfg",
                EntityKind::File,
                "c.toml",
                None,
                CodeVisibility::Public,
                None,
                Language::Toml,
                0,
            ),
            mk_entity(
                "priv_tiny",
                EntityKind::Function,
                "a.rs",
                Some("fn priv_tiny()"),
                CodeVisibility::Private,
                None,
                Language::Rust,
                TRIVIAL_FN_MAX_SPAN,
            ),
            mk_entity(
                "a_struct",
                EntityKind::Struct,
                "a.rs",
                Some("struct S"),
                CodeVisibility::Public,
                None,
                Language::Rust,
                5,
            ),
            mk_entity(
                "an_import",
                EntityKind::Import,
                "a.rs",
                Some("use x"),
                CodeVisibility::Public,
                None,
                Language::Rust,
                0,
            ),
        ];
        backend.upsert_entities(&entities).await.unwrap();

        let expected = entities.iter().filter(|e| e.is_summarisable()).count() as u64;
        let actual = backend.count_summarisable_entities("r").await.unwrap();
        assert_eq!(actual, expected, "SQL count must equal the Rust predicate");
        assert_eq!(expected, 7, "fixture sanity");
    }

    /// `summary_hashes` must round-trip `(source_hash, is_stale)` so the
    /// resume skip (`Some((hash, false))`) only fires for fresh, matching rows.
    #[tokio::test]
    async fn summary_hashes_roundtrip_reflects_staleness() {
        let backend = SqliteCodeGraphBackend::open_in_memory().unwrap();
        backend.register_repo("r", "r", "/r").await.unwrap();
        let e = mk_entity(
            "pub_fn",
            EntityKind::Function,
            "a.rs",
            Some("fn pub_fn()"),
            CodeVisibility::Public,
            None,
            Language::Rust,
            5,
        );
        backend
            .upsert_entities(std::slice::from_ref(&e))
            .await
            .unwrap();
        backend
            .upsert_summary(&e.id, "function", "text", "hash-1", "model")
            .await
            .unwrap();

        let key = (e.id.clone(), "function".to_string());
        let hashes = backend.summary_hashes("r").await.unwrap();
        assert_eq!(hashes.get(&key), Some(&("hash-1".to_string(), false)));

        backend
            .mark_summaries_stale(std::slice::from_ref(&e.id))
            .await
            .unwrap();
        let hashes = backend.summary_hashes("r").await.unwrap();
        assert_eq!(hashes.get(&key), Some(&("hash-1".to_string(), true)));
    }
}

/// A [`StoredSummary`] from a row of entity, granularity, file, text and
/// staleness.
fn stored_summary(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredSummary> {
    Ok(StoredSummary {
        entity_id: row.get(0)?,
        granularity: row.get(1)?,
        file_path: row.get(2)?,
        text: row.get(3)?,
        is_stale: row.get::<_, i64>(4)? != 0,
    })
}
