//! One-shot SQLite → Postgres migration for the code graph store.
//!
//! Reads every table from a SQLite `codegraph.db` file and writes
//! them into a Postgres backend (after running the refinery
//! migrations against the destination). Driven by the
//! `workforce migrate-codegraph` CLI subcommand.
//!
//! ## Design
//!
//! - **Idempotent.** Every insert uses `ON CONFLICT DO NOTHING`, so
//!   re-running on a partially-migrated destination is safe.
//! - **Preserves IDs.** Entity and relationship IDs are deterministic
//!   (blake3 hashes of `repo:file:kind:name`) so they're stable
//!   across runs; the `ON CONFLICT DO NOTHING` clause skips rows
//!   that were already copied.
//! - **Order matters.** The Postgres schema has foreign keys that
//!   cascade through repositories → entities → (relationships,
//!   summaries, file_index, index_state). We insert in dependency
//!   order with `SET CONSTRAINTS ALL DEFERRED` so parent-child self
//!   references (entities with `parent_id`) resolve after the whole
//!   entities batch is loaded.
//! - **Timestamps.** SQLite stores `datetime('now')` as naive text
//!   without a timezone. Postgres wants `TIMESTAMPTZ`. We parse
//!   with a permissive strategy (try RFC3339 then `YYYY-MM-DD
//!   HH:MM:SS`) and fall back to the current time for rows that
//!   can't be parsed — the timestamp is metadata, not load-bearing.

use std::path::Path;
use std::sync::Arc;

use chrono::{DateTime, NaiveDateTime, Utc};
use eyre::{Context, Result};
use rusqlite::{Connection, OpenFlags};
use workforce_postgres::{PostgresConfig, PostgresPool};

use crate::graph::postgres::PostgresCodeGraphBackend;

#[derive(Debug, Default, Clone)]
pub struct CodeGraphMigrationReport {
    pub repositories: usize,
    pub entities: usize,
    pub relationships: usize,
    pub summaries: usize,
    pub file_index: usize,
    pub index_state: usize,
    /// Whether the graph's identity and what it records of its code
    /// vectors came across: which collection each repo's are complete in,
    /// how far each fill has got, the files still to put right. They do
    /// when the destination held no repository before, so that it becomes
    /// the very graph the source was, and what a vector database keeps
    /// for that graph is kept for it still: nothing is embedded again.
    /// Into a destination that already was a graph of its own they do
    /// not: it stays that graph, and the repos copied in are filled.
    pub vectors_carried: bool,
    pub vector_fills: usize,
    pub vector_pending: usize,
}

impl CodeGraphMigrationReport {
    pub fn total(&self) -> usize {
        self.repositories
            + self.entities
            + self.relationships
            + self.summaries
            + self.file_index
            + self.index_state
    }
}

/// Migrate a SQLite code graph database into a Postgres backend.
pub async fn migrate_sqlite_to_postgres(
    sqlite_path: &Path,
    pg_config: &PostgresConfig,
) -> Result<CodeGraphMigrationReport> {
    let pool = PostgresCodeGraphBackend::build_pool(pg_config)
        .context("building codegraph pool for migration destination")?;
    migrate_sqlite_to_postgres_with_pool(sqlite_path, pool).await
}

