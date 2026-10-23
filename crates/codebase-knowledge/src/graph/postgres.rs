//! Postgres implementation of [`CodeGraphBackend`]. Mirrors every
//! method of [`crate::graph::sqlite::SqliteCodeGraphBackend`] and is
//! selected at runtime via `[codegraph] backend = "postgres"`. Tables
//! live in the `codegraph` schema; the pool is built via
//! [`PostgresCodeGraphBackend::build_pool`] which pins `search_path`
//! to that schema on every connection. See `docs/POSTGRES.md` for the
//! migration roadmap and the SQLite-to-Postgres translation rules.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use eyre::{Context, Result};
use std::collections::HashMap;
use std::sync::Arc;
use tokio_postgres::Row;
use workforce_postgres::{
    MigrationRun, MigrationRunner, MigrationSet, PostgresConfig, PostgresError, PostgresPool,
};

use super::backend::{CodeGraphBackend, EdgeDirection};
use super::entities::{
    summarisable_where_sql, CodeEntity, CodeVisibility, EntityId, EntityKind, Language,
};
use super::relationships::Relationship;
use super::vectors::{summary_cursor, FillStage, PendingFile, StoredSummary, VectorFill};
use super::{GraphStats, IndexState, RepoLineage, RepositoryRow};
use crate::lease::{now_ms, AcquireOutcome, IndexLease, IndexPhase, STALE_THRESHOLD};
use std::str::FromStr;

mod embedded {
    refinery::embed_migrations!("migrations");
}

pub struct PostgresCodeGraphBackend {
    pool: PostgresPool,
    identity: String,
}

impl PostgresCodeGraphBackend {
    /// Schema this store owns. All code graph tables live under it.
    pub const SCHEMA: &'static str = "codegraph";

    /// Build a `PostgresPool` with `search_path` pinned to the
    /// `codegraph` schema.
    pub fn build_pool(config: &PostgresConfig) -> Result<PostgresPool, PostgresError> {
        PostgresPool::from_config_with_session_init(
            config,
            &["SET search_path TO codegraph, public"],
        )
    }

    /// Build a pool from config, run pending migrations, and return a
    /// ready-to-use backend.
    pub async fn new(config: &PostgresConfig) -> Result<Arc<Self>> {
        let pool = Self::build_pool(config).context("building codegraph pool")?;
        Self::with_pool(pool).await
    }

    /// Variant taking a pre-built pool whose `search_path` has
    /// already been pinned. Used by tests.
    pub async fn with_pool(pool: PostgresPool) -> Result<Arc<Self>> {
        let migration_set = MigrationSet::new(
            Self::SCHEMA,
            Box::new(EmbeddedCodegraphMigrations) as Box<dyn MigrationRun>,
        );
        MigrationRunner::run(&pool, &[migration_set])
            .await
            .context("running codegraph migrations")?;
        let identity: String = {
            let conn = pool.get().await?;
            conn.query_one("SELECT id FROM graph_identity", &[])
                .await
                .context("reading the code graph's identity")?
                .get(0)
        };
        // Built after the hub has started, not in its boot: see V005.
        tokio::spawn(Self::keep_fill_index(pool.clone(), Self::FILL_INDEX_RETRY));
        Ok(Arc::new(Self { pool, identity }))
    }

    /// How long after finding the fill index not there, and not built by
    /// this hub, it is looked for again: long enough to cost nothing, and
    /// short beside a fill of a graph large enough to need the index.
    const FILL_INDEX_RETRY: std::time::Duration = std::time::Duration::from_secs(60);
    const FILL_INDEX: &'static str = "idx_entities_repo_id";
    /// The session advisory lock one hub builds the fill index under: any
    /// constant no other lock of the hub's shares.
    const FILL_INDEX_BUILDING: i64 = 0x7766_6331_6478;

    /// See to it that the fill index is there and valid, looking again
    /// every `every` until it is: when another hub was building it, which
    /// may have stopped part of the way and left it invalid, and when this
    /// one could not build it. Ends once the index is valid.
    pub(crate) async fn keep_fill_index(pool: PostgresPool, every: std::time::Duration) {
        let mut warned = false;
        loop {
            match Self::ensure_fill_index(&pool).await {
                Ok(true) => return,
                Ok(false) => tracing::debug!(
                    again_in = ?every,
                    "another hub is building the index code-vector fills page by"
                ),
                // Said once, and then as often as it is tried.
                Err(e) if !warned => {
                    warned = true;
                    tracing::warn!(error = ?e, again_in = ?every, "the index code-vector fills page by could not be built; fills page by the primary key until it is, which is slower on a large graph");
                }
                Err(e) => tracing::debug!(error = ?e, "the fill index could still not be built"),
            }
            tokio::time::sleep(every).await;
        }
    }

    /// Whether the fill index is there, and whether Postgres holds it
    /// valid: an interrupted build leaves one that is not.
    async fn fill_index_valid(conn: &tokio_postgres::Client) -> Result<Option<bool>> {
        let found = conn
            .query_opt(
                "SELECT i.indisvalid FROM pg_index i \
                 JOIN pg_class c ON c.oid = i.indexrelid \
                 JOIN pg_namespace n ON n.oid = c.relnamespace \
                 WHERE c.relname = $1 AND n.nspname = current_schema()",
                &[&Self::FILL_INDEX],
            )
            .await?;
        Ok(found.map(|row| row.get(0)))
    }

    /// Build the index a fill pages a repo's entities by, `(repo_id, id)`,
    /// if it is not there: CONCURRENTLY, so that nothing waits for it, and
    /// outside any transaction, which a concurrent build must be. A build
    /// that was interrupted leaves an index Postgres marks invalid: that
    /// is dropped and built again.
    ///
    /// `Ok(true)` once the index is there and valid. One hub builds it at
    /// a time, under a session advisory lock: another that finds the lock
    /// taken is told `Ok(false)`, the index not yet there, and looks again
    /// later ([`Self::keep_fill_index`]), for the one building may not
    /// finish.
    pub(crate) async fn ensure_fill_index(pool: &PostgresPool) -> Result<bool> {
        const INDEX: &str = PostgresCodeGraphBackend::FILL_INDEX;
        let conn = pool.get().await?;
        if Self::fill_index_valid(&conn).await? == Some(true) {
            return Ok(true);
        }
        let taken: bool = conn
            .query_one(
                "SELECT pg_try_advisory_lock($1)",
                &[&Self::FILL_INDEX_BUILDING],
            )
            .await?
            .get(0);
        if !taken {
            return Ok(false);
        }
        let built = async {
            // Looked at again under the lock: built meanwhile, perhaps.
            match Self::fill_index_valid(&conn).await? {
                Some(true) => return Ok(()),
                Some(false) => {
                    conn.batch_execute(&format!("DROP INDEX CONCURRENTLY IF EXISTS {INDEX}"))
                        .await
                        .context("dropping the half-built fill index")?;
                }
                None => {}
            }
            let began = std::time::Instant::now();
            conn.batch_execute(&format!(
                "CREATE INDEX CONCURRENTLY IF NOT EXISTS {INDEX} ON entities(repo_id, id)"
            ))
            .await
            .context("building the fill index")?;
            tracing::info!(took = ?began.elapsed(), "built the index code-vector fills page by");
            Ok(())
        };
        let built: Result<()> = built.await;
        let _ = conn
            .execute(
                "SELECT pg_advisory_unlock($1)",
                &[&Self::FILL_INDEX_BUILDING],
            )
            .await;
        built.map(|()| true)
    }
}

struct EmbeddedCodegraphMigrations;

#[async_trait::async_trait]
impl MigrationRun for EmbeddedCodegraphMigrations {
    async fn run(&self, client: &mut tokio_postgres::Client) -> Result<(), PostgresError> {
        embedded::migrations::runner()
            .run_async(client)
            .await
            .map_err(PostgresError::from)?;
        Ok(())
    }
}

// ─── Row mapping ───────────────────────────────────────────────────────

fn row_to_entity(row: &Row) -> CodeEntity {
    let id: String = row.get("id");
    let kind_str: String = row.get("kind");
    let visibility_str: String = row.get("visibility");
    let parent_id: Option<String> = row.get("parent_id");
    let language_str: String = row.get("language");
    let start_line: i64 = row.get("start_line");
    let end_line: i64 = row.get("end_line");
    CodeEntity {
        id: EntityId(id),
        repo_id: row.get("repo_id"),
        kind: EntityKind::from_str(&kind_str),
        name: row.get("name"),
        qualified_name: row.get("qualified_name"),
        file_path: row.get("file_path"),
        start_line: start_line as usize,
        end_line: end_line as usize,
        signature: row.get("signature"),
        visibility: CodeVisibility::from_str(&visibility_str),
        parent_id: parent_id.map(EntityId),
        doc_comment: row.get("doc_comment"),
        content_hash: row.get("content_hash"),
        language: Language::from_str(&language_str),
    }
}

