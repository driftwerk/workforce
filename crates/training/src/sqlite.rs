use std::path::Path;
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use eyre::{eyre, Result, WrapErr};
use rusqlite::{params, Connection, OptionalExtension};
use tracing::info;
use uuid::Uuid;

use crate::model::{Chunk, Corpus, CorpusScope, Document, ProcessingStatus, SourceKind};
use crate::store::TrainingStore;

pub struct SqliteTrainingStore {
    conn: Mutex<Connection>,
}

impl SqliteTrainingStore {
    pub fn new(path: impl AsRef<Path>) -> Result<Self> {
        let conn = Connection::open(path.as_ref())
            .wrap_err_with(|| format!("open training DB at {:?}", path.as_ref()))?;
        // Multiple AgentService instances + the HTTP handler thread + the
        // CLI direct-mode binary can all hold their own connection to this
        // same file. WAL lets readers and a writer coexist without "database
        // is locked", and busy_timeout makes the contention case wait
        // instead of erroring on the first conflict.
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
                "Training store fell back to non-WAL journal mode; \
                 concurrent writers may see `database is locked` errors",
            );
        }
        Self::init_schema(&conn)?;
        info!(path = %path.as_ref().display(), "Training store opened");
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
            CREATE TABLE IF NOT EXISTS training_corpora (
                id              TEXT PRIMARY KEY,
                agent_id        TEXT NOT NULL,
                name            TEXT NOT NULL,
                scope_kind      TEXT NOT NULL DEFAULT 'agent',
                scope_id        TEXT,
                created_at      TEXT NOT NULL,
                created_by      TEXT NOT NULL,
                UNIQUE (agent_id, name)
            );
            CREATE INDEX IF NOT EXISTS idx_training_corpora_agent
                ON training_corpora(agent_id);

            CREATE TABLE IF NOT EXISTS training_documents (
                id                  TEXT PRIMARY KEY,
                corpus_id           TEXT NOT NULL REFERENCES training_corpora(id) ON DELETE CASCADE,
                original_filename   TEXT NOT NULL,
                mime_type           TEXT NOT NULL,
                size_bytes          INTEGER NOT NULL,
                storage_uri         TEXT NOT NULL,
                source_kind         TEXT NOT NULL,
                source_ref          TEXT,
                processing_status   TEXT NOT NULL DEFAULT 'pending',
                processing_error    TEXT,
                uploaded_at         TEXT NOT NULL,
                uploaded_by         TEXT NOT NULL,
                page_count          INTEGER
            );
            CREATE INDEX IF NOT EXISTS idx_training_documents_corpus
                ON training_documents(corpus_id);
            CREATE INDEX IF NOT EXISTS idx_training_documents_status
                ON training_documents(processing_status);

            CREATE TABLE IF NOT EXISTS training_chunks (
                id              TEXT PRIMARY KEY,
                document_id     TEXT NOT NULL REFERENCES training_documents(id) ON DELETE CASCADE,
                chunk_index     INTEGER NOT NULL,
                content_hash    TEXT NOT NULL,
                memory_id       TEXT NOT NULL,
                locator         TEXT NOT NULL DEFAULT '',
                char_start      INTEGER NOT NULL DEFAULT 0,
                char_end        INTEGER NOT NULL DEFAULT 0
            );
            CREATE INDEX IF NOT EXISTS idx_training_chunks_document
                ON training_chunks(document_id);
            CREATE INDEX IF NOT EXISTS idx_training_chunks_hash
                ON training_chunks(content_hash);
            CREATE UNIQUE INDEX IF NOT EXISTS idx_training_chunks_doc_index
                ON training_chunks(document_id, chunk_index);
            ",
        )
        .wrap_err("init training schema")?;
        conn.execute("PRAGMA foreign_keys = ON", [])?;
        Ok(())
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Connection>> {
        self.conn
            .lock()
            .map_err(|e| eyre!("training DB poisoned: {e}"))
    }
}

fn parse_ts(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or_else(|_| Utc::now())
}

fn corpus_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Corpus> {
    let id_str: String = row.get("id")?;
    let agent_id: String = row.get("agent_id")?;
    let name: String = row.get("name")?;
    let scope_kind: String = row.get("scope_kind")?;
    let scope_id: Option<String> = row.get("scope_id")?;
    let created_at: String = row.get("created_at")?;
    let created_by: String = row.get("created_by")?;
    Ok(Corpus {
        id: Uuid::parse_str(&id_str).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
        })?,
        agent_id,
        name,
        scope: CorpusScope::from_parts(&scope_kind, scope_id.as_deref()),
        created_at: parse_ts(&created_at),
        created_by,
    })
}

