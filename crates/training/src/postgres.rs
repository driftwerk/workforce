//! Postgres backend for [`TrainingStore`].
//!
//! Mirrors every method of the SQLite backend at
//! [`crate::sqlite::SqliteTrainingStore`]. Selected at runtime via
//! `[training] backend = "postgres"` in `workforce.toml`; the
//! [`TrainingStore`] trait is stable so call sites are unchanged
//! regardless of which backend is in use.
//!
//! The async-to-sync bridge and per-store-pool architecture are the
//! same as `workforce-mcp-store`; see that crate's `postgres.rs` doc
//! comment for the full rationale.

use std::sync::Arc;

use eyre::{Context, Result};
use tokio_postgres::Row;
use uuid::Uuid;
use workforce_postgres::{
    MigrationRun, MigrationRunner, MigrationSet, PostgresConfig, PostgresError, PostgresPool,
};

use crate::model::{Chunk, Corpus, CorpusScope, Document, ProcessingStatus, SourceKind};
use crate::store::TrainingStore;

mod embedded {
    refinery::embed_migrations!("migrations");
}

pub struct PostgresTrainingStore {
    pool: PostgresPool,
}

impl PostgresTrainingStore {
    pub const SCHEMA: &'static str = "training";

    pub fn build_pool(config: &PostgresConfig) -> Result<PostgresPool, PostgresError> {
        PostgresPool::from_config_with_session_init(
            config,
            &["SET search_path TO training, public"],
        )
    }

    pub async fn new(config: &PostgresConfig) -> Result<Arc<Self>> {
        let pool = Self::build_pool(config).context("building training-store pool")?;
        Self::with_pool(pool).await
    }

    pub async fn with_pool(pool: PostgresPool) -> Result<Arc<Self>> {
        let migration_set = MigrationSet::new(
            Self::SCHEMA,
            Box::new(EmbeddedTrainingMigrations) as Box<dyn MigrationRun>,
        );
        MigrationRunner::run(&pool, &[migration_set])
            .await
            .context("running training-store migrations")?;
        Ok(Arc::new(Self { pool }))
    }

    fn block_on<F, T>(&self, fut: F) -> Result<T>
    where
        F: std::future::Future<Output = Result<T>>,
    {
        tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(fut))
    }
}

struct EmbeddedTrainingMigrations;

#[async_trait::async_trait]
impl MigrationRun for EmbeddedTrainingMigrations {
    async fn run(&self, client: &mut tokio_postgres::Client) -> Result<(), PostgresError> {
        embedded::migrations::runner()
            .run_async(client)
            .await
            .map_err(PostgresError::from)?;
        Ok(())
    }
}

fn corpus_from_row(row: &Row) -> Result<Corpus> {
    let scope_kind: String = row.try_get("scope_kind")?;
    let scope_id: Option<String> = row.try_get("scope_id")?;
    Ok(Corpus {
        id: row.try_get("id")?,
        agent_id: row.try_get("agent_id")?,
        name: row.try_get("name")?,
        scope: CorpusScope::from_parts(&scope_kind, scope_id.as_deref()),
        created_at: row.try_get("created_at")?,
        created_by: row.try_get("created_by")?,
    })
}

fn document_from_row(row: &Row) -> Result<Document> {
    let source_kind_str: String = row.try_get("source_kind")?;
    let processing_status_str: String = row.try_get("processing_status")?;
    Ok(Document {
        id: row.try_get("id")?,
        corpus_id: row.try_get("corpus_id")?,
        original_filename: row.try_get("original_filename")?,
        mime_type: row.try_get("mime_type")?,
        size_bytes: row.try_get("size_bytes")?,
        storage_uri: row.try_get("storage_uri")?,
        source_kind: SourceKind::parse(&source_kind_str),
        source_ref: row.try_get("source_ref")?,
        processing_status: ProcessingStatus::parse(&processing_status_str),
        processing_error: row.try_get("processing_error")?,
        uploaded_at: row.try_get("uploaded_at")?,
        uploaded_by: row.try_get("uploaded_by")?,
        page_count: row.try_get("page_count")?,
    })
}

fn chunk_from_row(row: &Row) -> Result<Chunk> {
    Ok(Chunk {
        id: row.try_get("id")?,
        document_id: row.try_get("document_id")?,
        chunk_index: row.try_get("chunk_index")?,
        content_hash: row.try_get("content_hash")?,
        memory_id: row.try_get("memory_id")?,
        locator: row.try_get("locator")?,
        char_start: row.try_get("char_start")?,
        char_end: row.try_get("char_end")?,
    })
}