fn stored_summary(row: &Row) -> StoredSummary {
    StoredSummary {
        entity_id: row.get(0),
        granularity: row.get(1),
        file_path: row.get(2),
        text: row.get(3),
        is_stale: row.get(4),
    }
}

fn ts_to_string(opt: Option<DateTime<Utc>>) -> Option<String> {
    opt.map(|dt| dt.to_rfc3339())
}

fn pg_row_to_repository(row: &Row) -> RepositoryRow {
    let lineage_str: String = row.get(6);
    RepositoryRow {
        id: row.get(0),
        name: row.get(1),
        root_path: row.get(2),
        last_indexed_commit: row.get(3),
        last_indexed_at: ts_to_string(row.get(4)),
        created_at: ts_to_string(row.get::<_, Option<DateTime<Utc>>>(5)).unwrap_or_default(),
        lineage: RepoLineage::from_str(&lineage_str).unwrap_or(RepoLineage::Unknown),
    }
}

// ─── Trait impl ────────────────────────────────────────────────────────

#[async_trait]
impl CodeGraphBackend for PostgresCodeGraphBackend {
    async fn register_repo(&self, id: &str, name: &str, root_path: &str) -> Result<()> {
        let conn = self.pool.get().await?;
        conn.execute(
            "INSERT INTO repositories (id, name, root_path, created_at)
             VALUES ($1, $2, $3, now())
             ON CONFLICT (id) DO NOTHING",
            &[&id, &name, &root_path],
        )
        .await
        .context("insert repositories")?;
        Ok(())
    }