/// Variant taking a pre-built pool whose `search_path` is already
/// pinned to the `codegraph` schema. Used by tests.
pub async fn migrate_sqlite_to_postgres_with_pool(
    sqlite_path: &Path,
    pool: PostgresPool,
) -> Result<CodeGraphMigrationReport> {
    // Run destination migrations first.
    let _store: Arc<PostgresCodeGraphBackend> = PostgresCodeGraphBackend::with_pool(pool.clone())
        .await
        .context("preparing Postgres codegraph destination")?;

    let sqlite = Connection::open_with_flags(sqlite_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("opening SQLite source at {}", sqlite_path.display()))?;

    let repos = read_repositories(&sqlite)?;
    let entities = read_entities(&sqlite)?;
    let relationships = read_relationships(&sqlite)?;
    let summaries = read_summaries(&sqlite)?;
    let file_index = read_file_index(&sqlite)?;
    let index_state = read_index_state(&sqlite)?;
    let vectors = read_vector_state(&sqlite)?;
    drop(sqlite);

    // A destination that holds nothing yet becomes the graph the source
    // was (and is, when this is run again); one that is a graph of its
    // own already stays itself.
    let fresh = {
        let conn = pool.get().await?;
        let held: i64 = conn
            .query_one("SELECT count(*) FROM repositories", &[])
            .await?
            .get(0);
        let is: String = conn
            .query_one("SELECT id FROM graph_identity", &[])
            .await?
            .get(0);
        held == 0 || vectors.as_ref().is_some_and(|source| source.identity == is)
    };
    let mut report = CodeGraphMigrationReport {
        repositories: write_repositories(&pool, repos).await?,
        entities: write_entities(&pool, entities).await?,
        relationships: write_relationships(&pool, relationships).await?,
        summaries: write_summaries(&pool, summaries).await?,
        file_index: write_file_index(&pool, file_index).await?,
        index_state: write_index_state(&pool, index_state).await?,
        vectors_carried: false,
        vector_fills: 0,
        vector_pending: 0,
    };
    if let (true, Some(vectors)) = (fresh, vectors) {
        (report.vector_fills, report.vector_pending) = write_vector_state(&pool, vectors).await?;
        report.vectors_carried = true;
    }
    Ok(report)
}

// ─── identity and vector state ─────────────────────────────────────────

/// What a graph records of its code vectors, and which graph it is.
struct VectorState {
    identity: String,
    /// `(repo, collection)` for each repo whose vectors are complete.
    complete_in: Vec<(String, String)>,
    /// `(backend, repo, generation, collection, stage, cursor, done, total)`.
    fills: Vec<(String, String, String, String, String, String, i64, i64)>,
    /// `(backend, repo, file, seq)`.
    pending: Vec<(String, String, String, i64)>,
}

/// The source's identity and vector state, or `None` for a source from
/// before graphs had an identity: there is then nothing kept for it
/// anywhere to carry across.
fn read_vector_state(sqlite: &Connection) -> Result<Option<VectorState>> {
    let has = |table: &str| -> Result<bool> {
        Ok(sqlite
            .prepare("SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1")?
            .exists([table])?)
    };
    if !has("graph_identity")? {
        return Ok(None);
    }
    let identity: String = sqlite.query_row("SELECT id FROM graph_identity", [], |r| r.get(0))?;
    let complete_in = sqlite
        .prepare(
            "SELECT repo_id, vectors_complete_in FROM index_state \
             WHERE vectors_complete_in IS NOT NULL",
        )?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    // Which database each row is of: as the source says, or, in a source
    // from before they were kept per database (schema v6), vellev's, as
    // the migration of the source would label them.
    let backend = |table: &str| -> Result<&'static str> {
        let keyed = sqlite
            .prepare(&format!(
                "SELECT 1 FROM pragma_table_info('{table}') WHERE name = 'backend'"
            ))?
            .exists([])?;
        Ok(if keyed { "backend" } else { "'vellev'" })
    };
    let (mut fills, mut pending) = (Vec::new(), Vec::new());
    if has("vector_fill")? {
        fills = sqlite
            .prepare(&format!(
                "SELECT {}, repo_id, generation, collection, stage, cursor, done, total \
                 FROM vector_fill",
                backend("vector_fill")?
            ))?
            .query_map([], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                    r.get(7)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
    }
    if has("vector_pending")? {
        pending = sqlite
            .prepare(&format!(
                "SELECT {}, repo_id, file_path, seq FROM vector_pending",
                backend("vector_pending")?
            ))?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
    }
    Ok(Some(VectorState {
        identity,
        complete_in,
        fills,
        pending,
    }))
}

