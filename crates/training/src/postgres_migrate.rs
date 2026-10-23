//! One-shot SQLite → Postgres migration for the training store.
//!
//! Reads the `training_corpora`, `training_documents`, and
//! `training_chunks` tables from a SQLite training database file and
//! writes them into a Postgres backend (after running the refinery
//! migrations against the destination). Driven by the
//! `workforce migrate-training` CLI subcommand.
//!
//! Idempotent — every insert uses `ON CONFLICT (id) DO NOTHING`, so
//! re-running on a partially-migrated destination is safe.
//!
//! Enum/scope columns pass through as raw strings: the model parsers
//! coerce unknown values to a default, so round-tripping through the
//! typed model would silently rewrite any unrecognized stored value.

use std::path::Path;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use eyre::{Context, Result};
use rusqlite::{Connection, OpenFlags};
use uuid::Uuid;
use workforce_postgres::{PostgresConfig, PostgresPool};

use crate::postgres::PostgresTrainingStore;

#[derive(Debug, Default, Clone)]
pub struct TrainingMigrationReport {
    pub corpora: usize,
    pub documents: usize,
    pub chunks: usize,
}

impl TrainingMigrationReport {
    pub fn total(&self) -> usize {
        self.corpora + self.documents + self.chunks
    }
}

/// Migrate a SQLite training database into a Postgres backend.
pub async fn migrate_sqlite_to_postgres(
    sqlite_path: &Path,
    pg_config: &PostgresConfig,
) -> Result<TrainingMigrationReport> {
    let pool = PostgresTrainingStore::build_pool(pg_config)
        .context("building training-store pool for migration destination")?;
    migrate_sqlite_to_postgres_with_pool(sqlite_path, pool).await
}

/// Variant taking a pre-built pool. Pool MUST have its `search_path`
/// pinned to the training schema (use [`PostgresTrainingStore::build_pool`]).
pub async fn migrate_sqlite_to_postgres_with_pool(
    sqlite_path: &Path,
    pool: PostgresPool,
) -> Result<TrainingMigrationReport> {
    // Run destination migrations first.
    let _store: Arc<PostgresTrainingStore> = PostgresTrainingStore::with_pool(pool.clone())
        .await
        .context("preparing Postgres training destination")?;

    let sqlite = Connection::open_with_flags(sqlite_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("opening SQLite source at {}", sqlite_path.display()))?;

    let corpora = read_corpora(&sqlite)?;
    let documents = read_documents(&sqlite)?;
    let chunks = read_chunks(&sqlite)?;
    drop(sqlite);

    // Parent-first write order honours the destination foreign keys
    // (documents → corpora, chunks → documents).
    let corpus_count = write_corpora(&pool, corpora).await?;
    let document_count = write_documents(&pool, documents).await?;
    let chunk_count = write_chunks(&pool, chunks).await?;

    Ok(TrainingMigrationReport {
        corpora: corpus_count,
        documents: document_count,
        chunks: chunk_count,
    })
}

struct CorpusMigRow {
    id: Uuid,
    agent_id: String,
    name: String,
    scope_kind: String,
    scope_id: Option<String>,
    created_at: DateTime<Utc>,
    created_by: String,
}