    async fn upsert_entities(&self, entities: &[CodeEntity]) -> Result<usize> {
        if entities.is_empty() {
            return Ok(0);
        }
        let mut conn = self.pool.get().await?;
        let tx = conn.transaction().await?;
        // Defer self-FK and repo FK checks to end of tx so children
        // can be inserted before parents.
        tx.execute("SET CONSTRAINTS ALL DEFERRED", &[]).await?;
        for entity in entities {
            let start_line = entity.start_line as i64;
            let end_line = entity.end_line as i64;
            let parent_id_str = entity.parent_id.as_ref().map(|id| id.as_str().to_string());
            tx.execute(
                "INSERT INTO entities
                 (id, repo_id, kind, name, qualified_name, file_path, start_line, end_line,
                  signature, visibility, parent_id, doc_comment, content_hash, language, indexed_at)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, now())
                 ON CONFLICT (id) DO UPDATE SET
                    repo_id = EXCLUDED.repo_id,
                    kind = EXCLUDED.kind,
                    name = EXCLUDED.name,
                    qualified_name = EXCLUDED.qualified_name,
                    file_path = EXCLUDED.file_path,
                    start_line = EXCLUDED.start_line,
                    end_line = EXCLUDED.end_line,
                    signature = EXCLUDED.signature,
                    visibility = EXCLUDED.visibility,
                    parent_id = EXCLUDED.parent_id,
                    doc_comment = EXCLUDED.doc_comment,
                    content_hash = EXCLUDED.content_hash,
                    language = EXCLUDED.language,
                    indexed_at = now()",
                &[
                    &entity.id.as_str(),
                    &entity.repo_id,
                    &entity.kind.as_str(),
                    &entity.name,
                    &entity.qualified_name,
                    &entity.file_path,
                    &start_line,
                    &end_line,
                    &entity.signature,
                    &entity.visibility.as_str(),
                    &parent_id_str,
                    &entity.doc_comment,
                    &entity.content_hash,
                    &entity.language.as_str(),
                ],
            )
            .await
            .context("upsert entity")?;
        }
        tx.commit().await?;
        Ok(entities.len())
    }

    async fn get_entity(&self, id: &EntityId) -> Result<Option<CodeEntity>> {
        let conn = self.pool.get().await?;
        let row = conn
            .query_opt(
                "SELECT id, repo_id, kind, name, qualified_name, file_path, start_line, end_line,
                        signature, visibility, parent_id, doc_comment, content_hash, language
                 FROM entities WHERE id = $1",
                &[&id.as_str()],
            )
            .await?;
        Ok(row.as_ref().map(row_to_entity))
    }

    async fn find_entities_by_name(
        &self,
        repo_id: &str,
        name: &str,
        limit: usize,
    ) -> Result<Vec<CodeEntity>> {
        let conn = self.pool.get().await?;
        let pattern = format!("%{}", name);
        let limit = limit as i64;
        let rows = conn
            .query(
                "SELECT id, repo_id, kind, name, qualified_name, file_path, start_line, end_line,
                        signature, visibility, parent_id, doc_comment, content_hash, language
                 FROM entities
                 WHERE repo_id = $1 AND (name = $2 OR qualified_name LIKE $3)
                 LIMIT $4",
                &[&repo_id, &name, &pattern, &limit],
            )
            .await?;
        Ok(rows.iter().map(row_to_entity).collect())
    }

    async fn find_entities_in_file(
        &self,
        repo_id: &str,
        file_path: &str,
    ) -> Result<Vec<CodeEntity>> {
        let conn = self.pool.get().await?;
        let rows = conn
            .query(
                "SELECT id, repo_id, kind, name, qualified_name, file_path, start_line, end_line,
                        signature, visibility, parent_id, doc_comment, content_hash, language
                 FROM entities
                 WHERE repo_id = $1 AND file_path = $2
                 ORDER BY start_line",
                &[&repo_id, &file_path],
            )
            .await?;
        Ok(rows.iter().map(row_to_entity).collect())
    }

    async fn find_entities_by_kind(
        &self,
        repo_id: &str,
        kind: &str,
        limit: usize,
    ) -> Result<Vec<CodeEntity>> {
        let conn = self.pool.get().await?;
        let limit = limit as i64;
        let rows = conn
            .query(
                "SELECT id, repo_id, kind, name, qualified_name, file_path, start_line, end_line,
                        signature, visibility, parent_id, doc_comment, content_hash, language
                 FROM entities
                 WHERE repo_id = $1 AND kind = $2
                 ORDER BY qualified_name
                 LIMIT $3",
                &[&repo_id, &kind, &limit],
            )
            .await?;
        Ok(rows.iter().map(row_to_entity).collect())
    }

    async fn search_entities(
        &self,
        repo_id: &str,
        pattern: &str,
        limit: usize,
    ) -> Result<Vec<CodeEntity>> {
        let conn = self.pool.get().await?;
        let like_pattern = format!("%{}%", pattern);
        let prefix_pattern = format!("{}%", pattern);
        let limit = limit as i64;
        let rows = conn
            .query(
                "SELECT id, repo_id, kind, name, qualified_name, file_path, start_line, end_line,
                        signature, visibility, parent_id, doc_comment, content_hash, language
                 FROM entities
                 WHERE repo_id = $1 AND (
                     name LIKE $2 OR qualified_name LIKE $2 OR
                     doc_comment LIKE $2 OR signature LIKE $2
                 )
                 ORDER BY
                     CASE WHEN name = $3 THEN 0
                          WHEN name LIKE $4 THEN 1
                          WHEN qualified_name LIKE $2 THEN 2
                          ELSE 3 END,
                     name
                 LIMIT $5",
                &[&repo_id, &like_pattern, &pattern, &prefix_pattern, &limit],
            )
            .await?;
        Ok(rows.iter().map(row_to_entity).collect())
    }

    async fn all_entities(&self, repo_id: &str) -> Result<Vec<CodeEntity>> {
        let conn = self.pool.get().await?;
        let rows = conn
            .query(
                "SELECT id, repo_id, kind, name, qualified_name, file_path, start_line, end_line,
                        signature, visibility, parent_id, doc_comment, content_hash, language
                 FROM entities WHERE repo_id = $1 ORDER BY file_path, start_line",
                &[&repo_id],
            )
            .await?;
        Ok(rows.iter().map(row_to_entity).collect())
    }

    async fn get_entity_indexed_at(&self, entity_id: &EntityId) -> Result<Option<String>> {
        let conn = self.pool.get().await?;
        let row = conn
            .query_opt(
                "SELECT indexed_at FROM entities WHERE id = $1",
                &[&entity_id.as_str()],
            )
            .await?;
        Ok(row
            .as_ref()
            .map(|r| r.get::<_, DateTime<Utc>>("indexed_at"))
            .map(|dt| dt.to_rfc3339()))
    }

    async fn upsert_relationships(&self, relationships: &[Relationship]) -> Result<usize> {
        if relationships.is_empty() {
            return Ok(0);
        }
        let mut conn = self.pool.get().await?;
        let tx = conn.transaction().await?;
        tx.execute("SET CONSTRAINTS ALL DEFERRED", &[]).await?;
        for rel in relationships {
            let confidence = rel.confidence as f64;
            let metadata = rel.metadata.clone();
            tx.execute(
                "INSERT INTO relationships
                 (id, source_id, target_id, kind, confidence, metadata, indexed_at)
                 VALUES ($1, $2, $3, $4, $5, $6, now())
                 ON CONFLICT (id) DO UPDATE SET
                    source_id = EXCLUDED.source_id,
                    target_id = EXCLUDED.target_id,
                    kind = EXCLUDED.kind,
                    confidence = EXCLUDED.confidence,
                    metadata = EXCLUDED.metadata,
                    indexed_at = now()",
                &[
                    &rel.id,
                    &rel.source.as_str(),
                    &rel.target.as_str(),
                    &rel.kind.as_str(),
                    &confidence,
                    &metadata,
                ],
            )
            .await
            .context("upsert relationship")?;
        }
        tx.commit().await?;
        Ok(relationships.len())
    }

    async fn count_relationships_by_kind(&self, repo_id: &str, kind: &str) -> Result<usize> {
        let conn = self.pool.get().await?;
        let row = conn
            .query_one(
                "SELECT COUNT(*)::BIGINT FROM relationships r
                 JOIN entities e ON e.id = r.source_id
                 WHERE e.repo_id = $1 AND r.kind = $2",
                &[&repo_id, &kind],
            )
            .await?;
        Ok(row.get::<_, i64>(0) as usize)
    }

    async fn count_dependents(&self, entity_id: &EntityId) -> Result<usize> {
        let conn = self.pool.get().await?;
        let row = conn
            .query_one(
                "SELECT COUNT(DISTINCT source_id)::BIGINT FROM relationships
                 WHERE target_id = $1 AND kind IN ('called_by', 'imports', 'depends_on', 'implements')",
                &[&entity_id.as_str()],
            )
            .await?;
        Ok(row.get::<_, i64>(0) as usize)
    }

    async fn neighbours(
        &self,
        current_id: &EntityId,
        kind: &str,
        direction: EdgeDirection,
    ) -> Result<Vec<CodeEntity>> {
        let conn = self.pool.get().await?;
        let sql = match direction {
            EdgeDirection::Outgoing => {
                "SELECT e.id, e.repo_id, e.kind, e.name, e.qualified_name, e.file_path,
                        e.start_line, e.end_line, e.signature, e.visibility, e.parent_id,
                        e.doc_comment, e.content_hash, e.language
                 FROM entities e
                 JOIN relationships r ON r.target_id = e.id
                 WHERE r.source_id = $1 AND r.kind = $2"
            }
            EdgeDirection::Incoming => {
                "SELECT e.id, e.repo_id, e.kind, e.name, e.qualified_name, e.file_path,
                        e.start_line, e.end_line, e.signature, e.visibility, e.parent_id,
                        e.doc_comment, e.content_hash, e.language
                 FROM entities e
                 JOIN relationships r ON r.source_id = e.id
                 WHERE r.target_id = $1 AND r.kind = $2"
            }
        };
        let rows = conn.query(sql, &[&current_id.as_str(), &kind]).await?;
        Ok(rows.iter().map(row_to_entity).collect())
    }

    async fn upsert_summary(
        &self,
        entity_id: &EntityId,
        granularity: &str,
        text: &str,
        source_hash: &str,
        model_used: &str,
    ) -> Result<()> {
        let conn = self.pool.get().await?;
        conn.execute(
            "INSERT INTO summaries
             (entity_id, granularity, text, source_hash, model_used, generated_at, is_stale)
             VALUES ($1, $2, $3, $4, $5, now(), FALSE)
             ON CONFLICT (entity_id, granularity) DO UPDATE SET
                text = EXCLUDED.text,
                source_hash = EXCLUDED.source_hash,
                model_used = EXCLUDED.model_used,
                generated_at = now(),
                is_stale = FALSE",
            &[
                &entity_id.as_str(),
                &granularity,
                &text,
                &source_hash,
                &model_used,
            ],
        )
        .await?;
        Ok(())
    }

    async fn get_summary(
        &self,
        entity_id: &EntityId,
        granularity: &str,
    ) -> Result<Option<(String, bool)>> {
        let conn = self.pool.get().await?;
        let row = conn
            .query_opt(
                "SELECT text, is_stale FROM summaries WHERE entity_id = $1 AND granularity = $2",
                &[&entity_id.as_str(), &granularity],
            )
            .await?;
        Ok(row.as_ref().map(|r| {
            let text: String = r.get("text");
            let is_stale: bool = r.get("is_stale");
            (text, is_stale)
        }))
    }

    async fn get_stale_summaries(&self, repo_id: &str) -> Result<Vec<(EntityId, String)>> {
        let conn = self.pool.get().await?;
        let rows = conn
            .query(
                "SELECT s.entity_id, s.granularity
                 FROM summaries s
                 JOIN entities e ON e.id = s.entity_id
                 WHERE e.repo_id = $1 AND s.is_stale = TRUE",
                &[&repo_id],
            )
            .await?;
        Ok(rows
            .iter()
            .map(|r| {
                (
                    EntityId(r.get::<_, String>("entity_id")),
                    r.get("granularity"),
                )
            })
            .collect())
    }

    async fn summary_hashes(
        &self,
        repo_id: &str,
    ) -> Result<HashMap<(EntityId, String), (String, bool)>> {
        let conn = self.pool.get().await?;
        let rows = conn
            .query(
                "SELECT s.entity_id, s.granularity, s.source_hash, s.is_stale
                 FROM summaries s
                 JOIN entities e ON e.id = s.entity_id
                 WHERE e.repo_id = $1",
                &[&repo_id],
            )
            .await?;
        let mut map = HashMap::new();
        for r in &rows {
            let id: String = r.get("entity_id");
            let granularity: String = r.get("granularity");
            let source_hash: String = r.get("source_hash");
            let is_stale: bool = r.get("is_stale");
            map.insert((EntityId(id), granularity), (source_hash, is_stale));
        }
        Ok(map)
    }

    async fn count_stale_summaries(&self, repo_id: &str) -> Result<usize> {
        let conn = self.pool.get().await?;
        let row = conn
            .query_one(
                "SELECT COUNT(*)::BIGINT FROM summaries s
                 JOIN entities e ON e.id = s.entity_id
                 WHERE e.repo_id = $1 AND s.is_stale = TRUE",
                &[&repo_id],
            )
            .await?;
        Ok(row.get::<_, i64>(0) as usize)
    }

    async fn mark_summaries_stale(&self, entity_ids: &[EntityId]) -> Result<usize> {
        if entity_ids.is_empty() {
            return Ok(0);
        }
        let mut conn = self.pool.get().await?;
        let tx = conn.transaction().await?;
        let mut total = 0u64;
        for entity_id in entity_ids {
            let n = tx
                .execute(
                    "UPDATE summaries SET is_stale = TRUE
                     WHERE entity_id = $1 AND is_stale = FALSE",
                    &[&entity_id.as_str()],
                )
                .await?;
            total += n;
        }
        tx.commit().await?;
        Ok(total as usize)
    }

    async fn update_file_index(
        &self,
        repo_id: &str,
        file_path: &str,
        content_hash: &str,
        language: &str,
        entity_count: usize,
    ) -> Result<()> {
        let conn = self.pool.get().await?;
        let entity_count = entity_count as i64;
        conn.execute(
            "INSERT INTO file_index
             (repo_id, file_path, content_hash, language, entity_count, last_indexed_at)
             VALUES ($1, $2, $3, $4, $5, now())
             ON CONFLICT (repo_id, file_path) DO UPDATE SET
                content_hash = EXCLUDED.content_hash,
                language = EXCLUDED.language,
                entity_count = EXCLUDED.entity_count,
                last_indexed_at = now()",
            &[
                &repo_id,
                &file_path,
                &content_hash,
                &language,
                &entity_count,
            ],
        )
        .await?;
        Ok(())
    }

    async fn get_file_hash(&self, repo_id: &str, file_path: &str) -> Result<Option<String>> {
        let conn = self.pool.get().await?;
        let row = conn
            .query_opt(
                "SELECT content_hash FROM file_index WHERE repo_id = $1 AND file_path = $2",
                &[&repo_id, &file_path],
            )
            .await?;
        Ok(row.as_ref().map(|r| r.get("content_hash")))
    }

    async fn get_file_indexed_at(&self, repo_id: &str, file_path: &str) -> Result<Option<String>> {
        let conn = self.pool.get().await?;
        let row = conn
            .query_opt(
                "SELECT last_indexed_at FROM file_index WHERE repo_id = $1 AND file_path = $2",
                &[&repo_id, &file_path],
            )
            .await?;
        Ok(ts_to_string(
            row.as_ref()
                .map(|r| r.get::<_, DateTime<Utc>>("last_indexed_at")),
        ))
    }

    async fn delete_file_from_index(&self, repo_id: &str, file_path: &str) -> Result<()> {
        let conn = self.pool.get().await?;
        conn.execute(
            "DELETE FROM file_index WHERE repo_id = $1 AND file_path = $2",
            &[&repo_id, &file_path],
        )
        .await?;
        Ok(())
    }

    async fn get_file_paths(&self, repo_id: &str) -> Result<Vec<String>> {
        let conn = self.pool.get().await?;
        let rows = conn
            .query(
                "SELECT DISTINCT file_path FROM entities WHERE repo_id = $1 ORDER BY file_path",
                &[&repo_id],
            )
            .await?;
        Ok(rows.iter().map(|r| r.get("file_path")).collect())
    }

    async fn update_index_state_full(
        &self,
        repo_id: &str,
        commit: &str,
        total_entities: usize,
        total_relationships: usize,
    ) -> Result<()> {
        let conn = self.pool.get().await?;
        let total_entities = total_entities as i64;
        let total_relationships = total_relationships as i64;
        conn.execute(
            "INSERT INTO index_state
             (repo_id, last_commit, last_full_index_at, last_incremental_at,
              total_entities, total_relationships, indexing_in_progress)
             VALUES ($1, $2, now(), now(), $3, $4, FALSE)
             ON CONFLICT (repo_id) DO UPDATE SET
                last_commit = EXCLUDED.last_commit,
                last_full_index_at = now(),
                last_incremental_at = now(),
                total_entities = EXCLUDED.total_entities,
                total_relationships = EXCLUDED.total_relationships,
                indexing_in_progress = FALSE,
                vectors_complete_in = NULL",
            &[&repo_id, &commit, &total_entities, &total_relationships],
        )
        .await?;
        // A fill begun while the repo was being indexed went by part of it.
        conn.execute("DELETE FROM vector_fill WHERE repo_id = $1", &[&repo_id])
            .await?;
        Ok(())
    }

    async fn update_index_state_incremental(
        &self,
        repo_id: &str,
        commit: &str,
        total_entities: usize,
        total_relationships: usize,
    ) -> Result<()> {
        let conn = self.pool.get().await?;
        let total_entities = total_entities as i64;
        let total_relationships = total_relationships as i64;
        conn.execute(
            "UPDATE index_state
             SET last_commit = $2, last_incremental_at = now(),
                 total_entities = $3, total_relationships = $4, indexing_in_progress = FALSE
             WHERE repo_id = $1",
            &[&repo_id, &commit, &total_entities, &total_relationships],
        )
        .await?;
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
        let conn = self.pool.get().await?;
        let rows = conn
            .query(
                "SELECT id, repo_id, kind, name, qualified_name, file_path, start_line, end_line,
                        signature, visibility, parent_id, doc_comment, content_hash, language
                 FROM entities
                 WHERE repo_id = $1 AND id > $2 AND ($3::text IS NULL OR id <= $3)
                 ORDER BY id LIMIT $4",
                &[&repo_id, &after, &upto, &(limit as i64)],
            )
            .await?;
        Ok(rows.iter().map(row_to_entity).collect())
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
        let (upto_entity, upto_granularity) = (upto.map(|upto| upto.0), upto.map(|upto| upto.1));
        let conn = self.pool.get().await?;
        let rows = conn
            .query(
                "SELECT s.entity_id, s.granularity, e.file_path, s.text, s.is_stale
                 FROM summaries s JOIN entities e ON e.id = s.entity_id
                 WHERE e.repo_id = $1 AND (s.entity_id, s.granularity) > ($2, $3)
                   AND ($4::text IS NULL OR (s.entity_id, s.granularity) <= ($4, $5))
                 ORDER BY s.entity_id, s.granularity LIMIT $6",
                &[
                    &repo_id,
                    &entity,
                    &granularity,
                    &upto_entity,
                    &upto_granularity,
                    &(limit as i64),
                ],
            )
            .await?;
        Ok(rows.iter().map(stored_summary).collect())
    }

    async fn summaries_in_file(
        &self,
        repo_id: &str,
        file_path: &str,
    ) -> Result<Vec<StoredSummary>> {
        let conn = self.pool.get().await?;
        let rows = conn
            .query(
                "SELECT s.entity_id, s.granularity, e.file_path, s.text, s.is_stale
                 FROM summaries s JOIN entities e ON e.id = s.entity_id
                 WHERE e.repo_id = $1 AND e.file_path = $2
                 ORDER BY s.entity_id, s.granularity",
                &[&repo_id, &file_path],
            )
            .await?;
        Ok(rows.iter().map(stored_summary).collect())
    }

    async fn vector_fill(&self, backend: &str, repo_id: &str) -> Result<Option<VectorFill>> {
        let conn = self.pool.get().await?;
        let row = conn
            .query_opt(
                "SELECT generation, collection, stage, cursor, done, total
                 FROM vector_fill WHERE backend = $1 AND repo_id = $2",
                &[&backend, &repo_id],
            )
            .await?;
        Ok(row.and_then(|row| {
            Some(VectorFill {
                generation: row.get(0),
                collection: row.get(1),
                stage: FillStage::parse(row.get(2))?,
                cursor: row.get(3),
                done: row.get::<_, i64>(4) as u64,
                total: row.get::<_, i64>(5) as u64,
            })
        }))
    }

    async fn advance_vector_fill(
        &self,
        backend: &str,
        repo_id: &str,
        from: Option<&VectorFill>,
        to: &VectorFill,
    ) -> Result<bool> {
        let conn = self.pool.get().await?;
        let (stage, done, total) = (to.stage.as_str(), to.done as i64, to.total as i64);
        let moved = match from {
            Some(from) => {
                conn.execute(
                    "UPDATE vector_fill
                     SET generation = $2, collection = $3, stage = $4, cursor = $5, done = $6, total = $7
                     WHERE repo_id = $1 AND generation = $8 AND stage = $9 AND cursor = $10
                       AND backend = $11",
                    &[
                        &repo_id,
                        &to.generation,
                        &to.collection,
                        &stage,
                        &to.cursor,
                        &done,
                        &total,
                        &from.generation,
                        &from.stage.as_str(),
                        &from.cursor,
                        &backend,
                    ],
                )
                .await?
            }
            None => {
                conn.execute(
                    "INSERT INTO vector_fill
                         (repo_id, generation, collection, stage, cursor, done, total, backend)
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
                     ON CONFLICT (backend, repo_id) DO UPDATE
                     SET generation = EXCLUDED.generation, collection = EXCLUDED.collection,
                         stage = EXCLUDED.stage, cursor = EXCLUDED.cursor, done = EXCLUDED.done,
                         total = EXCLUDED.total
                     WHERE vector_fill.stage = 'done'",
                    &[
                        &repo_id,
                        &to.generation,
                        &to.collection,
                        &stage,
                        &to.cursor,
                        &done,
                        &total,
                        &backend,
                    ],
                )
                .await?
            }
        };
        Ok(moved > 0)
    }

    async fn clear_vector_fill(&self, backend: &str, repo_id: &str) -> Result<()> {
        let conn = self.pool.get().await?;
        conn.execute(
            "DELETE FROM vector_fill WHERE backend = $1 AND repo_id = $2",
            &[&backend, &repo_id],
        )
        .await?;
        Ok(())
    }

    async fn note_vectors_pending(
        &self,
        backends: &[&str],
        repo_id: &str,
        files: &[String],
    ) -> Result<()> {
        let backends: Vec<String> = backends.iter().map(|b| b.to_string()).collect();
        let conn = self.pool.get().await?;
        conn.execute(
            "INSERT INTO vector_pending (backend, repo_id, file_path)
             SELECT backend, $1, file
             FROM unnest($2::text[]) AS b(backend), unnest($3::text[]) AS noted(file)
             ON CONFLICT (backend, repo_id, file_path) DO UPDATE SET seq = vector_pending.seq + 1",
            &[&repo_id, &backends, &files],
        )
        .await?;
        Ok(())
    }

    async fn vectors_pending(
        &self,
        backend: &str,
        repo_id: &str,
        limit: usize,
    ) -> Result<Vec<PendingFile>> {
        let conn = self.pool.get().await?;
        let rows = conn
            .query(
                "SELECT file_path, seq FROM vector_pending WHERE backend = $1 AND repo_id = $2
                 ORDER BY file_path LIMIT $3",
                &[&backend, &repo_id, &(limit as i64)],
            )
            .await?;
        Ok(rows.iter().map(|row| (row.get(0), row.get(1))).collect())
    }

    async fn count_vectors_pending(&self, backend: &str, repo_id: &str) -> Result<usize> {
        let conn = self.pool.get().await?;
        let pending: i64 = conn
            .query_one(
                "SELECT count(*) FROM vector_pending WHERE backend = $1 AND repo_id = $2",
                &[&backend, &repo_id],
            )
            .await?
            .get(0);
        Ok(pending as usize)
    }

    async fn clear_vectors_pending(
        &self,
        backend: &str,
        repo_id: &str,
        file_path: &str,
        seq: i64,
    ) -> Result<()> {
        let conn = self.pool.get().await?;
        conn.execute(
            "DELETE FROM vector_pending
             WHERE backend = $1 AND repo_id = $2 AND file_path = $3 AND seq = $4",
            &[&backend, &repo_id, &file_path, &seq],
        )
        .await?;
        Ok(())
    }

    async fn vectors_complete_in(&self, repo_id: &str) -> Result<Option<String>> {
        let conn = self.pool.get().await?;
        let row = conn
            .query_opt(
                "SELECT vectors_complete_in FROM index_state WHERE repo_id = $1",
                &[&repo_id],
            )
            .await?;
        Ok(row.and_then(|row| row.get::<_, Option<String>>(0)))
    }

    async fn set_vectors_complete_in(&self, repo_id: &str, identity: Option<&str>) -> Result<()> {
        let conn = self.pool.get().await?;
        conn.execute(
            "UPDATE index_state SET vectors_complete_in = $2 WHERE repo_id = $1",
            &[&repo_id, &identity],
        )
        .await?;
        Ok(())
    }

    async fn get_index_state(&self, repo_id: &str) -> Result<Option<IndexState>> {
        let conn = self.pool.get().await?;
        let row = conn
            .query_opt(
                "SELECT repo_id, last_commit, last_full_index_at, last_incremental_at,
                        total_entities, total_relationships, indexing_in_progress
                 FROM index_state WHERE repo_id = $1",
                &[&repo_id],
            )
            .await?;
        Ok(row.as_ref().map(|r| {
            let total_entities: i64 = r.get("total_entities");
            let total_relationships: i64 = r.get("total_relationships");
            let last_full: Option<DateTime<Utc>> = r.get("last_full_index_at");
            let last_inc: Option<DateTime<Utc>> = r.get("last_incremental_at");
            IndexState {
                repo_id: r.get("repo_id"),
                last_commit: r.get("last_commit"),
                last_full_index_at: last_full.map(|dt| dt.to_rfc3339()),
                last_incremental_at: last_inc.map(|dt| dt.to_rfc3339()),
                total_entities: total_entities as usize,
                total_relationships: total_relationships as usize,
                indexing_in_progress: r.get("indexing_in_progress"),
            }
        }))
    }

    async fn set_indexing_in_progress(&self, repo_id: &str, in_progress: bool) -> Result<()> {
        let conn = self.pool.get().await?;
        conn.execute(
            "UPDATE index_state SET indexing_in_progress = $2 WHERE repo_id = $1",
            &[&repo_id, &in_progress],
        )
        .await?;
        Ok(())
    }

    async fn acquire_lease(
        &self,
        repo_id: &str,
        worker_id: &str,
        target_commit: Option<&str>,
        phase: IndexPhase,
    ) -> Result<AcquireOutcome> {
        let mut conn = self.pool.get().await?;
        let tx = conn.transaction().await?;
        let now = now_ms();
        let stale_cutoff = now - (STALE_THRESHOLD.as_millis() as i64);
        let phase_str = phase.as_str();
        let target = target_commit;

        let stolen = tx
            .execute(
                "UPDATE index_lease
                 SET worker_id = $2, target_commit = $3, phase = $4,
                     acquired_at_ms = $5, last_heartbeat_at_ms = $5,
                     entities_total = NULL, entities_processed = NULL,
                     summaries_total = NULL, summaries_processed = NULL
                 WHERE repo_id = $1 AND last_heartbeat_at_ms < $6",
                &[
                    &repo_id,
                    &worker_id,
                    &target,
                    &phase_str,
                    &now,
                    &stale_cutoff,
                ],
            )
            .await?;
        if stolen > 0 {
            tx.commit().await?;
            tracing::info!(repo_id, worker_id, "Stole stale indexing lease");
            return Ok(AcquireOutcome::Acquired);
        }

        let inserted = tx
            .execute(
                "INSERT INTO index_lease
                 (repo_id, worker_id, target_commit, phase,
                  acquired_at_ms, last_heartbeat_at_ms,
                  entities_total, entities_processed,
                  summaries_total, summaries_processed)
                 VALUES ($1, $2, $3, $4, $5, $5, NULL, NULL, NULL, NULL)
                 ON CONFLICT (repo_id) DO NOTHING",
                &[&repo_id, &worker_id, &target, &phase_str, &now],
            )
            .await?;
        if inserted > 0 {
            tx.commit().await?;
            return Ok(AcquireOutcome::Acquired);
        }

        let row = tx.query_opt(LEASE_SELECT_SQL, &[&repo_id]).await?;
        tx.commit().await?;
        Ok(match row.as_ref().map(row_to_lease) {
            Some(l) => AcquireOutcome::Held(l),
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
        let conn = self.pool.get().await?;
        let now = now_ms();
        let phase_str = phase.as_str();
        let entities_total = entities_total.map(|n| n as i64);
        let entities_processed = entities_processed.map(|n| n as i64);
        let summaries_total = summaries_total.map(|n| n as i64);
        let summaries_processed = summaries_processed.map(|n| n as i64);
        conn.execute(
            "UPDATE index_lease
             SET last_heartbeat_at_ms = $3, phase = $4,
                 entities_total = COALESCE($5, entities_total),
                 entities_processed = COALESCE($6, entities_processed),
                 summaries_total = COALESCE($7, summaries_total),
                 summaries_processed = COALESCE($8, summaries_processed)
             WHERE repo_id = $1 AND worker_id = $2",
            &[
                &repo_id,
                &worker_id,
                &now,
                &phase_str,
                &entities_total,
                &entities_processed,
                &summaries_total,
                &summaries_processed,
            ],
        )
        .await?;
        Ok(())
    }

    async fn release_lease(&self, repo_id: &str, worker_id: &str) -> Result<()> {
        let conn = self.pool.get().await?;
        conn.execute(
            "DELETE FROM index_lease WHERE repo_id = $1 AND worker_id = $2",
            &[&repo_id, &worker_id],
        )
        .await?;
        Ok(())
    }

    async fn get_lease(&self, repo_id: &str) -> Result<Option<IndexLease>> {
        let conn = self.pool.get().await?;
        let row = conn.query_opt(LEASE_SELECT_SQL, &[&repo_id]).await?;
        Ok(row.as_ref().map(row_to_lease))
    }

    async fn count_summarisable_entities(&self, repo_id: &str) -> Result<u64> {
        let conn = self.pool.get().await?;
        let sql = format!(
            "SELECT COUNT(*)::BIGINT FROM entities WHERE repo_id = $1 AND {}",
            summarisable_where_sql()
        );
        let row = conn.query_one(&sql, &[&repo_id]).await?;
        let count: i64 = row.get(0);
        Ok(count as u64)
    }

    async fn count_summaries_present(&self, repo_id: &str) -> Result<u64> {
        let conn = self.pool.get().await?;
        let row = conn
            .query_one(
                "SELECT COUNT(*)::BIGINT FROM summaries s
                 JOIN entities e ON e.id = s.entity_id
                 WHERE e.repo_id = $1",
                &[&repo_id],
            )
            .await?;
        let count: i64 = row.get(0);
        Ok(count as u64)
    }

    async fn delete_repo_data(&self, repo_id: &str, keep_pending: &[&str]) -> Result<usize> {
        let mut conn = self.pool.get().await?;
        let tx = conn.transaction().await?;

        tx.execute(
            "DELETE FROM relationships WHERE source_id IN
             (SELECT id FROM entities WHERE repo_id = $1)",
            &[&repo_id],
        )
        .await?;

        tx.execute(
            "DELETE FROM summaries WHERE entity_id IN
             (SELECT id FROM entities WHERE repo_id = $1)",
            &[&repo_id],
        )
        .await?;

        tx.execute("DELETE FROM file_index WHERE repo_id = $1", &[&repo_id])
            .await?;

        let count = tx
            .execute("DELETE FROM entities WHERE repo_id = $1", &[&repo_id])
            .await?;

        tx.execute("DELETE FROM index_state WHERE repo_id = $1", &[&repo_id])
            .await?;
        tx.execute("DELETE FROM vector_fill WHERE repo_id = $1", &[&repo_id])
            .await?;
        let kept: Vec<String> = keep_pending.iter().map(|b| b.to_string()).collect();
        tx.execute(
            "DELETE FROM vector_pending WHERE repo_id = $1 AND NOT (backend = ANY($2))",
            &[&repo_id, &kept],
        )
        .await?;

        tx.commit().await?;
        Ok(count as usize)
    }

    async fn drop_repo(&self, repo_id: &str) -> Result<usize> {
        let mut conn = self.pool.get().await?;
        let tx = conn.transaction().await?;

        tx.execute(
            "DELETE FROM relationships WHERE source_id IN
             (SELECT id FROM entities WHERE repo_id = $1)",
            &[&repo_id],
        )
        .await?;
        tx.execute(
            "DELETE FROM summaries WHERE entity_id IN
             (SELECT id FROM entities WHERE repo_id = $1)",
            &[&repo_id],
        )
        .await?;
        tx.execute("DELETE FROM file_index WHERE repo_id = $1", &[&repo_id])
            .await?;
        let count = tx
            .execute("DELETE FROM entities WHERE repo_id = $1", &[&repo_id])
            .await?;
        tx.execute("DELETE FROM index_state WHERE repo_id = $1", &[&repo_id])
            .await?;
        // Its code vectors' fills and pending files, in every database:
        // nothing of the repo is left to put right.
        for table in ["vector_fill", "vector_pending"] {
            tx.execute(
                &format!("DELETE FROM {table} WHERE repo_id = $1"),
                &[&repo_id],
            )
            .await?;
        }
        // Drop any lease before the repositories row — the lease is
        // FK-bound to it.
        tx.execute("DELETE FROM index_lease WHERE repo_id = $1", &[&repo_id])
            .await?;
        tx.execute("DELETE FROM repositories WHERE id = $1", &[&repo_id])
            .await?;

        tx.commit().await?;
        Ok(count as usize)
    }

    async fn list_repos(&self) -> Result<Vec<String>> {
        let conn = self.pool.get().await?;
        let rows = conn
            .query("SELECT id FROM repositories ORDER BY id", &[])
            .await?;
        Ok(rows.into_iter().map(|r| r.get(0)).collect())
    }

    async fn list_repositories(&self) -> Result<Vec<RepositoryRow>> {
        let conn = self.pool.get().await?;
        let rows = conn
            .query(
                "SELECT id, name, root_path, last_indexed_commit, last_indexed_at,
                        created_at, lineage
                 FROM repositories ORDER BY id",
                &[],
            )
            .await?;
        Ok(rows.iter().map(pg_row_to_repository).collect())
    }

    async fn list_repositories_by_lineage(
        &self,
        lineage: RepoLineage,
    ) -> Result<Vec<RepositoryRow>> {
        let conn = self.pool.get().await?;
        let rows = conn
            .query(
                "SELECT id, name, root_path, last_indexed_commit, last_indexed_at,
                        created_at, lineage
                 FROM repositories WHERE lineage = $1 ORDER BY id",
                &[&lineage.as_str()],
            )
            .await?;
        Ok(rows.iter().map(pg_row_to_repository).collect())
    }

    async fn set_repo_lineage(&self, repo_id: &str, lineage: RepoLineage) -> Result<()> {
        let conn = self.pool.get().await?;
        conn.execute(
            "UPDATE repositories SET lineage = $1 WHERE id = $2",
            &[&lineage.as_str(), &repo_id],
        )
        .await?;
        Ok(())
    }

    async fn list_active_leases(&self) -> Result<Vec<IndexLease>> {
        let conn = self.pool.get().await?;
        let rows = conn
            .query(
                "SELECT repo_id, worker_id, target_commit, phase,
                        acquired_at_ms, last_heartbeat_at_ms,
                        entities_total, entities_processed,
                        summaries_total, summaries_processed
                 FROM index_lease
                 ORDER BY acquired_at_ms ASC",
                &[],
            )
            .await?;
        Ok(rows.iter().map(row_to_lease).collect())
    }

    async fn set_repo_lineage_batch(&self, updates: &[(String, RepoLineage)]) -> Result<usize> {
        if updates.is_empty() {
            return Ok(0);
        }
        let mut conn = self.pool.get().await?;
        let tx = conn.transaction().await?;
        let stmt = tx
            .prepare("UPDATE repositories SET lineage = $1 WHERE id = $2")
            .await?;
        let mut touched = 0u64;
        for (repo_id, lineage) in updates {
            touched += tx.execute(&stmt, &[&lineage.as_str(), &repo_id]).await?;
        }
        tx.commit().await?;
        Ok(touched as usize)
    }

    async fn delete_file_entities(&self, repo_id: &str, file_path: &str) -> Result<usize> {
        let mut conn = self.pool.get().await?;
        let tx = conn.transaction().await?;

        tx.execute(
            "DELETE FROM relationships WHERE source_id IN
             (SELECT id FROM entities WHERE repo_id = $1 AND file_path = $2)
             OR target_id IN
             (SELECT id FROM entities WHERE repo_id = $1 AND file_path = $2)",
            &[&repo_id, &file_path],
        )
        .await?;

        tx.execute(
            "DELETE FROM summaries WHERE entity_id IN
             (SELECT id FROM entities WHERE repo_id = $1 AND file_path = $2)",
            &[&repo_id, &file_path],
        )
        .await?;

        let count = tx
            .execute(
                "DELETE FROM entities WHERE repo_id = $1 AND file_path = $2",
                &[&repo_id, &file_path],
            )
            .await?;

        tx.commit().await?;
        Ok(count as usize)
    }

    async fn rename_file(&self, repo_id: &str, old_path: &str, new_path: &str) -> Result<usize> {
        let mut conn = self.pool.get().await?;
        let tx = conn.transaction().await?;
        let count = tx
            .execute(
                "UPDATE entities SET file_path = $3 WHERE repo_id = $1 AND file_path = $2",
                &[&repo_id, &old_path, &new_path],
            )
            .await?;
        tx.execute(
            "UPDATE file_index SET file_path = $3 WHERE repo_id = $1 AND file_path = $2",
            &[&repo_id, &old_path, &new_path],
        )
        .await?;
        tx.commit().await?;
        Ok(count as usize)
    }

    async fn get_stats(&self, repo_id: &str) -> Result<GraphStats> {
        let conn = self.pool.get().await?;
        let entity_count: i64 = conn
            .query_one(
                "SELECT COUNT(*)::BIGINT FROM entities WHERE repo_id = $1",
                &[&repo_id],
            )
            .await?
            .get(0);
        let relationship_count: i64 = conn
            .query_one(
                "SELECT COUNT(*)::BIGINT FROM relationships WHERE source_id IN
                 (SELECT id FROM entities WHERE repo_id = $1)",
                &[&repo_id],
            )
            .await?
            .get(0);
        let file_count: i64 = conn
            .query_one(
                "SELECT COUNT(*)::BIGINT FROM file_index WHERE repo_id = $1",
                &[&repo_id],
            )
            .await?
            .get(0);
        Ok(GraphStats {
            entity_count: entity_count as usize,
            relationship_count: relationship_count as usize,
            file_count: file_count as usize,
        })
    }

    async fn checkpoint(&self) -> Result<()> {
        // Postgres fsyncs at commit time so there's no equivalent to
        // SQLite's `wal_checkpoint(TRUNCATE)`. The trait contract says
        // "flush any pending writes so data survives an abrupt
        // shutdown" — in Postgres that's already the case for every
        // committed transaction, so the operation is a no-op.
        Ok(())
    }
}

/// `SELECT` clause for the lease table, shared between `acquire_lease`
/// (inside its transaction) and `get_lease` (against a pooled client).
/// The two callers use different connection types — `tokio_postgres::Transaction`
/// and `deadpool_postgres::ClientWrapper` — neither of which share a
/// trait that would let us factor the call into a generic helper, so
/// the SQL string and row mapper are extracted instead.
const LEASE_SELECT_SQL: &str = "SELECT repo_id, worker_id, target_commit, phase,
            acquired_at_ms, last_heartbeat_at_ms,
            entities_total, entities_processed,
            summaries_total, summaries_processed
     FROM index_lease WHERE repo_id = $1";

fn row_to_lease(r: &Row) -> IndexLease {
    let phase_str: String = r.get("phase");
    IndexLease {
        repo_id: r.get("repo_id"),
        worker_id: r.get("worker_id"),
        target_commit: r.get("target_commit"),
        phase: IndexPhase::parse(&phase_str).unwrap_or(IndexPhase::Crawling),
        acquired_at_ms: r.get("acquired_at_ms"),
        last_heartbeat_at_ms: r.get("last_heartbeat_at_ms"),
        entities_total: r.get::<_, Option<i64>>("entities_total").map(|n| n as u64),
        entities_processed: r
            .get::<_, Option<i64>>("entities_processed")
            .map(|n| n as u64),
        summaries_total: r.get::<_, Option<i64>>("summaries_total").map(|n| n as u64),
        summaries_processed: r
            .get::<_, Option<i64>>("summaries_processed")
            .map(|n| n as u64),
    }
}

// ─── Parity tests against embedded Postgres ────────────────────────────
//
// Same pattern as `workforce-common::telemetry_postgres::tests`:
// `WORKFORCE_TEST_POSTGRES_URL` for CI / external postgres, fall back
// to `postgresql_embedded` when unset.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::entities::{CodeVisibility, EntityKind, Language};
    use crate::graph::relationships::RelationshipKind;
    use crate::graph::CodeGraph;
    use workforce_postgres::test_support::with_test_pool;

    async fn with_embedded_pg<F, Fut>(test: F)
    where
        F: FnOnce(CodeGraph) -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        with_test_pool(PostgresCodeGraphBackend::build_pool, |pool| async move {
            let backend = PostgresCodeGraphBackend::with_pool(pool)
                .await
                .expect("construct backend");
            let graph = CodeGraph::from_backend(backend);
            test(graph).await;
        })
        .await;
    }

    fn make_entity(id: &str, name: &str, kind: EntityKind) -> CodeEntity {
        CodeEntity {
            id: EntityId(id.to_string()),
            repo_id: "test-repo".into(),
            kind,
            name: name.into(),
            qualified_name: format!("test::{name}"),
            file_path: "src/lib.rs".into(),
            start_line: 10,
            end_line: 20,
            signature: Some(format!("fn {name}()")),
            visibility: CodeVisibility::Public,
            parent_id: None,
            doc_comment: None,
            content_hash: "abc".into(),
            language: Language::Rust,
        }
    }

    /// The index fills page by is built after the graph is opened, not in
    /// the migration, and built again if a build was interrupted and left
    /// it invalid; a hub that finds another building it leaves it be.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_fill_index_is_built_outside_the_migration_and_rebuilt_if_left_invalid() {
        with_test_pool(PostgresCodeGraphBackend::build_pool, |pool| async move {
            let valid = || async {
                let conn = pool.get().await.unwrap();
                let found = conn
                    .query_opt(
                        "SELECT i.indisvalid FROM pg_index i \
                         JOIN pg_class c ON c.oid = i.indexrelid \
                         WHERE c.relname = 'idx_entities_repo_id'",
                        &[],
                    )
                    .await
                    .unwrap();
                found.map(|row| row.get::<_, bool>(0))
            };
            // Not by the migration.
            let migration = include_str!("../../migrations/V005__vector_fill.sql");
            assert!(!migration
                .lines()
                .any(|line| line.trim_start().starts_with("CREATE INDEX")));
            let _graph = PostgresCodeGraphBackend::with_pool(pool.clone())
                .await
                .unwrap();
            // By the hub, once it is up; and at once when asked.
            PostgresCodeGraphBackend::ensure_fill_index(&pool)
                .await
                .unwrap();
            let begun = std::time::Instant::now();
            while valid().await != Some(true) {
                assert!(begun.elapsed() < std::time::Duration::from_secs(20));
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
            // As an interrupted build leaves it.
            let conn = pool.get().await.unwrap();
            conn.execute(
                "UPDATE pg_index SET indisvalid = false WHERE indexrelid = \
                 (SELECT oid FROM pg_class WHERE relname = 'idx_entities_repo_id')",
                &[],
            )
            .await
            .unwrap();
            assert_eq!(valid().await, Some(false));
            PostgresCodeGraphBackend::ensure_fill_index(&pool)
                .await
                .unwrap();
            assert_eq!(valid().await, Some(true));
        })
        .await;
    }

    /// A hub that finds another building the fill index does not leave it
    /// at that: it looks again until the index is there, and builds it
    /// itself once the other has let go without finishing.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_hub_that_finds_the_fill_index_being_built_sees_it_through() {
        use std::time::Duration;
        with_test_pool(PostgresCodeGraphBackend::build_pool, |pool| async move {
            let conn = pool.get().await.unwrap();
            let migrations = MigrationSet::new(
                PostgresCodeGraphBackend::SCHEMA,
                Box::new(EmbeddedCodegraphMigrations) as Box<dyn MigrationRun>,
            );
            MigrationRunner::run(&pool, &[migrations]).await.unwrap();
            // Another hub has the build, and does not get on with it.
            let building = PostgresCodeGraphBackend::FILL_INDEX_BUILDING;
            conn.execute("SELECT pg_advisory_lock($1)", &[&building])
                .await
                .unwrap();
            assert!(!PostgresCodeGraphBackend::ensure_fill_index(&pool)
                .await
                .unwrap());
            let keeping = tokio::spawn(PostgresCodeGraphBackend::keep_fill_index(
                pool.clone(),
                Duration::from_millis(20),
            ));
            tokio::time::sleep(Duration::from_millis(300)).await;
            assert!(!keeping.is_finished(), "it goes on looking");
            let valid = PostgresCodeGraphBackend::fill_index_valid(&conn).await;
            assert_eq!(valid.unwrap(), None, "not built behind the other's back");
            // It stops without having built it.
            conn.execute("SELECT pg_advisory_unlock($1)", &[&building])
                .await
                .unwrap();
            tokio::time::timeout(Duration::from_secs(30), keeping)
                .await
                .expect("it ends once the index is there")
                .unwrap();
            let valid = PostgresCodeGraphBackend::fill_index_valid(&conn).await;
            assert_eq!(valid.unwrap(), Some(true));
            assert!(PostgresCodeGraphBackend::ensure_fill_index(&pool)
                .await
                .unwrap());
        })
        .await;
    }

    /// A hub that could not build the fill index does not leave it at
    /// that either: it builds it once it can.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_hub_that_could_not_build_the_fill_index_builds_it_once_it_can() {
        use std::time::Duration;
        with_test_pool(PostgresCodeGraphBackend::build_pool, |pool| async move {
            let conn = pool.get().await.unwrap();
            let migrations = MigrationSet::new(
                PostgresCodeGraphBackend::SCHEMA,
                Box::new(EmbeddedCodegraphMigrations) as Box<dyn MigrationRun>,
            );
            MigrationRunner::run(&pool, &[migrations]).await.unwrap();
            // The table it is built on is not there, as in a graph being
            // restored: building fails.
            conn.batch_execute("ALTER TABLE entities RENAME TO entities_away")
                .await
                .unwrap();
            assert!(PostgresCodeGraphBackend::ensure_fill_index(&pool)
                .await
                .is_err());
            let keeping = tokio::spawn(PostgresCodeGraphBackend::keep_fill_index(
                pool.clone(),
                Duration::from_millis(20),
            ));
            tokio::time::sleep(Duration::from_millis(300)).await;
            assert!(!keeping.is_finished(), "it goes on trying");
            conn.batch_execute("ALTER TABLE entities_away RENAME TO entities")
                .await
                .unwrap();
            tokio::time::timeout(Duration::from_secs(30), keeping)
                .await
                .expect("it ends once the index is built")
                .unwrap();
            let valid = PostgresCodeGraphBackend::fill_index_valid(&conn).await;
            assert_eq!(valid.unwrap(), Some(true));
        })
        .await;
    }

    /// The codegraph migrations up to `version` only, as an older hub
    /// left the schema.
    struct MigratedUpTo(u32);

    #[async_trait::async_trait]
    impl MigrationRun for MigratedUpTo {
        async fn run(&self, client: &mut tokio_postgres::Client) -> Result<(), PostgresError> {
            embedded::migrations::runner()
                .set_target(refinery::Target::Version(self.0))
                .run_async(client)
                .await
                .map_err(PostgresError::from)?;
            Ok(())
        }
    }

    /// V007: what fills and pending files a graph held before they were
    /// kept per vector database are vellev's, every column kept; each
    /// table's key then takes the database, and nothing is written later
    /// without naming one.
    #[tokio::test(flavor = "multi_thread")]
    async fn v007_keys_vector_state_by_backend_and_labels_what_was_there_vellevs() {
        with_test_pool(PostgresCodeGraphBackend::build_pool, |pool| async move {
            let older = MigrationSet::new(
                PostgresCodeGraphBackend::SCHEMA,
                Box::new(MigratedUpTo(6)) as Box<dyn MigrationRun>,
            );
            MigrationRunner::run(&pool, &[older]).await.unwrap();
            let conn = pool.get().await.unwrap();
            conn.batch_execute(
                "INSERT INTO vector_fill (repo_id, generation, collection, stage, cursor, done, total)
                 VALUES ('r', 'g1', 'making-1', 'entities', 'e7', 7, 9);
                 INSERT INTO vector_pending (repo_id, file_path, seq) VALUES ('r', 'src/a.rs', 3);",
            )
            .await
            .unwrap();
            let backend = PostgresCodeGraphBackend::with_pool(pool.clone())
                .await
                .unwrap();
            let fill = backend.vector_fill("vellev", "r").await.unwrap().unwrap();
            assert_eq!(
                (fill.generation.as_str(), fill.collection.as_str(), fill.cursor.as_str()),
                ("g1", "making-1", "e7")
            );
            assert_eq!((fill.done, fill.total), (7, 9));
            assert_eq!(backend.vector_fill("weaviate", "r").await.unwrap(), None);
            assert_eq!(
                backend.vectors_pending("vellev", "r", 10).await.unwrap(),
                [("src/a.rs".to_string(), 3)]
            );
            assert_eq!(backend.count_vectors_pending("weaviate", "r").await.unwrap(), 0);
            // Keyed by database, with no default.
            conn.batch_execute(
                "INSERT INTO vector_pending (backend, repo_id, file_path)
                 VALUES ('weaviate', 'r', 'src/a.rs')",
            )
            .await
            .unwrap();
            assert!(conn
                .batch_execute(
                    "INSERT INTO vector_pending (backend, repo_id, file_path)
                     VALUES ('vellev', 'r', 'src/a.rs')"
                )
                .await
                .is_err());
            assert!(conn
                .batch_execute("INSERT INTO vector_pending (repo_id, file_path) VALUES ('r', 'src/b.rs')")
                .await
                .is_err());
            assert!(conn
                .batch_execute(
                    "INSERT INTO vector_fill (repo_id, generation, collection, stage)
                     VALUES ('s', 'g', 'm', 'entities')"
                )
                .await
                .is_err());
        })
        .await;
    }

    /// A Postgres code graph is one graph to every hub that opens it: its
    /// identity is made once, by its migration, and read by each.
    #[tokio::test(flavor = "multi_thread")]
    async fn every_hub_that_opens_the_graph_finds_one_identity() {
        with_test_pool(PostgresCodeGraphBackend::build_pool, |pool| async move {
            let opened = |pool| async {
                let backend = PostgresCodeGraphBackend::with_pool(pool)
                    .await
                    .expect("construct backend");
                CodeGraph::from_backend(backend).identity().to_string()
            };
            let (first, second) = (opened(pool.clone()).await, opened(pool).await);
            assert_eq!(first.len(), 32, "{first}");
            assert_eq!(first, second);
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn parity_entity_crud_and_search() {
        with_embedded_pg(|graph| async move {
            graph
                .register_repo("test-repo", "Test", "/tmp/test")
                .await
                .unwrap();

            let fn_a = make_entity("a", "fn_a", EntityKind::Function);
            let fn_b = make_entity("b", "fn_b", EntityKind::Function);
            graph.upsert_entities(&[fn_a.clone(), fn_b]).await.unwrap();

            let got = graph.get_entity(&fn_a.id).await.unwrap().expect("found");
            assert_eq!(got.name, "fn_a");
            assert_eq!(got.visibility, CodeVisibility::Public);

            let by_name = graph
                .find_entities_by_name("test-repo", "fn_a", 10)
                .await
                .unwrap();
            assert_eq!(by_name.len(), 1);

            let in_file = graph
                .find_entities_in_file("test-repo", "src/lib.rs")
                .await
                .unwrap();
            assert_eq!(in_file.len(), 2);

            let search = graph.search_entities("test-repo", "fn_", 10).await.unwrap();
            assert_eq!(search.len(), 2);

            let all = graph.all_entities("test-repo").await.unwrap();
            assert_eq!(all.len(), 2);

            let stats = graph.get_stats("test-repo").await.unwrap();
            assert_eq!(stats.entity_count, 2);
            assert_eq!(stats.relationship_count, 0);
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn parity_relationships_and_bfs() {
        with_embedded_pg(|graph| async move {
            graph
                .register_repo("test-repo", "Test", "/tmp/test")
                .await
                .unwrap();

            // a ← b, c  (b and c call a).
            let fn_a = make_entity("a", "fn_a", EntityKind::Function);
            let fn_b = make_entity("b", "fn_b", EntityKind::Function);
            let fn_c = make_entity("c", "fn_c", EntityKind::Function);
            graph.upsert_entities(&[fn_a, fn_b, fn_c]).await.unwrap();

            let rels = vec![
                Relationship::new(
                    EntityId("a".into()),
                    EntityId("b".into()),
                    RelationshipKind::CalledBy,
                    1.0,
                ),
                Relationship::new(
                    EntityId("a".into()),
                    EntityId("c".into()),
                    RelationshipKind::CalledBy,
                    1.0,
                ),
            ];
            graph.upsert_relationships(&rels).await.unwrap();

            let callers = graph.callers_of(&EntityId("a".into()), 2).await.unwrap();
            assert_eq!(callers.len(), 2);

            let count = graph
                .count_relationships_by_kind("test-repo", "called_by")
                .await
                .unwrap();
            assert_eq!(count, 2);

            let dependents = graph.count_dependents(&EntityId("a".into())).await.unwrap();
            // called_by edges from 'a' target 'b' and 'c', so 'a'
            // itself has zero dependents under the count_dependents
            // definition (source_id = a, target_id = a is empty).
            assert_eq!(dependents, 0);
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn parity_summaries_and_staleness() {
        with_embedded_pg(|graph| async move {
            graph
                .register_repo("test-repo", "Test", "/tmp/test")
                .await
                .unwrap();
            let fn_a = make_entity("a", "fn_a", EntityKind::Function);
            graph
                .upsert_entities(std::slice::from_ref(&fn_a))
                .await
                .unwrap();

            graph
                .upsert_summary(
                    &fn_a.id,
                    "signature",
                    "fn fn_a()",
                    "src-hash",
                    "claude-opus",
                )
                .await
                .unwrap();

            let (text, stale) = graph
                .get_summary(&fn_a.id, "signature")
                .await
                .unwrap()
                .expect("summary");
            assert_eq!(text, "fn fn_a()");
            assert!(!stale);

            assert_eq!(graph.count_stale_summaries("test-repo").await.unwrap(), 0);
            graph
                .mark_summaries_stale(std::slice::from_ref(&fn_a.id))
                .await
                .unwrap();
            assert_eq!(graph.count_stale_summaries("test-repo").await.unwrap(), 1);
            let stale_list = graph.get_stale_summaries("test-repo").await.unwrap();
            assert_eq!(stale_list.len(), 1);
            assert_eq!(stale_list[0].1, "signature");
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn parity_file_index_and_index_state() {
        with_embedded_pg(|graph| async move {
            graph
                .register_repo("test-repo", "Test", "/tmp/test")
                .await
                .unwrap();

            graph
                .update_file_index("test-repo", "src/lib.rs", "hash-v1", "rust", 5)
                .await
                .unwrap();
            assert_eq!(
                graph
                    .get_file_hash("test-repo", "src/lib.rs")
                    .await
                    .unwrap()
                    .as_deref(),
                Some("hash-v1")
            );

            graph
                .update_file_index("test-repo", "src/lib.rs", "hash-v2", "rust", 7)
                .await
                .unwrap();
            assert_eq!(
                graph
                    .get_file_hash("test-repo", "src/lib.rs")
                    .await
                    .unwrap()
                    .as_deref(),
                Some("hash-v2"),
                "update_file_index should overwrite, not append"
            );

            graph
                .update_index_state_full("test-repo", "abc123", 10, 20)
                .await
                .unwrap();
            let state = graph
                .get_index_state("test-repo")
                .await
                .unwrap()
                .expect("state");
            assert_eq!(state.last_commit.as_deref(), Some("abc123"));
            assert_eq!(state.total_entities, 10);
            assert_eq!(state.total_relationships, 20);
            assert!(!state.indexing_in_progress);

            crate::graph::tests::vectors_complete_in_is_kept_per_repo(&graph, "test-repo").await;
            crate::graph::tests::fills_are_kept_per_repo(&graph).await;
            crate::graph::tests::vector_state_is_kept_per_backend(&graph).await;

            graph
                .set_indexing_in_progress("test-repo", true)
                .await
                .unwrap();
            let state = graph.get_index_state("test-repo").await.unwrap().unwrap();
            assert!(state.indexing_in_progress);
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn parity_delete_repo_data_cascades() {
        with_embedded_pg(|graph| async move {
            graph
                .register_repo("test-repo", "Test", "/tmp/test")
                .await
                .unwrap();

            let fn_a = make_entity("a", "fn_a", EntityKind::Function);
            let fn_b = make_entity("b", "fn_b", EntityKind::Function);
            graph.upsert_entities(&[fn_a.clone(), fn_b]).await.unwrap();
            graph
                .upsert_relationships(&[Relationship::new(
                    EntityId("a".into()),
                    EntityId("b".into()),
                    RelationshipKind::Calls,
                    1.0,
                )])
                .await
                .unwrap();
            graph
                .upsert_summary(&fn_a.id, "sig", "fn fn_a()", "h", "m")
                .await
                .unwrap();
            graph
                .update_file_index("test-repo", "src/lib.rs", "h", "rust", 2)
                .await
                .unwrap();
            graph
                .update_index_state_full("test-repo", "c1", 2, 1)
                .await
                .unwrap();

            let before = graph.get_stats("test-repo").await.unwrap();
            assert_eq!(before.entity_count, 2);
            assert_eq!(before.relationship_count, 1);
            assert_eq!(before.file_count, 1);

            let deleted = graph.delete_repo_data("test-repo").await.unwrap();
            assert_eq!(deleted, 2);

            let after = graph.get_stats("test-repo").await.unwrap();
            assert_eq!(after.entity_count, 0);
            assert_eq!(after.relationship_count, 0);
            assert_eq!(after.file_count, 0);
            assert!(graph.get_index_state("test-repo").await.unwrap().is_none());
        })
        .await;
    }
}