/// Make the destination the graph the source was, and record of its code
/// vectors what the source did. Returns how many fills and pending files
/// were written.
async fn write_vector_state(pool: &PostgresPool, vectors: VectorState) -> Result<(usize, usize)> {
    let conn = pool.get().await?;
    conn.execute("UPDATE graph_identity SET id = $1", &[&vectors.identity])
        .await
        .context("carry the graph's identity across")?;
    for (repo, collection) in &vectors.complete_in {
        conn.execute(
            "UPDATE index_state SET vectors_complete_in = $2 WHERE repo_id = $1",
            &[repo, collection],
        )
        .await
        .context("carry vectors_complete_in across")?;
    }
    for (backend, repo, generation, collection, stage, cursor, done, total) in &vectors.fills {
        conn.execute(
            "INSERT INTO vector_fill
                 (backend, repo_id, generation, collection, stage, cursor, done, total)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
             ON CONFLICT (backend, repo_id) DO NOTHING",
            &[
                backend, repo, generation, collection, stage, cursor, done, total,
            ],
        )
        .await
        .context("insert vector_fill row")?;
    }
    for (backend, repo, file, seq) in &vectors.pending {
        conn.execute(
            "INSERT INTO vector_pending (backend, repo_id, file_path, seq) VALUES ($1, $2, $3, $4)
             ON CONFLICT (backend, repo_id, file_path) DO NOTHING",
            &[backend, repo, file, seq],
        )
        .await
        .context("insert vector_pending row")?;
    }
    Ok((vectors.fills.len(), vectors.pending.len()))
}

// ─── Helpers ───────────────────────────────────────────────────────────

/// Parse a timestamp that could be either RFC3339 or
/// `YYYY-MM-DD HH:MM:SS` (SQLite's `datetime('now')` format).
fn parse_ts(s: &str) -> DateTime<Utc> {
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return dt.with_timezone(&Utc);
    }
    if let Ok(naive) = NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S") {
        return DateTime::<Utc>::from_naive_utc_and_offset(naive, Utc);
    }
    Utc::now()
}

fn parse_ts_opt(s: Option<String>) -> Option<DateTime<Utc>> {
    s.as_deref().map(parse_ts)
}

fn parse_json(s: Option<String>) -> Option<serde_json::Value> {
    s.and_then(|text| serde_json::from_str(&text).ok())
}

// ─── repositories ──────────────────────────────────────────────────────

struct RepoRow {
    id: String,
    name: String,
    root_path: String,
    last_indexed_commit: Option<String>,
    last_indexed_at: Option<DateTime<Utc>>,
    created_at: DateTime<Utc>,
}