fn read_corpora(sqlite: &Connection) -> Result<Vec<CorpusMigRow>> {
    let mut stmt = sqlite.prepare(
        "SELECT id, agent_id, name, scope_kind, scope_id, created_at, created_by
         FROM training_corpora",
    )?;
    let rows = stmt
        .query_map([], |r| {
            let id_str: String = r.get(0)?;
            let created_at_str: String = r.get(5)?;
            Ok(CorpusMigRow {
                id: Uuid::parse_str(&id_str).unwrap_or_else(|_| Uuid::new_v4()),
                agent_id: r.get(1)?,
                name: r.get(2)?,
                scope_kind: r.get(3)?,
                scope_id: r.get(4)?,
                created_at: DateTime::parse_from_rfc3339(&created_at_str)
                    .map(|d| d.with_timezone(&Utc))
                    .unwrap_or_else(|_| Utc::now()),
                created_by: r.get(6)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

async fn write_corpora(pool: &PostgresPool, rows: Vec<CorpusMigRow>) -> Result<usize> {
    if rows.is_empty() {
        return Ok(0);
    }
    let conn = pool.get().await?;
    for r in &rows {
        conn.execute(
            "INSERT INTO training_corpora
             (id, agent_id, name, scope_kind, scope_id, created_at, created_by)
             VALUES ($1, $2, $3, $4, $5, $6, $7)
             ON CONFLICT (id) DO NOTHING",
            &[
                &r.id,
                &r.agent_id,
                &r.name,
                &r.scope_kind,
                &r.scope_id,
                &r.created_at,
                &r.created_by,
            ],
        )
        .await
        .context("insert training_corpora row")?;
    }
    Ok(rows.len())
}

struct DocumentMigRow {
    id: Uuid,
    corpus_id: Uuid,
    original_filename: String,
    mime_type: String,
    size_bytes: i64,
    storage_uri: String,
    source_kind: String,
    source_ref: Option<String>,
    processing_status: String,
    processing_error: Option<String>,
    uploaded_at: DateTime<Utc>,
    uploaded_by: String,
    page_count: Option<i32>,
}

fn read_documents(sqlite: &Connection) -> Result<Vec<DocumentMigRow>> {
    let mut stmt = sqlite.prepare(
        "SELECT id, corpus_id, original_filename, mime_type, size_bytes, storage_uri,
                source_kind, source_ref, processing_status, processing_error,
                uploaded_at, uploaded_by, page_count
         FROM training_documents",
    )?;
    let rows = stmt
        .query_map([], |r| {
            let id_str: String = r.get(0)?;
            let corpus_id_str: String = r.get(1)?;
            let uploaded_at_str: String = r.get(10)?;
            Ok(DocumentMigRow {
                id: Uuid::parse_str(&id_str).unwrap_or_else(|_| Uuid::new_v4()),
                corpus_id: Uuid::parse_str(&corpus_id_str).unwrap_or_else(|_| Uuid::new_v4()),
                original_filename: r.get(2)?,
                mime_type: r.get(3)?,
                size_bytes: r.get(4)?,
                storage_uri: r.get(5)?,
                source_kind: r.get(6)?,
                source_ref: r.get(7)?,
                processing_status: r.get(8)?,
                processing_error: r.get(9)?,
                uploaded_at: DateTime::parse_from_rfc3339(&uploaded_at_str)
                    .map(|d| d.with_timezone(&Utc))
                    .unwrap_or_else(|_| Utc::now()),
                uploaded_by: r.get(11)?,
                page_count: r.get(12)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

async fn write_documents(pool: &PostgresPool, rows: Vec<DocumentMigRow>) -> Result<usize> {
    if rows.is_empty() {
        return Ok(0);
    }
    let conn = pool.get().await?;
    for r in &rows {
        conn.execute(
            "INSERT INTO training_documents
             (id, corpus_id, original_filename, mime_type, size_bytes, storage_uri,
              source_kind, source_ref, processing_status, processing_error,
              uploaded_at, uploaded_by, page_count)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)
             ON CONFLICT (id) DO NOTHING",
            &[
                &r.id,
                &r.corpus_id,
                &r.original_filename,
                &r.mime_type,
                &r.size_bytes,
                &r.storage_uri,
                &r.source_kind,
                &r.source_ref,
                &r.processing_status,
                &r.processing_error,
                &r.uploaded_at,
                &r.uploaded_by,
                &r.page_count,
            ],
        )
        .await
        .context("insert training_documents row")?;
    }
    Ok(rows.len())
}

struct ChunkMigRow {
    id: Uuid,
    document_id: Uuid,
    chunk_index: i32,
    content_hash: String,
    memory_id: String,
    locator: String,
    char_start: i32,
    char_end: i32,
}

fn read_chunks(sqlite: &Connection) -> Result<Vec<ChunkMigRow>> {
    let mut stmt = sqlite.prepare(
        "SELECT id, document_id, chunk_index, content_hash, memory_id,
                locator, char_start, char_end
         FROM training_chunks",
    )?;
    let rows = stmt
        .query_map([], |r| {
            let id_str: String = r.get(0)?;
            let document_id_str: String = r.get(1)?;
            Ok(ChunkMigRow {
                id: Uuid::parse_str(&id_str).unwrap_or_else(|_| Uuid::new_v4()),
                document_id: Uuid::parse_str(&document_id_str).unwrap_or_else(|_| Uuid::new_v4()),
                chunk_index: r.get(2)?,
                content_hash: r.get(3)?,
                memory_id: r.get(4)?,
                locator: r.get(5)?,
                char_start: r.get(6)?,
                char_end: r.get(7)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

async fn write_chunks(pool: &PostgresPool, rows: Vec<ChunkMigRow>) -> Result<usize> {
    if rows.is_empty() {
        return Ok(0);
    }
    let conn = pool.get().await?;
    for r in &rows {
        conn.execute(
            "INSERT INTO training_chunks
             (id, document_id, chunk_index, content_hash, memory_id,
              locator, char_start, char_end)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
             ON CONFLICT (id) DO NOTHING",
            &[
                &r.id,
                &r.document_id,
                &r.chunk_index,
                &r.content_hash,
                &r.memory_id,
                &r.locator,
                &r.char_start,
                &r.char_end,
            ],
        )
        .await
        .context("insert training_chunks row")?;
    }
    Ok(rows.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Chunk, Corpus, CorpusScope, Document, ProcessingStatus, SourceKind};
    use crate::sqlite::SqliteTrainingStore;
    use crate::store::TrainingStore;
    use workforce_postgres::test_support::{now_micros, with_test_pool};

    #[tokio::test(flavor = "multi_thread")]
    async fn migrates_training_round_trip() {
        with_test_pool(PostgresTrainingStore::build_pool, |pool| async move {
            let tmp = tempfile::NamedTempFile::new().unwrap();
            let sqlite = SqliteTrainingStore::new(tmp.path()).unwrap();

            let corpus = Corpus {
                id: Uuid::new_v4(),
                agent_id: "agent-a".into(),
                name: "product-catalogue".into(),
                scope: CorpusScope::Team("team-1".into()),
                created_at: now_micros(),
                created_by: "alice".into(),
            };
            sqlite.create_corpus(&corpus).unwrap();

            let document = Document {
                id: Uuid::new_v4(),
                corpus_id: corpus.id,
                original_filename: "report.pdf".into(),
                mime_type: "application/pdf".into(),
                size_bytes: 4096,
                storage_uri: "file://tmp/report.pdf".into(),
                source_kind: SourceKind::WebUi,
                source_ref: Some("upload-7".into()),
                processing_status: ProcessingStatus::Ready,
                processing_error: None,
                uploaded_at: now_micros(),
                uploaded_by: "bob".into(),
                page_count: Some(12),
            };
            sqlite.create_document(&document).unwrap();

            let chunk = Chunk {
                id: Uuid::new_v4(),
                document_id: document.id,
                chunk_index: 0,
                content_hash: "abc123".into(),
                memory_id: Uuid::new_v4().to_string(),
                locator: "chunk:0".into(),
                char_start: 0,
                char_end: 42,
            };
            sqlite.create_chunks(std::slice::from_ref(&chunk)).unwrap();
            drop(sqlite);

            let report = migrate_sqlite_to_postgres_with_pool(tmp.path(), pool.clone())
                .await
                .expect("migration succeeds");
            assert_eq!(report.corpora, 1);
            assert_eq!(report.documents, 1);
            assert_eq!(report.chunks, 1);

            let dest = PostgresTrainingStore::with_pool(pool.clone())
                .await
                .unwrap();
            let store: Arc<dyn TrainingStore> = dest;

            let fetched_corpus = store
                .get_corpus(corpus.id)
                .unwrap()
                .expect("corpus present");
            assert_eq!(fetched_corpus, corpus);

            let fetched_doc = store
                .get_document(document.id)
                .unwrap()
                .expect("document present");
            assert_eq!(fetched_doc, document);

            let fetched_chunks = store.list_chunks_for_document(document.id).unwrap();
            assert_eq!(fetched_chunks, vec![chunk]);

            // Idempotency: re-running is a no-op, not a duplicate-key error.
            let report2 = migrate_sqlite_to_postgres_with_pool(tmp.path(), pool)
                .await
                .expect("idempotent");
            assert_eq!(report2.corpora, 1);
            assert_eq!(report2.documents, 1);
            assert_eq!(report2.chunks, 1);
        })
        .await;
    }
}