fn document_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Document> {
    let id_str: String = row.get("id")?;
    let corpus_id_str: String = row.get("corpus_id")?;
    let mk_err = |e: uuid::Error| -> rusqlite::Error {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
    };
    Ok(Document {
        id: Uuid::parse_str(&id_str).map_err(mk_err)?,
        corpus_id: Uuid::parse_str(&corpus_id_str).map_err(mk_err)?,
        original_filename: row.get("original_filename")?,
        mime_type: row.get("mime_type")?,
        size_bytes: row.get("size_bytes")?,
        storage_uri: row.get("storage_uri")?,
        source_kind: SourceKind::parse(&row.get::<_, String>("source_kind")?),
        source_ref: row.get("source_ref")?,
        processing_status: ProcessingStatus::parse(&row.get::<_, String>("processing_status")?),
        processing_error: row.get("processing_error")?,
        uploaded_at: parse_ts(&row.get::<_, String>("uploaded_at")?),
        uploaded_by: row.get("uploaded_by")?,
        page_count: row.get("page_count")?,
    })
}

fn chunk_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Chunk> {
    let id_str: String = row.get("id")?;
    let doc_id_str: String = row.get("document_id")?;
    let mk_err = |e: uuid::Error| -> rusqlite::Error {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
    };
    Ok(Chunk {
        id: Uuid::parse_str(&id_str).map_err(mk_err)?,
        document_id: Uuid::parse_str(&doc_id_str).map_err(mk_err)?,
        chunk_index: row.get("chunk_index")?,
        content_hash: row.get("content_hash")?,
        memory_id: row.get("memory_id")?,
        locator: row.get("locator")?,
        char_start: row.get("char_start")?,
        char_end: row.get("char_end")?,
    })
}