const CORPUS_COLUMNS: &str = "id, agent_id, name, scope_kind, scope_id, created_at, created_by";
const DOCUMENT_COLUMNS: &str = "id, corpus_id, original_filename, mime_type, size_bytes, \
     storage_uri, source_kind, source_ref, processing_status, processing_error, \
     uploaded_at, uploaded_by, page_count";
const CHUNK_COLUMNS: &str =
    "id, document_id, chunk_index, content_hash, memory_id, locator, char_start, char_end";

impl TrainingStore for PostgresTrainingStore {
    fn create_corpus(&self, c: &Corpus) -> Result<()> {
        let c = c.clone();
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let (scope_kind, scope_id) = c.scope.parts();
            let scope_id_owned = scope_id.map(str::to_string);
            conn.execute(
                "INSERT INTO training_corpora \
                     (id, agent_id, name, scope_kind, scope_id, created_at, created_by) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7)",
                &[
                    &c.id,
                    &c.agent_id,
                    &c.name,
                    &scope_kind,
                    &scope_id_owned,
                    &c.created_at,
                    &c.created_by,
                ],
            )
            .await
            .context("insert corpus")?;
            Ok(())
        })
    }

    fn list_corpora(&self, agent_id: &str) -> Result<Vec<Corpus>> {
        let agent_id = agent_id.to_string();
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let rows = conn
                .query(
                    &format!(
                        "SELECT {CORPUS_COLUMNS} FROM training_corpora \
                         WHERE agent_id = $1 ORDER BY name"
                    ),
                    &[&agent_id],
                )
                .await?;
            rows.iter().map(corpus_from_row).collect()
        })
    }

    fn list_all_corpora(&self) -> Result<Vec<Corpus>> {
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let rows = conn
                .query(
                    &format!(
                        "SELECT {CORPUS_COLUMNS} FROM training_corpora \
                         ORDER BY agent_id, name"
                    ),
                    &[],
                )
                .await?;
            rows.iter().map(corpus_from_row).collect()
        })
    }

    fn get_corpus_by_name(&self, agent_id: &str, name: &str) -> Result<Option<Corpus>> {
        let agent_id = agent_id.to_string();
        let name = name.to_string();
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let row = conn
                .query_opt(
                    &format!(
                        "SELECT {CORPUS_COLUMNS} FROM training_corpora \
                         WHERE agent_id = $1 AND name = $2"
                    ),
                    &[&agent_id, &name],
                )
                .await?;
            row.as_ref().map(corpus_from_row).transpose()
        })
    }

    fn get_corpus(&self, corpus_id: Uuid) -> Result<Option<Corpus>> {
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let row = conn
                .query_opt(
                    &format!("SELECT {CORPUS_COLUMNS} FROM training_corpora WHERE id = $1"),
                    &[&corpus_id],
                )
                .await?;
            row.as_ref().map(corpus_from_row).transpose()
        })
    }

    fn delete_corpus(&self, corpus_id: Uuid) -> Result<bool> {
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let n = conn
                .execute("DELETE FROM training_corpora WHERE id = $1", &[&corpus_id])
                .await?;
            Ok(n > 0)
        })
    }

    fn set_corpus_scope(&self, corpus_id: Uuid, scope: &CorpusScope) -> Result<bool> {
        let (kind, id) = scope.parts();
        let id = id.map(str::to_string);
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let n = conn
                .execute(
                    "UPDATE training_corpora SET scope_kind = $2, scope_id = $3 WHERE id = $1",
                    &[&corpus_id, &kind, &id],
                )
                .await?;
            Ok(n > 0)
        })
    }

    fn create_document(&self, d: &Document) -> Result<()> {
        let d = d.clone();
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let source_kind = d.source_kind.as_str().to_string();
            let processing_status = d.processing_status.as_str().to_string();
            conn.execute(
                "INSERT INTO training_documents \
                     (id, corpus_id, original_filename, mime_type, size_bytes, storage_uri, \
                      source_kind, source_ref, processing_status, processing_error, \
                      uploaded_at, uploaded_by, page_count) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)",
                &[
                    &d.id,
                    &d.corpus_id,
                    &d.original_filename,
                    &d.mime_type,
                    &d.size_bytes,
                    &d.storage_uri,
                    &source_kind,
                    &d.source_ref,
                    &processing_status,
                    &d.processing_error,
                    &d.uploaded_at,
                    &d.uploaded_by,
                    &d.page_count,
                ],
            )
            .await
            .context("insert document")?;
            Ok(())
        })
    }

    fn list_documents(&self, corpus_id: Uuid) -> Result<Vec<Document>> {
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let rows = conn
                .query(
                    &format!(
                        "SELECT {DOCUMENT_COLUMNS} FROM training_documents \
                         WHERE corpus_id = $1 ORDER BY uploaded_at DESC, id"
                    ),
                    &[&corpus_id],
                )
                .await?;
            rows.iter().map(document_from_row).collect()
        })
    }

    fn find_document_by_filename(
        &self,
        corpus_id: Uuid,
        filename: &str,
    ) -> Result<Option<Document>> {
        let filename = filename.to_string();
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let row = conn
                .query_opt(
                    &format!(
                        "SELECT {DOCUMENT_COLUMNS} FROM training_documents \
                         WHERE corpus_id = $1 AND original_filename = $2 \
                         ORDER BY uploaded_at DESC LIMIT 1"
                    ),
                    &[&corpus_id, &filename],
                )
                .await?;
            row.as_ref().map(document_from_row).transpose()
        })
    }

    fn get_document(&self, document_id: Uuid) -> Result<Option<Document>> {
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let row = conn
                .query_opt(
                    &format!("SELECT {DOCUMENT_COLUMNS} FROM training_documents WHERE id = $1"),
                    &[&document_id],
                )
                .await?;
            row.as_ref().map(document_from_row).transpose()
        })
    }

    fn update_document_status(
        &self,
        document_id: Uuid,
        status: ProcessingStatus,
        error: Option<&str>,
    ) -> Result<()> {
        let status = status.as_str().to_string();
        let error = error.map(str::to_string);
        self.block_on(async move {
            let conn = self.pool.get().await?;
            conn.execute(
                "UPDATE training_documents \
                 SET processing_status = $1, processing_error = $2 \
                 WHERE id = $3",
                &[&status, &error, &document_id],
            )
            .await
            .context("update document status")?;
            Ok(())
        })
    }

    fn delete_document(&self, document_id: Uuid) -> Result<bool> {
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let n = conn
                .execute(
                    "DELETE FROM training_documents WHERE id = $1",
                    &[&document_id],
                )
                .await?;
            Ok(n > 0)
        })
    }

    fn create_chunks(&self, chunks: &[Chunk]) -> Result<()> {
        if chunks.is_empty() {
            return Ok(());
        }
        let chunks = chunks.to_vec();
        self.block_on(async move {
            let mut conn = self.pool.get().await?;
            let tx = conn.transaction().await?;
            for c in &chunks {
                tx.execute(
                    "INSERT INTO training_chunks \
                         (id, document_id, chunk_index, content_hash, memory_id, \
                          locator, char_start, char_end) \
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
                    &[
                        &c.id,
                        &c.document_id,
                        &c.chunk_index,
                        &c.content_hash,
                        &c.memory_id,
                        &c.locator,
                        &c.char_start,
                        &c.char_end,
                    ],
                )
                .await
                .context("insert chunk")?;
            }
            tx.commit().await.context("commit chunk insert")?;
            Ok(())
        })
    }

    fn list_chunks_for_document(&self, document_id: Uuid) -> Result<Vec<Chunk>> {
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let rows = conn
                .query(
                    &format!(
                        "SELECT {CHUNK_COLUMNS} FROM training_chunks \
                         WHERE document_id = $1 ORDER BY chunk_index"
                    ),
                    &[&document_id],
                )
                .await?;
            rows.iter().map(chunk_from_row).collect()
        })
    }

    fn list_all_chunk_memory_ids(&self) -> Result<Vec<String>> {
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let rows = conn
                .query("SELECT memory_id FROM training_chunks", &[])
                .await?;
            rows.iter()
                .map(|r| r.try_get::<_, String>(0).context("read memory_id"))
                .collect()
        })
    }

    fn list_chunks_for_corpus(&self, corpus_id: Uuid) -> Result<Vec<Chunk>> {
        self.block_on(async move {
            let conn = self.pool.get().await?;
            // Corpus-scoped chunks need the join because chunks reference
            // documents, not corpora directly. Order matches SQLite.
            let rows = conn
                .query(
                    "SELECT c.id, c.document_id, c.chunk_index, c.content_hash, \
                            c.memory_id, c.locator, c.char_start, c.char_end \
                     FROM training_chunks c \
                     JOIN training_documents d ON c.document_id = d.id \
                     WHERE d.corpus_id = $1 \
                     ORDER BY d.uploaded_at, c.chunk_index",
                    &[&corpus_id],
                )
                .await?;
            rows.iter().map(chunk_from_row).collect()
        })
    }

    fn delete_chunk(&self, chunk_id: Uuid) -> Result<bool> {
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let n = conn
                .execute("DELETE FROM training_chunks WHERE id = $1", &[&chunk_id])
                .await?;
            Ok(n > 0)
        })
    }

    fn delete_chunks_for_document(&self, document_id: Uuid) -> Result<()> {
        self.block_on(async move {
            let conn = self.pool.get().await?;
            conn.execute(
                "DELETE FROM training_chunks WHERE document_id = $1",
                &[&document_id],
            )
            .await?;
            Ok(())
        })
    }

    fn find_chunk_in_corpus_by_hash(
        &self,
        corpus_id: Uuid,
        content_hash: &str,
    ) -> Result<Option<Chunk>> {
        let content_hash = content_hash.to_string();
        self.block_on(async move {
            let conn = self.pool.get().await?;
            let row = conn
                .query_opt(
                    "SELECT c.id, c.document_id, c.chunk_index, c.content_hash, \
                            c.memory_id, c.locator, c.char_start, c.char_end \
                     FROM training_chunks c \
                     JOIN training_documents d ON c.document_id = d.id \
                     WHERE d.corpus_id = $1 AND c.content_hash = $2 \
                     LIMIT 1",
                    &[&corpus_id, &content_hash],
                )
                .await?;
            row.as_ref().map(chunk_from_row).transpose()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ChunkIdentity;
    use workforce_postgres::test_support::{now_micros, with_test_pool};

    async fn with_embedded_pg<F, Fut>(test: F)
    where
        F: FnOnce(Arc<PostgresTrainingStore>) -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        with_test_pool(PostgresTrainingStore::build_pool, |pool| async move {
            let store = PostgresTrainingStore::with_pool(pool)
                .await
                .expect("construct store");
            test(store).await;
        })
        .await;
    }

    fn make_corpus(agent: &str, name: &str) -> Corpus {
        Corpus {
            id: Uuid::new_v4(),
            agent_id: agent.into(),
            name: name.into(),
            scope: CorpusScope::Agent,
            created_at: now_micros(),
            created_by: "test".into(),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn pg_a_corpus_moves_to_another_scope() {
        with_embedded_pg(|s| async move {
            let c = Corpus {
                scope: CorpusScope::User("alice".into()),
                ..make_corpus("mark", "notes")
            };
            s.create_corpus(&c).unwrap();
            assert!(s
                .set_corpus_scope(c.id, &CorpusScope::User("bob".into()))
                .unwrap());
            assert_eq!(
                s.get_corpus(c.id).unwrap().unwrap().scope,
                CorpusScope::User("bob".into())
            );
        })
        .await;
    }

    fn make_document(corpus_id: Uuid, filename: &str) -> Document {
        Document {
            id: Uuid::new_v4(),
            corpus_id,
            original_filename: filename.into(),
            mime_type: "application/json".into(),
            size_bytes: 100,
            storage_uri: format!("file://tmp/{filename}"),
            source_kind: SourceKind::Cli,
            source_ref: None,
            processing_status: ProcessingStatus::Pending,
            processing_error: None,
            uploaded_at: now_micros(),
            uploaded_by: "test".into(),
            page_count: None,
        }
    }

    fn make_chunk(doc_id: Uuid, corpus_id: Uuid, index: i32, body: &str) -> Chunk {
        Chunk {
            id: Uuid::new_v4(),
            document_id: doc_id,
            chunk_index: index,
            content_hash: ChunkIdentity::hash(corpus_id, body),
            memory_id: Uuid::new_v4().to_string(),
            locator: format!("chunk:{index}"),
            char_start: 0,
            char_end: body.len() as i32,
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn parity_corpus_crud_round_trip() {
        with_embedded_pg(|store| async move {
            let s: Arc<dyn TrainingStore> = store;
            let c = make_corpus("mark", "product-catalogue");
            s.create_corpus(&c).unwrap();
            let got = s
                .get_corpus_by_name("mark", "product-catalogue")
                .unwrap()
                .unwrap();
            assert_eq!(got, c);
            assert_eq!(s.list_corpora("mark").unwrap(), vec![c.clone()]);
            assert!(s.get_corpus(c.id).unwrap().is_some());
            assert!(s.delete_corpus(c.id).unwrap());
            assert!(s.list_corpora("mark").unwrap().is_empty());
            assert!(!s.delete_corpus(c.id).unwrap());
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn parity_corpus_unique_name_per_agent() {
        with_embedded_pg(|store| async move {
            let s: Arc<dyn TrainingStore> = store;
            s.create_corpus(&make_corpus("mark", "x")).unwrap();
            assert!(s.create_corpus(&make_corpus("mark", "x")).is_err());
            // Same name under a different agent is fine.
            s.create_corpus(&make_corpus("alice", "x")).unwrap();
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn parity_document_crud_and_status() {
        with_embedded_pg(|store| async move {
            let s: Arc<dyn TrainingStore> = store;
            let c = make_corpus("mark", "x");
            s.create_corpus(&c).unwrap();
            let d = make_document(c.id, "x.json");
            s.create_document(&d).unwrap();

            assert_eq!(s.list_documents(c.id).unwrap(), vec![d.clone()]);
            assert_eq!(s.get_document(d.id).unwrap(), Some(d.clone()));

            s.update_document_status(d.id, ProcessingStatus::Processing, None)
                .unwrap();
            s.update_document_status(d.id, ProcessingStatus::Ready, None)
                .unwrap();
            let got = s.get_document(d.id).unwrap().unwrap();
            assert_eq!(got.processing_status, ProcessingStatus::Ready);

            s.update_document_status(d.id, ProcessingStatus::Failed, Some("boom"))
                .unwrap();
            let failed = s.get_document(d.id).unwrap().unwrap();
            assert_eq!(failed.processing_error.as_deref(), Some("boom"));

            assert!(s.delete_document(d.id).unwrap());
            assert!(s.get_document(d.id).unwrap().is_none());
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn parity_find_document_by_filename_scopes_to_corpus() {
        with_embedded_pg(|store| async move {
            let s: Arc<dyn TrainingStore> = store;
            let c1 = make_corpus("mark", "a");
            let c2 = make_corpus("mark", "b");
            s.create_corpus(&c1).unwrap();
            s.create_corpus(&c2).unwrap();
            let d1 = make_document(c1.id, "shared.json");
            let d2 = make_document(c2.id, "shared.json");
            s.create_document(&d1).unwrap();
            s.create_document(&d2).unwrap();

            let hit1 = s
                .find_document_by_filename(c1.id, "shared.json")
                .unwrap()
                .unwrap();
            assert_eq!(hit1.id, d1.id);
            let hit2 = s
                .find_document_by_filename(c2.id, "shared.json")
                .unwrap()
                .unwrap();
            assert_eq!(hit2.id, d2.id);
            assert!(s
                .find_document_by_filename(c1.id, "missing.json")
                .unwrap()
                .is_none());
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn parity_chunk_crud_and_dedup_in_corpus() {
        with_embedded_pg(|store| async move {
            let s: Arc<dyn TrainingStore> = store;
            let c = make_corpus("mark", "x");
            s.create_corpus(&c).unwrap();
            let d = make_document(c.id, "x.json");
            s.create_document(&d).unwrap();

            let ch = make_chunk(d.id, c.id, 0, "hello");
            s.create_chunks(std::slice::from_ref(&ch)).unwrap();
            assert_eq!(s.list_chunks_for_document(d.id).unwrap(), vec![ch.clone()]);
            assert_eq!(s.list_chunks_for_corpus(c.id).unwrap(), vec![ch.clone()]);

            let hit = s
                .find_chunk_in_corpus_by_hash(c.id, &ch.content_hash)
                .unwrap();
            assert_eq!(hit, Some(ch.clone()));
            assert!(s
                .find_chunk_in_corpus_by_hash(c.id, "missing")
                .unwrap()
                .is_none());

            assert!(s.delete_chunk(ch.id).unwrap());
            assert!(s.list_chunks_for_document(d.id).unwrap().is_empty());

            // delete_chunks_for_document is a no-op when there are no chunks.
            s.delete_chunks_for_document(d.id).unwrap();

            // Empty-slice create is a no-op.
            s.create_chunks(&[]).unwrap();
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn parity_delete_corpus_cascades_documents_and_chunks() {
        with_embedded_pg(|store| async move {
            let s: Arc<dyn TrainingStore> = store;
            let c = make_corpus("mark", "x");
            s.create_corpus(&c).unwrap();
            let d = make_document(c.id, "x.json");
            s.create_document(&d).unwrap();
            let ch = make_chunk(d.id, c.id, 0, "hello");
            s.create_chunks(std::slice::from_ref(&ch)).unwrap();

            assert!(s.delete_corpus(c.id).unwrap());
            assert!(s.get_document(d.id).unwrap().is_none());
            assert!(s.list_chunks_for_corpus(c.id).unwrap().is_empty());
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn parity_list_orders_are_stable() {
        with_embedded_pg(|store| async move {
            let s: Arc<dyn TrainingStore> = store;
            let c = make_corpus("mark", "x");
            s.create_corpus(&c).unwrap();
            // Insert documents in reverse upload order; list_documents
            // must return them newest-first.
            let mut d_old = make_document(c.id, "old.json");
            d_old.uploaded_at = now_micros() - chrono::Duration::seconds(60);
            let d_new = make_document(c.id, "new.json");
            s.create_document(&d_old).unwrap();
            s.create_document(&d_new).unwrap();
            let docs = s.list_documents(c.id).unwrap();
            assert_eq!(docs[0].id, d_new.id);
            assert_eq!(docs[1].id, d_old.id);

            // Chunks must come back sorted by chunk_index.
            let c1 = make_chunk(d_new.id, c.id, 2, "two");
            let c0 = make_chunk(d_new.id, c.id, 0, "zero");
            let c2 = make_chunk(d_new.id, c.id, 1, "one");
            s.create_chunks(&[c1, c0, c2]).unwrap();
            let chunks = s.list_chunks_for_document(d_new.id).unwrap();
            assert_eq!(
                chunks.iter().map(|c| c.chunk_index).collect::<Vec<_>>(),
                vec![0, 1, 2],
            );

            // Multiple corpora for the same agent come back ordered by name.
            s.create_corpus(&make_corpus("mark", "a")).unwrap();
            s.create_corpus(&make_corpus("mark", "z")).unwrap();
            let names: Vec<_> = s
                .list_corpora("mark")
                .unwrap()
                .iter()
                .map(|c| c.name.clone())
                .collect();
            assert_eq!(names, vec!["a", "x", "z"]);
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn parity_list_all_corpora_walks_every_agent() {
        with_embedded_pg(|store| async move {
            let s: Arc<dyn TrainingStore> = store;
            s.create_corpus(&make_corpus("mark", "a")).unwrap();
            s.create_corpus(&make_corpus("mark", "b")).unwrap();
            s.create_corpus(&make_corpus("alice", "a")).unwrap();
            let pairs: Vec<(String, String)> = s
                .list_all_corpora()
                .unwrap()
                .iter()
                .map(|c| (c.agent_id.clone(), c.name.clone()))
                .collect();
            assert_eq!(
                pairs,
                vec![
                    ("alice".into(), "a".into()),
                    ("mark".into(), "a".into()),
                    ("mark".into(), "b".into()),
                ],
            );
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn parity_list_all_chunk_memory_ids_returns_every_chunk() {
        with_embedded_pg(|store| async move {
            let s: Arc<dyn TrainingStore> = store;
            let c = make_corpus("mark", "x");
            s.create_corpus(&c).unwrap();
            let d = make_document(c.id, "x.json");
            s.create_document(&d).unwrap();
            let ch1 = make_chunk(d.id, c.id, 0, "hello");
            let ch2 = make_chunk(d.id, c.id, 1, "world");
            s.create_chunks(&[ch1.clone(), ch2.clone()]).unwrap();

            let mut ids = s.list_all_chunk_memory_ids().unwrap();
            ids.sort();
            let mut expected = vec![ch1.memory_id, ch2.memory_id];
            expected.sort();
            assert_eq!(ids, expected);
        })
        .await;
    }
}