fn read_repositories(sqlite: &Connection) -> Result<Vec<RepoRow>> {
    let mut stmt = sqlite.prepare(
        "SELECT id, name, root_path, last_indexed_commit, last_indexed_at, created_at
         FROM repositories",
    )?;
    let rows = stmt
        .query_map([], |r| {
            let last_indexed_at: Option<String> = r.get(4)?;
            let created_at: String = r.get(5)?;
            Ok(RepoRow {
                id: r.get(0)?,
                name: r.get(1)?,
                root_path: r.get(2)?,
                last_indexed_commit: r.get(3)?,
                last_indexed_at: parse_ts_opt(last_indexed_at),
                created_at: parse_ts(&created_at),
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

async fn write_repositories(pool: &PostgresPool, rows: Vec<RepoRow>) -> Result<usize> {
    if rows.is_empty() {
        return Ok(0);
    }
    let conn = pool.get().await?;
    for r in &rows {
        conn.execute(
            "INSERT INTO repositories
             (id, name, root_path, last_indexed_commit, last_indexed_at, created_at)
             VALUES ($1, $2, $3, $4, $5, $6)
             ON CONFLICT (id) DO NOTHING",
            &[
                &r.id,
                &r.name,
                &r.root_path,
                &r.last_indexed_commit,
                &r.last_indexed_at,
                &r.created_at,
            ],
        )
        .await
        .context("insert repositories row")?;
    }
    Ok(rows.len())
}

// ─── entities ──────────────────────────────────────────────────────────

struct EntityRow {
    id: String,
    repo_id: String,
    kind: String,
    name: String,
    qualified_name: String,
    file_path: String,
    start_line: i64,
    end_line: i64,
    signature: Option<String>,
    visibility: String,
    parent_id: Option<String>,
    doc_comment: Option<String>,
    content_hash: String,
    language: String,
    indexed_at: DateTime<Utc>,
}

fn read_entities(sqlite: &Connection) -> Result<Vec<EntityRow>> {
    let mut stmt = sqlite.prepare(
        "SELECT id, repo_id, kind, name, qualified_name, file_path, start_line, end_line,
                signature, visibility, parent_id, doc_comment, content_hash, language, indexed_at
         FROM entities",
    )?;
    let rows = stmt
        .query_map([], |r| {
            let indexed_at: String = r.get(14)?;
            Ok(EntityRow {
                id: r.get(0)?,
                repo_id: r.get(1)?,
                kind: r.get(2)?,
                name: r.get(3)?,
                qualified_name: r.get(4)?,
                file_path: r.get(5)?,
                start_line: r.get(6)?,
                end_line: r.get(7)?,
                signature: r.get(8)?,
                visibility: r.get(9).unwrap_or_else(|_| "private".to_string()),
                parent_id: r.get(10)?,
                doc_comment: r.get(11)?,
                content_hash: r.get(12)?,
                language: r.get(13)?,
                indexed_at: parse_ts(&indexed_at),
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

async fn write_entities(pool: &PostgresPool, rows: Vec<EntityRow>) -> Result<usize> {
    if rows.is_empty() {
        return Ok(0);
    }
    let mut conn = pool.get().await?;
    let tx = conn.transaction().await?;
    // Defer self-FK and repo FK so children + parents can load in
    // any order within the same transaction.
    tx.execute("SET CONSTRAINTS ALL DEFERRED", &[]).await?;
    for r in &rows {
        tx.execute(
            "INSERT INTO entities
             (id, repo_id, kind, name, qualified_name, file_path, start_line, end_line,
              signature, visibility, parent_id, doc_comment, content_hash, language, indexed_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15)
             ON CONFLICT (id) DO NOTHING",
            &[
                &r.id,
                &r.repo_id,
                &r.kind,
                &r.name,
                &r.qualified_name,
                &r.file_path,
                &r.start_line,
                &r.end_line,
                &r.signature,
                &r.visibility,
                &r.parent_id,
                &r.doc_comment,
                &r.content_hash,
                &r.language,
                &r.indexed_at,
            ],
        )
        .await
        .context("insert entities row")?;
    }
    tx.commit().await?;
    Ok(rows.len())
}

// ─── relationships ─────────────────────────────────────────────────────

struct RelationshipRow {
    id: String,
    source_id: String,
    target_id: String,
    kind: String,
    confidence: f64,
    metadata: Option<serde_json::Value>,
    indexed_at: DateTime<Utc>,
}

fn read_relationships(sqlite: &Connection) -> Result<Vec<RelationshipRow>> {
    let mut stmt = sqlite.prepare(
        "SELECT id, source_id, target_id, kind, confidence, metadata, indexed_at
         FROM relationships",
    )?;
    let rows = stmt
        .query_map([], |r| {
            let indexed_at: String = r.get(6)?;
            let metadata: Option<String> = r.get(5)?;
            let confidence: f64 = r.get::<_, f64>(4).unwrap_or(1.0);
            Ok(RelationshipRow {
                id: r.get(0)?,
                source_id: r.get(1)?,
                target_id: r.get(2)?,
                kind: r.get(3)?,
                confidence,
                metadata: parse_json(metadata),
                indexed_at: parse_ts(&indexed_at),
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

async fn write_relationships(pool: &PostgresPool, rows: Vec<RelationshipRow>) -> Result<usize> {
    if rows.is_empty() {
        return Ok(0);
    }
    let mut conn = pool.get().await?;
    let tx = conn.transaction().await?;
    tx.execute("SET CONSTRAINTS ALL DEFERRED", &[]).await?;
    for r in &rows {
        tx.execute(
            "INSERT INTO relationships
             (id, source_id, target_id, kind, confidence, metadata, indexed_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7)
             ON CONFLICT (id) DO NOTHING",
            &[
                &r.id,
                &r.source_id,
                &r.target_id,
                &r.kind,
                &r.confidence,
                &r.metadata,
                &r.indexed_at,
            ],
        )
        .await
        .context("insert relationships row")?;
    }
    tx.commit().await?;
    Ok(rows.len())
}

// ─── summaries ─────────────────────────────────────────────────────────

struct SummaryRow {
    entity_id: String,
    granularity: String,
    text: String,
    source_hash: String,
    model_used: String,
    generated_at: DateTime<Utc>,
    is_stale: bool,
}

fn read_summaries(sqlite: &Connection) -> Result<Vec<SummaryRow>> {
    let mut stmt = sqlite.prepare(
        "SELECT entity_id, granularity, text, source_hash, model_used, generated_at, is_stale
         FROM summaries",
    )?;
    let rows = stmt
        .query_map([], |r| {
            let generated_at: String = r.get(5)?;
            let is_stale_int: i64 = r.get(6)?;
            Ok(SummaryRow {
                entity_id: r.get(0)?,
                granularity: r.get(1)?,
                text: r.get(2)?,
                source_hash: r.get(3)?,
                model_used: r.get(4)?,
                generated_at: parse_ts(&generated_at),
                is_stale: is_stale_int != 0,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

async fn write_summaries(pool: &PostgresPool, rows: Vec<SummaryRow>) -> Result<usize> {
    if rows.is_empty() {
        return Ok(0);
    }
    let conn = pool.get().await?;
    for r in &rows {
        conn.execute(
            "INSERT INTO summaries
             (entity_id, granularity, text, source_hash, model_used, generated_at, is_stale)
             VALUES ($1, $2, $3, $4, $5, $6, $7)
             ON CONFLICT (entity_id, granularity) DO NOTHING",
            &[
                &r.entity_id,
                &r.granularity,
                &r.text,
                &r.source_hash,
                &r.model_used,
                &r.generated_at,
                &r.is_stale,
            ],
        )
        .await
        .context("insert summaries row")?;
    }
    Ok(rows.len())
}

// ─── file_index ────────────────────────────────────────────────────────

struct FileIndexRow {
    repo_id: String,
    file_path: String,
    content_hash: String,
    language: String,
    entity_count: i64,
    last_indexed_at: DateTime<Utc>,
}

fn read_file_index(sqlite: &Connection) -> Result<Vec<FileIndexRow>> {
    let mut stmt = sqlite.prepare(
        "SELECT repo_id, file_path, content_hash, language, entity_count, last_indexed_at
         FROM file_index",
    )?;
    let rows = stmt
        .query_map([], |r| {
            let last_indexed_at: String = r.get(5)?;
            Ok(FileIndexRow {
                repo_id: r.get(0)?,
                file_path: r.get(1)?,
                content_hash: r.get(2)?,
                language: r.get(3)?,
                entity_count: r.get(4)?,
                last_indexed_at: parse_ts(&last_indexed_at),
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

async fn write_file_index(pool: &PostgresPool, rows: Vec<FileIndexRow>) -> Result<usize> {
    if rows.is_empty() {
        return Ok(0);
    }
    let conn = pool.get().await?;
    for r in &rows {
        conn.execute(
            "INSERT INTO file_index
             (repo_id, file_path, content_hash, language, entity_count, last_indexed_at)
             VALUES ($1, $2, $3, $4, $5, $6)
             ON CONFLICT (repo_id, file_path) DO NOTHING",
            &[
                &r.repo_id,
                &r.file_path,
                &r.content_hash,
                &r.language,
                &r.entity_count,
                &r.last_indexed_at,
            ],
        )
        .await
        .context("insert file_index row")?;
    }
    Ok(rows.len())
}

// ─── index_state ───────────────────────────────────────────────────────

struct IndexStateRow {
    repo_id: String,
    last_commit: Option<String>,
    last_full_index_at: Option<DateTime<Utc>>,
    last_incremental_at: Option<DateTime<Utc>>,
    total_entities: i64,
    total_relationships: i64,
    indexing_in_progress: bool,
}

fn read_index_state(sqlite: &Connection) -> Result<Vec<IndexStateRow>> {
    let mut stmt = sqlite.prepare(
        "SELECT repo_id, last_commit, last_full_index_at, last_incremental_at,
                total_entities, total_relationships, indexing_in_progress
         FROM index_state",
    )?;
    let rows = stmt
        .query_map([], |r| {
            let last_full_index_at: Option<String> = r.get(2)?;
            let last_incremental_at: Option<String> = r.get(3)?;
            let in_progress: i64 = r.get(6)?;
            Ok(IndexStateRow {
                repo_id: r.get(0)?,
                last_commit: r.get(1)?,
                last_full_index_at: parse_ts_opt(last_full_index_at),
                last_incremental_at: parse_ts_opt(last_incremental_at),
                total_entities: r.get(4)?,
                total_relationships: r.get(5)?,
                indexing_in_progress: in_progress != 0,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

async fn write_index_state(pool: &PostgresPool, rows: Vec<IndexStateRow>) -> Result<usize> {
    if rows.is_empty() {
        return Ok(0);
    }
    let conn = pool.get().await?;
    for r in &rows {
        conn.execute(
            "INSERT INTO index_state
             (repo_id, last_commit, last_full_index_at, last_incremental_at,
              total_entities, total_relationships, indexing_in_progress)
             VALUES ($1, $2, $3, $4, $5, $6, $7)
             ON CONFLICT (repo_id) DO NOTHING",
            &[
                &r.repo_id,
                &r.last_commit,
                &r.last_full_index_at,
                &r.last_incremental_at,
                &r.total_entities,
                &r.total_relationships,
                &r.indexing_in_progress,
            ],
        )
        .await
        .context("insert index_state row")?;
    }
    Ok(rows.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::entities::{CodeEntity, CodeVisibility, EntityId, EntityKind, Language};
    use crate::graph::postgres::PostgresCodeGraphBackend;
    use crate::graph::relationships::{Relationship, RelationshipKind};
    use crate::graph::{CodeGraph, CodeGraphBackend};
    use workforce_postgres::test_support::with_test_pool;

    fn make_entity(id: &str, name: &str) -> CodeEntity {
        CodeEntity {
            id: EntityId(id.to_string()),
            repo_id: "test-repo".into(),
            kind: EntityKind::Function,
            name: name.into(),
            qualified_name: format!("test::{name}"),
            file_path: "src/lib.rs".into(),
            start_line: 1,
            end_line: 10,
            signature: Some(format!("fn {name}()")),
            visibility: CodeVisibility::Public,
            parent_id: None,
            doc_comment: None,
            content_hash: "h".into(),
            language: Language::Rust,
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn migrates_codegraph_round_trip() {
        with_test_pool(PostgresCodeGraphBackend::build_pool, |pool| async move {
            let tmp = tempfile::NamedTempFile::new().unwrap();
            let sqlite = CodeGraph::open(tmp.path()).unwrap();
            sqlite
                .register_repo("test-repo", "Test", "/tmp/test")
                .await
                .unwrap();
            let fn_a = make_entity("a", "fn_a");
            let fn_b = make_entity("b", "fn_b");
            sqlite.upsert_entities(&[fn_a.clone(), fn_b]).await.unwrap();
            sqlite
                .upsert_relationships(&[Relationship::new(
                    EntityId("a".into()),
                    EntityId("b".into()),
                    RelationshipKind::Calls,
                    1.0,
                )])
                .await
                .unwrap();
            sqlite
                .upsert_summary(&fn_a.id, "sig", "fn fn_a()", "h", "m")
                .await
                .unwrap();
            sqlite
                .update_file_index("test-repo", "src/lib.rs", "h", "rust", 2)
                .await
                .unwrap();
            sqlite
                .update_index_state_full("test-repo", "c1", 2, 1)
                .await
                .unwrap();
            // What it records of its code vectors, and which graph it is.
            let identity = sqlite.identity().to_string();
            let fill = crate::graph::vectors::VectorFill {
                generation: "g1".into(),
                collection: "making-1".into(),
                stage: crate::graph::vectors::FillStage::Done,
                cursor: String::new(),
                done: 2,
                total: 2,
            };
            assert!(sqlite
                .advance_vector_fill("test-repo", None, &fill)
                .await
                .unwrap());
            sqlite
                .set_vectors_complete_in("test-repo", Some("making-1"))
                .await
                .unwrap();
            sqlite
                .note_vectors_pending("test-repo", &["src/lib.rs".to_string()])
                .await
                .unwrap();
            drop(sqlite);

            let report = migrate_sqlite_to_postgres_with_pool(tmp.path(), pool.clone())
                .await
                .expect("migration");
            assert_eq!(report.repositories, 1);
            assert_eq!(report.entities, 2);
            assert_eq!(report.relationships, 1);
            assert_eq!(report.summaries, 1);
            assert_eq!(report.file_index, 1);
            assert_eq!(report.index_state, 1);
            assert!(report.vectors_carried);
            assert_eq!((report.vector_fills, report.vector_pending), (1, 1));

            let dest = PostgresCodeGraphBackend::with_pool(pool.clone())
                .await
                .unwrap();
            let stats = dest.get_stats("test-repo").await.unwrap();
            assert_eq!(stats.entity_count, 2);
            assert_eq!(stats.relationship_count, 1);
            assert_eq!(stats.file_count, 1);
            assert!(dest.get_summary(&fn_a.id, "sig").await.unwrap().is_some());

            // The destination is the graph the source was, and records of
            // its code vectors what the source did: whatever keeps them
            // for that graph keeps them for this one, and nothing is
            // filled again.
            let graph = CodeGraph::from_backend(dest);
            assert_eq!(graph.identity(), identity);
            assert_eq!(
                graph
                    .vectors_complete_in("test-repo")
                    .await
                    .unwrap()
                    .as_deref(),
                Some("making-1")
            );
            assert_eq!(graph.vector_fill("test-repo").await.unwrap(), Some(fill));
            assert_eq!(graph.count_vectors_pending("test-repo").await.unwrap(), 1);

            // Idempotency: re-running on the same destination is a no-op.
            let report2 = migrate_sqlite_to_postgres_with_pool(tmp.path(), pool.clone())
                .await
                .expect("idempotent");
            assert_eq!(report2.total(), report.total());
            assert!(report2.vectors_carried, "the same graph still");

            // Another graph copied into it does not make it that graph:
            // it stays the one it is, and the other's repos are filled.
            let other = tempfile::NamedTempFile::new().unwrap();
            let another = CodeGraph::open(other.path()).unwrap();
            another
                .register_repo("other-repo", "Other", "/tmp/other")
                .await
                .unwrap();
            another
                .update_index_state_full("other-repo", "c1", 0, 0)
                .await
                .unwrap();
            another
                .set_vectors_complete_in("other-repo", Some("making-9"))
                .await
                .unwrap();
            assert_ne!(another.identity(), identity);
            drop(another);
            let report3 = migrate_sqlite_to_postgres_with_pool(other.path(), pool.clone())
                .await
                .unwrap();
            assert!(!report3.vectors_carried);
            let dest = PostgresCodeGraphBackend::with_pool(pool).await.unwrap();
            let graph = CodeGraph::from_backend(dest);
            assert_eq!(graph.identity(), identity);
            assert_eq!(graph.vectors_complete_in("other-repo").await.unwrap(), None);
        })
        .await;
    }

    /// What a source graph keeps for each vector database is carried
    /// across as that database's; and a source from before it was kept per
    /// database (schema v6), read only and so never migrated, has its rows
    /// carried as vellev's, as its own migration would label them.
    #[tokio::test(flavor = "multi_thread")]
    async fn vector_state_is_carried_per_backend_and_an_older_sources_as_vellevs() {
        use crate::graph::vectors::{FillStage, VectorFill};
        use crate::graph::VectorKeys;
        let fill = |generation: &str| VectorFill {
            generation: generation.into(),
            collection: "making-1".into(),
            stage: FillStage::Entities,
            cursor: "e1".into(),
            done: 1,
            total: 2,
        };
        // A source keyed by database, with state of both.
        with_test_pool(PostgresCodeGraphBackend::build_pool, |pool| async move {
            let tmp = tempfile::NamedTempFile::new().unwrap();
            let sqlite = CodeGraph::open(tmp.path()).unwrap();
            sqlite.register_repo("r", "R", "/tmp/r").await.unwrap();
            let store = sqlite.backend();
            for (backend, generation) in [("vellev", "v1"), ("weaviate", "w1")] {
                assert!(store
                    .advance_vector_fill(backend, "r", None, &fill(generation))
                    .await
                    .unwrap());
            }
            store
                .note_vectors_pending(&["weaviate"], "r", &["src/w.rs".to_string()])
                .await
                .unwrap();
            store
                .note_vectors_pending(&["vellev", "weaviate"], "r", &["src/v.rs".to_string()])
                .await
                .unwrap();
            drop(sqlite);
            let report = migrate_sqlite_to_postgres_with_pool(tmp.path(), pool.clone())
                .await
                .unwrap();
            assert_eq!((report.vector_fills, report.vector_pending), (2, 3));
            let dest = PostgresCodeGraphBackend::with_pool(pool).await.unwrap();
            assert_eq!(
                dest.vector_fill(VectorKeys::VELLEV, "r").await.unwrap(),
                Some(fill("v1"))
            );
            assert_eq!(
                dest.vector_fill(VectorKeys::WEAVIATE, "r").await.unwrap(),
                Some(fill("w1"))
            );
            assert_eq!(
                dest.vectors_pending(VectorKeys::VELLEV, "r", 10)
                    .await
                    .unwrap(),
                [("src/v.rs".to_string(), 1)]
            );
            assert_eq!(
                dest.vectors_pending(VectorKeys::WEAVIATE, "r", 10)
                    .await
                    .unwrap(),
                [("src/v.rs".to_string(), 1), ("src/w.rs".to_string(), 1)]
            );
        })
        .await;
        // A source as schema v5 left it: no database named.
        with_test_pool(PostgresCodeGraphBackend::build_pool, |pool| async move {
            let tmp = tempfile::NamedTempFile::new().unwrap();
            let sqlite = CodeGraph::open(tmp.path()).unwrap();
            sqlite.register_repo("r", "R", "/tmp/r").await.unwrap();
            drop(sqlite);
            {
                let conn = Connection::open(tmp.path()).unwrap();
                conn.execute_batch(
                    "DROP TABLE vector_fill;
                     DROP TABLE vector_pending;
                     CREATE TABLE vector_fill (
                         repo_id TEXT PRIMARY KEY, generation TEXT NOT NULL,
                         collection TEXT NOT NULL, stage TEXT NOT NULL,
                         cursor TEXT NOT NULL DEFAULT '', done INTEGER NOT NULL DEFAULT 0,
                         total INTEGER NOT NULL DEFAULT 0);
                     CREATE TABLE vector_pending (
                         repo_id TEXT NOT NULL, file_path TEXT NOT NULL,
                         seq INTEGER NOT NULL DEFAULT 1, PRIMARY KEY (repo_id, file_path));
                     INSERT INTO vector_fill (repo_id, generation, collection, stage, cursor, done, total)
                     VALUES ('r', 'v1', 'making-1', 'entities', 'e1', 1, 2);
                     INSERT INTO vector_pending (repo_id, file_path, seq) VALUES ('r', 'src/v.rs', 4);
                     PRAGMA user_version = 5;",
                )
                .unwrap();
            }
            let report = migrate_sqlite_to_postgres_with_pool(tmp.path(), pool.clone())
                .await
                .unwrap();
            assert_eq!((report.vector_fills, report.vector_pending), (1, 1));
            let dest = PostgresCodeGraphBackend::with_pool(pool).await.unwrap();
            assert_eq!(
                dest.vector_fill(VectorKeys::VELLEV, "r").await.unwrap(),
                Some(fill("v1"))
            );
            assert_eq!(
                dest.vectors_pending(VectorKeys::VELLEV, "r", 10).await.unwrap(),
                [("src/v.rs".to_string(), 4)]
            );
            assert_eq!(dest.count_vectors_pending(VectorKeys::WEAVIATE, "r").await.unwrap(), 0);
        })
        .await;
    }
}