impl TrainingStore for SqliteTrainingStore {
    fn create_corpus(&self, c: &Corpus) -> Result<()> {
        let conn = self.lock()?;
        let (scope_kind, scope_id) = c.scope.parts();
        conn.execute(
            "INSERT INTO training_corpora (id, agent_id, name, scope_kind, scope_id, created_at, created_by)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                c.id.to_string(),
                c.agent_id,
                c.name,
                scope_kind,
                scope_id,
                c.created_at.to_rfc3339(),
                c.created_by,
            ],
        )
        .wrap_err("insert corpus")?;
        Ok(())
    }

    fn list_corpora(&self, agent_id: &str) -> Result<Vec<Corpus>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT id, agent_id, name, scope_kind, scope_id, created_at, created_by
             FROM training_corpora WHERE agent_id = ?1 ORDER BY name",
        )?;
        let rows = stmt.query_map(params![agent_id], corpus_from_row)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .wrap_err("list corpora")
    }

    fn list_all_corpora(&self) -> Result<Vec<Corpus>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT id, agent_id, name, scope_kind, scope_id, created_at, created_by
             FROM training_corpora ORDER BY agent_id, name",
        )?;
        let rows = stmt.query_map([], corpus_from_row)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .wrap_err("list all corpora")
    }

    fn get_corpus_by_name(&self, agent_id: &str, name: &str) -> Result<Option<Corpus>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT id, agent_id, name, scope_kind, scope_id, created_at, created_by
             FROM training_corpora WHERE agent_id = ?1 AND name = ?2",
        )?;
        stmt.query_row(params![agent_id, name], corpus_from_row)
            .optional()
            .wrap_err("get corpus by name")
    }

    fn get_corpus(&self, corpus_id: Uuid) -> Result<Option<Corpus>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT id, agent_id, name, scope_kind, scope_id, created_at, created_by
             FROM training_corpora WHERE id = ?1",
        )?;
        stmt.query_row(params![corpus_id.to_string()], corpus_from_row)
            .optional()
            .wrap_err("get corpus")
    }

    fn delete_corpus(&self, corpus_id: Uuid) -> Result<bool> {
        let conn = self.lock()?;
        let n = conn.execute(
            "DELETE FROM training_corpora WHERE id = ?1",
            params![corpus_id.to_string()],
        )?;
        Ok(n > 0)
    }

    fn set_corpus_scope(&self, corpus_id: Uuid, scope: &CorpusScope) -> Result<bool> {
        let (kind, id) = scope.parts();
        let n = self.lock()?.execute(
            "UPDATE training_corpora SET scope_kind = ?2, scope_id = ?3 WHERE id = ?1",
            params![corpus_id.to_string(), kind, id],
        )?;
        Ok(n > 0)
    }

    fn create_document(&self, d: &Document) -> Result<()> {
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO training_documents
             (id, corpus_id, original_filename, mime_type, size_bytes, storage_uri,
              source_kind, source_ref, processing_status, processing_error,
              uploaded_at, uploaded_by, page_count)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            params![
                d.id.to_string(),
                d.corpus_id.to_string(),
                d.original_filename,
                d.mime_type,
                d.size_bytes,
                d.storage_uri,
                d.source_kind.as_str(),
                d.source_ref,
                d.processing_status.as_str(),
                d.processing_error,
                d.uploaded_at.to_rfc3339(),
                d.uploaded_by,
                d.page_count,
            ],
        )
        .wrap_err("insert document")?;
        Ok(())
    }

    fn list_documents(&self, corpus_id: Uuid) -> Result<Vec<Document>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT * FROM training_documents WHERE corpus_id = ?1 ORDER BY uploaded_at DESC",
        )?;
        let rows = stmt.query_map(params![corpus_id.to_string()], document_from_row)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .wrap_err("list documents")
    }

    fn find_document_by_filename(
        &self,
        corpus_id: Uuid,
        filename: &str,
    ) -> Result<Option<Document>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT * FROM training_documents
             WHERE corpus_id = ?1 AND original_filename = ?2
             ORDER BY uploaded_at DESC LIMIT 1",
        )?;
        stmt.query_row(params![corpus_id.to_string(), filename], document_from_row)
            .optional()
            .wrap_err("find document by filename")
    }

    fn get_document(&self, document_id: Uuid) -> Result<Option<Document>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare("SELECT * FROM training_documents WHERE id = ?1")?;
        stmt.query_row(params![document_id.to_string()], document_from_row)
            .optional()
            .wrap_err("get document")
    }

    fn update_document_status(
        &self,
        document_id: Uuid,
        status: ProcessingStatus,
        error: Option<&str>,
    ) -> Result<()> {
        let conn = self.lock()?;
        conn.execute(
            "UPDATE training_documents
             SET processing_status = ?1, processing_error = ?2
             WHERE id = ?3",
            params![status.as_str(), error, document_id.to_string()],
        )?;
        Ok(())
    }

    fn delete_document(&self, document_id: Uuid) -> Result<bool> {
        let conn = self.lock()?;
        let n = conn.execute(
            "DELETE FROM training_documents WHERE id = ?1",
            params![document_id.to_string()],
        )?;
        Ok(n > 0)
    }

    fn create_chunks(&self, chunks: &[Chunk]) -> Result<()> {
        if chunks.is_empty() {
            return Ok(());
        }
        let mut conn = self.lock()?;
        let tx = conn.transaction()?;
        for c in chunks {
            tx.execute(
                "INSERT INTO training_chunks
                 (id, document_id, chunk_index, content_hash, memory_id, locator, char_start, char_end)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    c.id.to_string(),
                    c.document_id.to_string(),
                    c.chunk_index,
                    c.content_hash,
                    c.memory_id,
                    c.locator,
                    c.char_start,
                    c.char_end,
                ],
            )?;
        }
        tx.commit().wrap_err("commit chunk insert")?;
        Ok(())
    }

    fn list_chunks_for_document(&self, document_id: Uuid) -> Result<Vec<Chunk>> {
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare("SELECT * FROM training_chunks WHERE document_id = ?1 ORDER BY chunk_index")?;
        let rows = stmt.query_map(params![document_id.to_string()], chunk_from_row)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .wrap_err("list chunks for document")
    }

    fn list_all_chunk_memory_ids(&self) -> Result<Vec<String>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare("SELECT memory_id FROM training_chunks")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .wrap_err("list all chunk memory ids")
    }

    fn list_chunks_for_corpus(&self, corpus_id: Uuid) -> Result<Vec<Chunk>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT c.* FROM training_chunks c
             JOIN training_documents d ON c.document_id = d.id
             WHERE d.corpus_id = ?1
             ORDER BY d.uploaded_at, c.chunk_index",
        )?;
        let rows = stmt.query_map(params![corpus_id.to_string()], chunk_from_row)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .wrap_err("list chunks for corpus")
    }

    fn delete_chunk(&self, chunk_id: Uuid) -> Result<bool> {
        let conn = self.lock()?;
        let n = conn.execute(
            "DELETE FROM training_chunks WHERE id = ?1",
            params![chunk_id.to_string()],
        )?;
        Ok(n > 0)
    }

    fn delete_chunks_for_document(&self, document_id: Uuid) -> Result<()> {
        let conn = self.lock()?;
        conn.execute(
            "DELETE FROM training_chunks WHERE document_id = ?1",
            params![document_id.to_string()],
        )?;
        Ok(())
    }

    fn find_chunk_in_corpus_by_hash(
        &self,
        corpus_id: Uuid,
        content_hash: &str,
    ) -> Result<Option<Chunk>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT c.* FROM training_chunks c
             JOIN training_documents d ON c.document_id = d.id
             WHERE d.corpus_id = ?1 AND c.content_hash = ?2
             LIMIT 1",
        )?;
        stmt.query_row(params![corpus_id.to_string(), content_hash], chunk_from_row)
            .optional()
            .wrap_err("find chunk by hash")
    }
}

#[cfg(test)]
#[allow(unused_must_use)]
mod tests {
    use super::*;
    use crate::model::ChunkIdentity;

    fn store() -> SqliteTrainingStore {
        SqliteTrainingStore::in_memory().unwrap()
    }

    fn make_corpus(agent: &str, name: &str) -> Corpus {
        Corpus {
            id: Uuid::new_v4(),
            agent_id: agent.into(),
            name: name.into(),
            scope: CorpusScope::Agent,
            created_at: Utc::now(),
            created_by: "test".into(),
        }
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
            uploaded_at: Utc::now(),
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

    #[test]
    fn corpus_crud_round_trip() {
        let s = store();
        let c = make_corpus("mark", "product-catalogue");
        s.create_corpus(&c).unwrap();
        let got = s
            .get_corpus_by_name("mark", "product-catalogue")
            .unwrap()
            .unwrap();
        assert_eq!(got, c);
        assert_eq!(s.list_corpora("mark").unwrap(), vec![c.clone()]);
        assert!(s.delete_corpus(c.id).unwrap());
        assert!(s.list_corpora("mark").unwrap().is_empty());
    }

    #[test]
    fn a_corpus_moves_to_another_scope() {
        let s = store();
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
        assert!(!s
            .set_corpus_scope(Uuid::new_v4(), &CorpusScope::Agent)
            .unwrap());
    }

    #[test]
    fn delete_corpus_cascades_documents_and_chunks() {
        let s = store();
        let c = make_corpus("mark", "x");
        s.create_corpus(&c).unwrap();
        let d = make_document(c.id, "x.json");
        s.create_document(&d).unwrap();
        let ch = make_chunk(d.id, c.id, 0, "hello");
        s.create_chunks(std::slice::from_ref(&ch)).unwrap();
        assert_eq!(s.list_chunks_for_corpus(c.id).unwrap().len(), 1);
        s.delete_corpus(c.id).unwrap();
        assert!(s.get_document(d.id).unwrap().is_none());
        assert!(s.list_chunks_for_corpus(c.id).unwrap().is_empty());
    }

    #[test]
    fn find_chunk_by_hash_returns_match_in_corpus() {
        let s = store();
        let c = make_corpus("mark", "x");
        s.create_corpus(&c).unwrap();
        let d = make_document(c.id, "x.json");
        s.create_document(&d).unwrap();
        let ch = make_chunk(d.id, c.id, 0, "hello");
        s.create_chunks(std::slice::from_ref(&ch)).unwrap();
        let hit = s
            .find_chunk_in_corpus_by_hash(c.id, &ch.content_hash)
            .unwrap();
        assert_eq!(hit, Some(ch));
        let miss = s.find_chunk_in_corpus_by_hash(c.id, "nope").unwrap();
        assert!(miss.is_none());
    }

    #[test]
    fn update_document_status_persists() {
        let s = store();
        let c = make_corpus("mark", "x");
        s.create_corpus(&c).unwrap();
        let d = make_document(c.id, "x.json");
        s.create_document(&d).unwrap();
        s.update_document_status(d.id, ProcessingStatus::Ready, None)
            .unwrap();
        let got = s.get_document(d.id).unwrap().unwrap();
        assert_eq!(got.processing_status, ProcessingStatus::Ready);
    }

    #[test]
    fn unique_corpus_name_per_agent_enforced() {
        let s = store();
        let c1 = make_corpus("mark", "x");
        s.create_corpus(&c1).unwrap();
        let c2 = make_corpus("mark", "x");
        assert!(s.create_corpus(&c2).is_err());
    }

    #[test]
    fn list_all_corpora_walks_every_agent() {
        let s = store();
        s.create_corpus(&make_corpus("mark", "a")).unwrap();
        s.create_corpus(&make_corpus("mark", "b")).unwrap();
        s.create_corpus(&make_corpus("alice", "a")).unwrap();
        let all = s.list_all_corpora().unwrap();
        let pairs: Vec<(String, String)> = all
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
    }

    #[test]
    fn list_all_chunk_memory_ids_returns_every_chunk() {
        let s = store();
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
    }
}
