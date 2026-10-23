//! SQLite-backed memory store with FTS5 keyword search and optional vector similarity.
//!
//! Provides embedded memory search that works out of the box without external services.
//! When an embedding provider is configured, stores embeddings and computes cosine
//! similarity for hybrid search. Falls back to keyword-only (FTS5/BM25) when no
//! embedding provider is available.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use eyre::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};
use workforce_common::secret::{current_key, ApiKeySource};

use crate::shape::{layer_to_string, string_to_layer, string_to_visibility, visibility_to_string};

use crate::embedding::{EmbeddingBackend, OpenAiEmbeddingBackend};
use crate::store::{apply_pollution_default, passes_excluded_tags, MemoryStore, IMPORTANCE_FLOOR};
use crate::types::{Memory, MemoryFilter, MemoryId, MemorySearchResult, MemorySource, Visibility};
use workforce_core::types::AgentId;

// ─── Configuration ───────────────────────────────────────────────────────────

/// Configuration for the SQLite memory store.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SqliteMemoryConfig {
    /// Path to the SQLite database file.
    #[serde(default = "default_db_path")]
    pub db_path: String,
    /// Embedding provider: "openai" or "none".
    #[serde(default = "default_embedding_provider")]
    pub embedding_provider: String,
    /// Embedding model name (e.g., "text-embedding-3-small").
    #[serde(default = "default_embedding_model")]
    pub embedding_model: String,
    /// Base URL for the embedding API (OpenAI-compatible).
    #[serde(default = "default_embedding_base_url")]
    pub embedding_base_url: String,
    /// API key for the embedding provider (falls back to OPENAI_API_KEY env var).
    #[serde(default)]
    pub embedding_api_key: Option<String>,
    /// Weight for vector similarity in hybrid scoring (0.0 to 1.0).
    #[serde(default = "default_vector_weight")]
    pub vector_weight: f32,
}

impl Default for SqliteMemoryConfig {
    fn default() -> Self {
        Self {
            db_path: default_db_path(),
            embedding_provider: default_embedding_provider(),
            embedding_model: default_embedding_model(),
            embedding_base_url: default_embedding_base_url(),
            embedding_api_key: None,
            vector_weight: default_vector_weight(),
        }
    }
}

fn default_db_path() -> String {
    "data/memory.db".to_string()
}

fn default_embedding_provider() -> String {
    "none".to_string()
}

fn default_embedding_model() -> String {
    "text-embedding-3-small".to_string()
}

fn default_embedding_base_url() -> String {
    "https://api.openai.com".to_string()
}

fn default_vector_weight() -> f32 {
    0.6
}

// ─── Backend Resolution ──────────────────────────────────────────────────────

/// Build an [`EmbeddingBackend`] from a `SqliteMemoryConfig`. Supports the
/// "openai" / "openai-compatible" family today; "none" (the default) returns
/// `Ok(None)` so callers get BM25-only recall. Unknown providers log a warning
/// and fall back to keyword-only rather than failing startup. `api_key` is
/// asked on every request; a provider it has no key for at open is an error.
fn backend_from_config(
    config: &SqliteMemoryConfig,
    api_key: ApiKeySource,
) -> Result<Option<Arc<dyn EmbeddingBackend>>> {
    match config.embedding_provider.as_str() {
        "openai" | "openai-compatible" => {
            if current_key(&api_key).is_none() {
                eyre::bail!(
                    "Embedding provider '{}' requires an API key (set embedding_api_key or OPENAI_API_KEY)",
                    config.embedding_provider
                );
            }
            Ok(Some(Arc::new(OpenAiEmbeddingBackend::with_key_source(
                &config.embedding_base_url,
                &config.embedding_model,
                api_key,
            ))))
        }
        "none" | "" => Ok(None),
        other => {
            warn!(
                provider = other,
                "Unknown embedding provider, falling back to keyword-only search"
            );
            Ok(None)
        }
    }
}

/// The config's key, else `OPENAI_API_KEY`.
fn configured_key(config: &SqliteMemoryConfig) -> ApiKeySource {
    let configured = config.embedding_api_key.clone();
    Arc::new(move || {
        configured
            .clone()
            .or_else(|| std::env::var("OPENAI_API_KEY").ok())
    })
}

// ─── Vector Utilities ────────────────────────────────────────────────────────

/// Encode a float vector as a little-endian byte blob for SQLite storage.
fn encode_embedding(embedding: &[f32]) -> Vec<u8> {
    embedding.iter().flat_map(|f| f.to_le_bytes()).collect()
}

/// Decode a little-endian byte blob back into a float vector.
fn decode_embedding(blob: &[u8]) -> Vec<f32> {
    blob.chunks_exact(4)
        .map(|chunk| {
            let bytes: [u8; 4] = [chunk[0], chunk[1], chunk[2], chunk[3]];
            f32::from_le_bytes(bytes)
        })
        .collect()
}

/// Compute cosine similarity between two vectors. Returns 0.0 for zero-length vectors.
fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }

    let (dot, mag_a, mag_b) =
        a.iter()
            .zip(b.iter())
            .fold((0.0_f64, 0.0_f64, 0.0_f64), |(dot, ma, mb), (x, y)| {
                let x = *x as f64;
                let y = *y as f64;
                (dot + x * y, ma + x * x, mb + y * y)
            });

    let denom = mag_a.sqrt() * mag_b.sqrt();
    if denom < 1e-10 {
        0.0
    } else {
        (dot / denom) as f32
    }
}

// ─── SQLite Memory Store ─────────────────────────────────────────────────────

/// SQLite-backed memory store with FTS5 full-text search and optional vector embeddings.
pub struct SqliteMemoryStore {
    conn: Mutex<Connection>,
    embedding: Option<Arc<dyn EmbeddingBackend>>,
    vector_weight: f32,
    /// Path to the on-disk database file. `None` for in-memory test stores.
    /// Used by `cache_stats()` to expose the db file size for memory
    /// observability (SQLite mmaps part of this file into RAM).
    db_path: Option<std::path::PathBuf>,
}

impl SqliteMemoryStore {
    /// Open (or create) a SQLite memory store at the given path. Resolves the
    /// embedding backend from config (OpenAI-compatible today).
    pub fn open(config: &SqliteMemoryConfig) -> Result<Self> {
        Self::open_with_key_source(config, configured_key(config))
    }

    /// [`Self::open`] with the embedding provider's key read from `api_key`
    /// on every request instead of from the config.
    pub fn open_with_key_source(
        config: &SqliteMemoryConfig,
        api_key: ApiKeySource,
    ) -> Result<Self> {
        let backend = backend_from_config(config, api_key)?;
        Self::open_with_backend(config, backend)
    }

    /// Open a SQLite memory store with an explicit [`EmbeddingBackend`]. Use
    /// this when the caller wants to inject a backend the config shape can't
    /// describe (e.g. an in-process fastembed-rs instance).
    pub fn open_with_backend(
        config: &SqliteMemoryConfig,
        backend: Option<Arc<dyn EmbeddingBackend>>,
    ) -> Result<Self> {
        let db_path = Path::new(&config.db_path);
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)
                .wrap_err_with(|| format!("Failed to create directory: {}", parent.display()))?;
        }

        let conn = Connection::open(db_path)
            .wrap_err_with(|| format!("Failed to open SQLite database: {}", config.db_path))?;

        Self::from_parts(conn, config, backend, Some(db_path.to_path_buf()))
    }

    /// Create an in-memory SQLite store (useful for testing).
    pub fn in_memory(config: &SqliteMemoryConfig) -> Result<Self> {
        let backend = backend_from_config(config, configured_key(config))?;
        Self::in_memory_with_backend(config, backend)
    }

    /// In-memory variant of [`open_with_backend`].
    pub fn in_memory_with_backend(
        config: &SqliteMemoryConfig,
        backend: Option<Arc<dyn EmbeddingBackend>>,
    ) -> Result<Self> {
        let conn =
            Connection::open_in_memory().wrap_err("Failed to open in-memory SQLite database")?;
        Self::from_parts(conn, config, backend, None)
    }

    fn from_parts(
        conn: Connection,
        config: &SqliteMemoryConfig,
        embedding: Option<Arc<dyn EmbeddingBackend>>,
        db_path: Option<std::path::PathBuf>,
    ) -> Result<Self> {
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")
            .wrap_err("Failed to set SQLite pragmas")?;

        Self::init_schema(&conn)?;

        if embedding.is_some() {
            info!(
                model = %config.embedding_model,
                "Memory store embedding backend enabled"
            );
        } else {
            info!("No embedding backend configured — using keyword-only search");
        }

        Ok(Self {
            conn: Mutex::new(conn),
            embedding,
            vector_weight: config.vector_weight,
            db_path,
        })
    }

    /// Initialize the database schema (memories table + FTS5 index).
    fn init_schema(conn: &Connection) -> Result<()> {
        conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS memories (
                id                  TEXT PRIMARY KEY,
                content             TEXT NOT NULL,
                layer               TEXT NOT NULL,
                owner               TEXT NOT NULL,
                visibility          TEXT NOT NULL DEFAULT 'private',
                importance          REAL NOT NULL DEFAULT 0.5,
                tags                TEXT NOT NULL DEFAULT '[]',
                created_at          TEXT NOT NULL,
                last_accessed       TEXT NOT NULL,
                access_count        INTEGER NOT NULL DEFAULT 0,
                deleted             INTEGER NOT NULL DEFAULT 0,
                embedding           BLOB,
                source              TEXT NOT NULL DEFAULT 'unknown',
                identity_id         TEXT,
                source_session_id   TEXT,
                source_message_ids  TEXT NOT NULL DEFAULT '[]',
                recurrence_count    INTEGER NOT NULL DEFAULT 0,
                last_recurrence_at  TEXT,
                event_time          TEXT
            );

            CREATE INDEX IF NOT EXISTS idx_memories_owner ON memories(owner);
            CREATE INDEX IF NOT EXISTS idx_memories_layer ON memories(layer);
            CREATE INDEX IF NOT EXISTS idx_memories_deleted ON memories(deleted);

            CREATE VIRTUAL TABLE IF NOT EXISTS memories_fts USING fts5(
                content,
                content='memories',
                content_rowid='rowid'
            );

            -- Triggers to keep FTS5 index in sync with the memories table.
            CREATE TRIGGER IF NOT EXISTS memories_ai AFTER INSERT ON memories BEGIN
                INSERT INTO memories_fts(rowid, content) VALUES (new.rowid, new.content);
            END;
            CREATE TRIGGER IF NOT EXISTS memories_ad AFTER DELETE ON memories BEGIN
                INSERT INTO memories_fts(memories_fts, rowid, content) VALUES('delete', old.rowid, old.content);
            END;
            CREATE TRIGGER IF NOT EXISTS memories_au AFTER UPDATE ON memories BEGIN
                INSERT INTO memories_fts(memories_fts, rowid, content) VALUES('delete', old.rowid, old.content);
                INSERT INTO memories_fts(rowid, content) VALUES (new.rowid, new.content);
            END;
            ",
        )
        .wrap_err("Failed to initialize SQLite schema")?;

        // Migrate: add columns that may not exist in older databases.
        for (col, default) in [
            ("source", "TEXT NOT NULL DEFAULT 'unknown'"),
            ("identity_id", "TEXT"),
            ("source_session_id", "TEXT"),
            ("source_message_ids", "TEXT NOT NULL DEFAULT '[]'"),
            ("recurrence_count", "INTEGER NOT NULL DEFAULT 0"),
            ("last_recurrence_at", "TEXT"),
            ("event_time", "TEXT"),
        ] {
            let _ =
                conn.execute_batch(&format!("ALTER TABLE memories ADD COLUMN {col} {default};"));
        }
        let _ = conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS idx_memories_source ON memories(source);
             CREATE INDEX IF NOT EXISTS idx_memories_identity ON memories(identity_id);
             CREATE INDEX IF NOT EXISTS idx_memories_source_session ON memories(source_session_id);",
        );

        Ok(())
    }

    /// Optionally generate an embedding for the given text.
    async fn maybe_embed(&self, text: &str) -> Option<Vec<f32>> {
        let backend = self.embedding.as_ref()?;
        match backend.embed(text).await {
            Ok(embedding) => Some(embedding),
            Err(e) => {
                warn!(error = %e, "Failed to generate embedding, falling back to keyword-only");
                None
            }
        }
    }

    /// Build a SQL WHERE clause fragment from a MemoryFilter.
    ///
    /// `param_offset` is the number of parameters already bound in the query
    /// (e.g., 1 when the FTS MATCH clause already uses `?1`).
    fn build_where_clause(
        filter: &MemoryFilter,
        param_offset: usize,
    ) -> (String, Vec<Box<dyn rusqlite::types::ToSql>>) {
        let mut conditions: Vec<String> = vec!["deleted = 0".to_string()];
        let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();

        if let Some(ref layer) = filter.layer {
            conditions.push(format!("layer = ?{}", param_offset + params.len() + 1));
            params.push(Box::new(layer_to_string(layer).to_string()));
        }

        if let Some(ref owner) = filter.owner {
            conditions.push(format!("owner = ?{}", param_offset + params.len() + 1));
            params.push(Box::new(owner.as_str().to_string()));
        }

        if let Some(ref vis) = filter.visibility {
            conditions.push(format!("visibility = ?{}", param_offset + params.len() + 1));
            params.push(Box::new(visibility_to_string(vis)));
        }

        if let Some(ref tags) = filter.tags {
            // Match any tag: use JSON extraction with LIKE
            let tag_conditions: Vec<String> = tags
                .iter()
                .enumerate()
                .map(|(i, _)| {
                    let idx = param_offset + params.len() + i + 1;
                    format!("tags LIKE ?{idx}")
                })
                .collect();
            if !tag_conditions.is_empty() {
                conditions.push(format!("({})", tag_conditions.join(" OR ")));
                for tag in tags {
                    params.push(Box::new(format!("%\"{}\"%", tag)));
                }
            }
        }

        if let Some(min_imp) = filter.min_importance {
            conditions.push(format!(
                "importance >= ?{}",
                param_offset + params.len() + 1
            ));
            params.push(Box::new(min_imp as f64));
        }

        if let Some(ref source) = filter.source {
            conditions.push(format!("source = ?{}", param_offset + params.len() + 1));
            params.push(Box::new(source.as_str().to_string()));
        }

        if let Some(ref identity_id) = filter.identity_id {
            let uid_param = param_offset + params.len() + 1;
            if let Some(ref team_id) = filter.team_id {
                let tid_param = uid_param + 1;
                conditions.push(format!(
                    "(identity_id = ?{uid_param} OR visibility = ?{tid_param})"
                ));
                params.push(Box::new(identity_id.clone()));
                params.push(Box::new(format!("team:{team_id}")));
            } else {
                conditions.push(format!("identity_id = ?{uid_param}"));
                params.push(Box::new(identity_id.clone()));
            }
        }

        if let Some(ref after) = filter.created_after {
            conditions.push(format!(
                "created_at >= ?{}",
                param_offset + params.len() + 1
            ));
            params.push(Box::new(after.to_rfc3339()));
        }

        if let Some(ref before) = filter.created_before {
            conditions.push(format!(
                "created_at <= ?{}",
                param_offset + params.len() + 1
            ));
            params.push(Box::new(before.to_rfc3339()));
        }

        if let Some(ref after) = filter.event_after {
            // COALESCE so legacy rows where event_time is NULL fall back to
            // created_at — the conservative interpretation: a memory whose
            // event time was never recorded behaves as if it happened when
            // it was written.
            conditions.push(format!(
                "COALESCE(event_time, created_at) >= ?{}",
                param_offset + params.len() + 1
            ));
            params.push(Box::new(after.to_rfc3339()));
        }

        if let Some(ref before) = filter.event_before {
            conditions.push(format!(
                "COALESCE(event_time, created_at) <= ?{}",
                param_offset + params.len() + 1
            ));
            params.push(Box::new(before.to_rfc3339()));
        }

        let where_clause = conditions.join(" AND ");
        (where_clause, params)
    }

    /// Parse a row from the memories table into a Memory struct.
    fn parse_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Memory> {
        let id: String = row.get("id")?;
        let content: String = row.get("content")?;
        let layer_str: String = row.get("layer")?;
        let owner_str: String = row.get("owner")?;
        let visibility_str: String = row.get("visibility")?;
        let importance: f64 = row.get("importance")?;
        let tags_json: String = row.get("tags")?;
        let created_at_str: String = row.get("created_at")?;
        let last_accessed_str: String = row.get("last_accessed")?;
        let access_count: u32 = row.get("access_count")?;
        let deleted: bool = row.get("deleted")?;
        let source_str: String = row
            .get::<_, String>("source")
            .unwrap_or_else(|_| "unknown".to_string());
        let identity_id: Option<String> = row.get("identity_id").unwrap_or(None);
        let source_session_id: Option<String> = row.get("source_session_id").unwrap_or(None);
        let source_message_ids_json: String = row
            .get::<_, String>("source_message_ids")
            .unwrap_or_else(|_| "[]".to_string());
        let source_message_ids: Vec<String> =
            serde_json::from_str(&source_message_ids_json).unwrap_or_default();
        let recurrence_count: u32 = row.get("recurrence_count").unwrap_or(0);
        let last_recurrence_at: Option<DateTime<Utc>> = row
            .get::<_, Option<String>>("last_recurrence_at")
            .ok()
            .flatten()
            .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
            .map(|dt| dt.with_timezone(&Utc));
        let event_time_str: Option<String> = row.get("event_time").unwrap_or(None);

        let tags: Vec<String> = serde_json::from_str(&tags_json).unwrap_or_default();
        let created_at = DateTime::parse_from_rfc3339(&created_at_str)
            .map(|dt| dt.with_timezone(&Utc))
            .unwrap_or_else(|_| Utc::now());
        let event_time = event_time_str
            .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
            .map(|dt| dt.with_timezone(&Utc))
            .unwrap_or(created_at);
        let last_accessed = DateTime::parse_from_rfc3339(&last_accessed_str)
            .map(|dt| dt.with_timezone(&Utc))
            .unwrap_or_else(|_| Utc::now());

        Ok(Memory {
            id: MemoryId(id),
            content,
            layer: string_to_layer(&layer_str),
            owner: AgentId::new(owner_str),
            visibility: string_to_visibility(&visibility_str),
            importance: importance as f32,
            tags,
            source: MemorySource::from_str_lossy(&source_str),
            identity_id,
            source_session_id,
            source_message_ids,
            created_at,
            event_time,
            last_accessed,
            access_count,
            recurrence_count,
            last_recurrence_at,
            deleted,
        })
    }
}

#[async_trait]
impl MemoryStore for SqliteMemoryStore {
    fn cache_stats(&self) -> crate::store::MemoryStoreCacheStats {
        let db_bytes = self
            .db_path
            .as_ref()
            .and_then(|p| std::fs::metadata(p).ok())
            .map(|m| m.len())
            .unwrap_or(0);
        crate::store::MemoryStoreCacheStats { db_bytes }
    }

    async fn remember(&self, memory: Memory) -> Result<MemoryId> {
        let embedding = self.maybe_embed(&memory.content).await;
        let embedding_blob = embedding.as_deref().map(encode_embedding);

        let id = memory.id.clone();
        let tags_json = serde_json::to_string(&memory.tags).wrap_err("Failed to serialize tags")?;
        let created_at = memory.created_at.to_rfc3339();
        let last_accessed = memory.last_accessed.to_rfc3339();
        let layer = layer_to_string(&memory.layer).to_string();
        let owner = memory.owner.as_str().to_string();
        let visibility = visibility_to_string(&memory.visibility);

        let conn = self
            .conn
            .lock()
            .map_err(|e| eyre::eyre!("Failed to lock database: {}", e))?;

        let source = memory.source.as_str().to_string();
        let identity_id = memory.identity_id.clone();
        let source_session_id = memory.source_session_id.clone();
        let source_message_ids_json = serde_json::to_string(&memory.source_message_ids)
            .wrap_err("Failed to serialize source_message_ids")?;

        let last_recurrence_at = memory.last_recurrence_at.map(|dt| dt.to_rfc3339());
        let event_time = memory.event_time.to_rfc3339();

        conn.execute(
            "INSERT OR REPLACE INTO memories (id, content, layer, owner, visibility, importance, tags, created_at, last_accessed, access_count, deleted, embedding, source, identity_id, source_session_id, source_message_ids, recurrence_count, last_recurrence_at, event_time)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19)",
            params![
                id.0,
                memory.content,
                layer,
                owner,
                visibility,
                memory.importance as f64,
                tags_json,
                created_at,
                last_accessed,
                memory.access_count,
                memory.deleted,
                embedding_blob,
                source,
                identity_id,
                source_session_id,
                source_message_ids_json,
                memory.recurrence_count,
                last_recurrence_at,
                event_time,
            ],
        )
        .wrap_err("Failed to insert memory")?;

        debug!(memory_id = %id, "Stored memory in SQLite");
        Ok(id)
    }

    async fn recall(
        &self,
        query: &str,
        mut filter: MemoryFilter,
    ) -> Result<Vec<MemorySearchResult>> {
        if !crate::store::check_and_audit_scope(
            &filter,
            crate::access_audit::AuditOperation::Recall,
        ) {
            return Ok(Vec::new());
        }
        apply_pollution_default(&mut filter);
        let limit = filter.limit.unwrap_or(10);
        let query_embedding = self.maybe_embed(query).await;
        let vector_weight = self.vector_weight;

        let conn = self
            .conn
            .lock()
            .map_err(|e| eyre::eyre!("Failed to lock database: {}", e))?;

        // Step 1: FTS5 keyword search — get BM25 scores for matching rows.
        // The FTS query uses ?1 for the MATCH term, so filter params start at offset 1.
        let mut fts_scores: HashMap<i64, f64> = HashMap::new();
        if !query.trim().is_empty() {
            let (fts_where, fts_params) = Self::build_where_clause(&filter, 1);

            // Escape FTS5 query: wrap each word in double quotes for safe matching
            let fts_query = query
                .split_whitespace()
                .map(|w| format!("\"{}\"", w.replace('"', "\"\"")))
                .collect::<Vec<_>>()
                .join(" OR ");

            let fts_sql = format!(
                "SELECT m.rowid, bm25(memories_fts) AS rank
                 FROM memories_fts
                 JOIN memories m ON m.rowid = memories_fts.rowid
                 WHERE memories_fts MATCH ?1 AND {fts_where}"
            );

            let mut param_values: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
            param_values.push(Box::new(fts_query));
            for p in &fts_params {
                param_values.push(clone_sql_param(p.as_ref()));
            }

            let param_refs: Vec<&dyn rusqlite::types::ToSql> =
                param_values.iter().map(|p| p.as_ref()).collect();

            let mut stmt = conn
                .prepare(&fts_sql)
                .wrap_err("Failed to prepare FTS query")?;
            let rows = stmt
                .query_map(param_refs.as_slice(), |row| {
                    let rowid: i64 = row.get(0)?;
                    let rank: f64 = row.get(1)?;
                    Ok((rowid, rank))
                })
                .wrap_err("Failed to execute FTS query")?;

            for row in rows {
                let (rowid, rank) = row.wrap_err("Failed to read FTS row")?;
                fts_scores.insert(rowid, rank);
            }
        }

        // Step 2: Fetch candidate memories (FTS hits + fallback scan for vector-only search).
        // Candidate query has no extra prefix params, so offset is 0.
        let (cand_where, cand_params) = Self::build_where_clause(&filter, 0);

        let candidates_sql = if fts_scores.is_empty() && query_embedding.is_some() {
            // No FTS hits but we have embeddings — scan all non-deleted memories with embeddings
            format!(
                "SELECT rowid, id, content, layer, owner, visibility, importance, tags,
                        created_at, last_accessed, access_count, deleted, embedding,
                        source, identity_id, source_session_id, source_message_ids,
                        recurrence_count, last_recurrence_at, event_time
                 FROM memories
                 WHERE {cand_where} AND embedding IS NOT NULL
                 LIMIT 1000"
            )
        } else if fts_scores.is_empty() {
            // No FTS hits and no embeddings — return empty
            return Ok(Vec::new());
        } else {
            // Have FTS hits — fetch those specific rows
            let rowid_list = fts_scores
                .keys()
                .map(|r| r.to_string())
                .collect::<Vec<_>>()
                .join(",");
            format!(
                "SELECT rowid, id, content, layer, owner, visibility, importance, tags,
                        created_at, last_accessed, access_count, deleted, embedding,
                        source, identity_id, source_session_id, source_message_ids,
                        recurrence_count, last_recurrence_at, event_time
                 FROM memories
                 WHERE rowid IN ({rowid_list}) AND {cand_where}"
            )
        };

        let candidate_refs: Vec<&dyn rusqlite::types::ToSql> =
            cand_params.iter().map(|p| p.as_ref()).collect();

        let mut stmt = conn
            .prepare(&candidates_sql)
            .wrap_err("Failed to prepare candidate query")?;

        let mut results: Vec<MemorySearchResult> = Vec::new();

        let rows = stmt
            .query_map(candidate_refs.as_slice(), |row| {
                let rowid: i64 = row.get(0)?;
                let memory = Self::parse_row_with_offset(row, 1)?;
                let embedding_blob: Option<Vec<u8>> = row.get(12)?;
                Ok((rowid, memory, embedding_blob))
            })
            .wrap_err("Failed to execute candidate query")?;

        for row in rows {
            let (rowid, memory, embedding_blob) = row.wrap_err("Failed to read candidate row")?;

            // Compute hybrid score
            let keyword_score = fts_scores.get(&rowid).copied().unwrap_or(0.0);
            // BM25 returns negative values (more negative = better match), normalise to 0..1
            let normalised_keyword = if keyword_score < 0.0 {
                (-keyword_score).min(20.0) / 20.0
            } else {
                0.0
            };

            let vector_score = match (&query_embedding, &embedding_blob) {
                (Some(qe), Some(blob)) => {
                    let stored = decode_embedding(blob);
                    cosine_similarity(qe, &stored) as f64
                }
                _ => 0.0,
            };

            // Hybrid: weighted combination
            let score = if query_embedding.is_some() && embedding_blob.is_some() {
                let vw = vector_weight as f64;
                vw * vector_score + (1.0 - vw) * normalised_keyword
            } else {
                normalised_keyword
            };

            if score > 0.0 {
                results.push(MemorySearchResult {
                    memory,
                    score: score as f32,
                });
            }
        }

        // Sort by score descending, then drop excluded-tag matches BEFORE
        // truncating so the limit reflects post-filter results, not raw
        // recall hits.
        results.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        results.retain(|r| passes_excluded_tags(&r.memory, &filter.excluded_tags));
        results.truncate(limit);

        Ok(crate::store::enforce_access_control(
            &filter,
            crate::access_audit::AuditOperation::Recall,
            results,
        ))
    }

    async fn get(&self, id: &MemoryId) -> Result<Option<Memory>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| eyre::eyre!("Failed to lock database: {}", e))?;

        let result = conn
            .query_row(
                "SELECT id, content, layer, owner, visibility, importance, tags,
                        created_at, last_accessed, access_count, deleted,
                        source, identity_id, source_session_id, source_message_ids,
                        recurrence_count, last_recurrence_at, event_time
                 FROM memories WHERE id = ?1",
                params![id.0],
                Self::parse_row,
            )
            .optional()
            .wrap_err("Failed to query memory by ID")?;

        Ok(result)
    }

    async fn forget(&self, id: &MemoryId) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| eyre::eyre!("Failed to lock database: {}", e))?;

        conn.execute(
            "UPDATE memories SET deleted = 1 WHERE id = ?1",
            params![id.0],
        )
        .wrap_err("Failed to soft-delete memory")?;

        debug!(memory_id = %id, "Soft-deleted memory in SQLite");
        Ok(())
    }

    async fn share(&self, id: &MemoryId, visibility: Visibility) -> Result<()> {
        let vis_str = visibility_to_string(&visibility);
        let conn = self
            .conn
            .lock()
            .map_err(|e| eyre::eyre!("Failed to lock database: {}", e))?;

        conn.execute(
            "UPDATE memories SET visibility = ?1 WHERE id = ?2",
            params![vis_str, id.0],
        )
        .wrap_err("Failed to update memory visibility")?;

        Ok(())
    }

    async fn count(&self, filter: MemoryFilter) -> Result<usize> {
        if !crate::store::check_and_audit_scope(&filter, crate::access_audit::AuditOperation::Count)
        {
            return Ok(0);
        }
        let (where_clause, where_params) = Self::build_where_clause(&filter, 0);
        let sql = format!("SELECT COUNT(*) FROM memories WHERE {where_clause}");

        let conn = self
            .conn
            .lock()
            .map_err(|e| eyre::eyre!("Failed to lock database: {}", e))?;

        let param_refs: Vec<&dyn rusqlite::types::ToSql> =
            where_params.iter().map(|p| p.as_ref()).collect();

        let count: usize = conn
            .query_row(&sql, param_refs.as_slice(), |row| row.get(0))
            .wrap_err("Failed to count memories")?;

        Ok(count)
    }

    async fn list(&self, mut filter: MemoryFilter) -> Result<Vec<Memory>> {
        if !crate::store::check_and_audit_scope(&filter, crate::access_audit::AuditOperation::List)
        {
            return Ok(Vec::new());
        }
        apply_pollution_default(&mut filter);
        let limit = filter.limit.unwrap_or(100);
        let (where_clause, where_params) = Self::build_where_clause(&filter, 0);
        let sql = format!(
            "SELECT id, content, layer, owner, visibility, importance, tags, \
             created_at, last_accessed, access_count, deleted, embedding, source, identity_id, \
             source_session_id, source_message_ids, recurrence_count, last_recurrence_at, event_time \
             FROM memories WHERE {where_clause} ORDER BY importance DESC LIMIT {limit}"
        );

        let conn = self
            .conn
            .lock()
            .map_err(|e| eyre::eyre!("Failed to lock database: {}", e))?;

        let param_refs: Vec<&dyn rusqlite::types::ToSql> =
            where_params.iter().map(|p| p.as_ref()).collect();

        let mut stmt = conn
            .prepare(&sql)
            .wrap_err("Failed to prepare list query")?;
        let results: Vec<Memory> = stmt
            .query_map(param_refs.as_slice(), |row| {
                Self::parse_row_with_offset(row, 0)
            })
            .wrap_err("Failed to list memories")?
            .filter_map(|r| r.ok())
            .filter(|m| passes_excluded_tags(m, &filter.excluded_tags))
            .collect();

        Ok(crate::store::enforce_access_control(
            &filter,
            crate::access_audit::AuditOperation::List,
            results,
        ))
    }

    async fn update_tags(&self, id: &MemoryId, tags: Vec<String>) -> Result<()> {
        let tags_json = serde_json::to_string(&tags)?;
        let conn = self
            .conn
            .lock()
            .map_err(|e| eyre::eyre!("Failed to lock database: {}", e))?;
        conn.execute(
            "UPDATE memories SET tags = ?1 WHERE id = ?2",
            rusqlite::params![tags_json, id.0],
        )?;
        Ok(())
    }

    async fn identity_memories(&self, owner: &AgentId, identity: &str) -> Result<Vec<MemoryId>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| eyre::eyre!("Failed to lock database: {}", e))?;
        let mut stmt =
            conn.prepare("SELECT id FROM memories WHERE owner = ?1 AND identity_id = ?2")?;
        let ids = stmt
            .query_map(params![owner.as_str(), identity], |row| {
                row.get::<_, String>(0).map(MemoryId)
            })?
            .collect::<rusqlite::Result<_>>()?;
        Ok(ids)
    }

    async fn identity_counts(
        &self,
        owner: &AgentId,
    ) -> Result<std::collections::BTreeMap<String, usize>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| eyre::eyre!("Failed to lock database: {}", e))?;
        let mut stmt = conn.prepare(
            "SELECT identity_id, COUNT(*) FROM memories
             WHERE owner = ?1 AND identity_id IS NOT NULL AND identity_id <> ''
             GROUP BY identity_id",
        )?;
        let counts = stmt
            .query_map(params![owner.as_str()], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    usize::try_from(row.get::<_, i64>(1)?).unwrap_or_default(),
                ))
            })?
            .collect::<rusqlite::Result<_>>()?;
        Ok(counts)
    }

    async fn reassign_identity(&self, owner: &AgentId, from: &str, to: &str) -> Result<usize> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| eyre::eyre!("Failed to lock database: {}", e))?;
        Ok(conn.execute(
            "UPDATE memories SET identity_id = ?3 WHERE owner = ?1 AND identity_id = ?2",
            params![owner.as_str(), from, to],
        )?)
    }

    async fn purge_identity(&self, owner: &AgentId, identity: &str) -> Result<usize> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| eyre::eyre!("Failed to lock database: {}", e))?;
        Ok(conn.execute(
            "DELETE FROM memories WHERE owner = ?1 AND identity_id = ?2",
            params![owner.as_str(), identity],
        )?)
    }

    async fn purge_memories(&self, ids: &[MemoryId]) -> Result<usize> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| eyre::eyre!("Failed to lock database: {}", e))?;
        let mut purged = 0;
        for id in ids {
            purged += conn.execute("DELETE FROM memories WHERE id = ?1", params![id.0])?;
        }
        Ok(purged)
    }

    async fn adjust_importance(&self, id: &MemoryId, delta: f32) -> Result<f32> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| eyre::eyre!("Failed to lock database: {}", e))?;
        let new_importance: f64 = conn
            .query_row(
                "UPDATE memories
                 SET importance = MIN(?1, MAX(?2, importance + ?3))
                 WHERE id = ?4
                 RETURNING importance",
                params![1.0_f64, IMPORTANCE_FLOOR as f64, delta as f64, id.0],
                |row| row.get(0),
            )
            .optional()?
            .ok_or_else(|| eyre::eyre!("memory {} not found", id.0))?;
        Ok(new_importance as f32)
    }

    async fn record_recurrence(
        &self,
        id: &MemoryId,
        additional_message_ids: Vec<String>,
    ) -> Result<u32> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| eyre::eyre!("Failed to lock database: {}", e))?;

        let existing_json: Option<String> = conn
            .query_row(
                "SELECT source_message_ids FROM memories WHERE id = ?1",
                params![id.0],
                |row| row.get(0),
            )
            .optional()?;
        let existing_json =
            existing_json.ok_or_else(|| eyre::eyre!("memory {} not found", id.0))?;
        let mut existing: Vec<String> = serde_json::from_str(&existing_json).unwrap_or_default();
        for new_id in additional_message_ids {
            if !new_id.is_empty() && !existing.contains(&new_id) {
                existing.push(new_id);
            }
        }
        let merged_json = serde_json::to_string(&existing)?;

        let now = chrono::Utc::now().to_rfc3339();
        conn.execute(
            "UPDATE memories
             SET recurrence_count = recurrence_count + 1,
                 last_recurrence_at = ?1,
                 last_accessed = ?1,
                 source_message_ids = ?2
             WHERE id = ?3",
            params![now, merged_json, id.0],
        )?;

        let new_count: u32 = conn.query_row(
            "SELECT recurrence_count FROM memories WHERE id = ?1",
            params![id.0],
            |row| row.get(0),
        )?;
        Ok(new_count)
    }
}

impl SqliteMemoryStore {
    /// Parse a row starting at a given column offset (for queries that include rowid first).
    fn parse_row_with_offset(row: &rusqlite::Row<'_>, offset: usize) -> rusqlite::Result<Memory> {
        let id: String = row.get(offset)?;
        let content: String = row.get(offset + 1)?;
        let layer_str: String = row.get(offset + 2)?;
        let owner_str: String = row.get(offset + 3)?;
        let visibility_str: String = row.get(offset + 4)?;
        let importance: f64 = row.get(offset + 5)?;
        let tags_json: String = row.get(offset + 6)?;
        let created_at_str: String = row.get(offset + 7)?;
        let last_accessed_str: String = row.get(offset + 8)?;
        let access_count: u32 = row.get(offset + 9)?;
        let deleted: bool = row.get(offset + 10)?;
        // offset+11 = embedding (handled separately by caller)
        let source_str: String = row
            .get(offset + 12)
            .unwrap_or_else(|_| "unknown".to_string());
        let identity_id: Option<String> = row.get(offset + 13).unwrap_or(None);
        let source_session_id: Option<String> = row.get(offset + 14).unwrap_or(None);
        let source_message_ids_json: String = row
            .get::<_, String>(offset + 15)
            .unwrap_or_else(|_| "[]".to_string());
        let source_message_ids: Vec<String> =
            serde_json::from_str(&source_message_ids_json).unwrap_or_default();
        let recurrence_count: u32 = row.get(offset + 16).unwrap_or(0);
        let last_recurrence_at: Option<DateTime<Utc>> = row
            .get::<_, Option<String>>(offset + 17)
            .ok()
            .flatten()
            .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
            .map(|dt| dt.with_timezone(&Utc));
        let event_time_str: Option<String> = row.get(offset + 18).unwrap_or(None);

        let tags: Vec<String> = serde_json::from_str(&tags_json).unwrap_or_default();
        let created_at = DateTime::parse_from_rfc3339(&created_at_str)
            .map(|dt| dt.with_timezone(&Utc))
            .unwrap_or_else(|_| Utc::now());
        let event_time = event_time_str
            .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
            .map(|dt| dt.with_timezone(&Utc))
            .unwrap_or(created_at);
        let last_accessed = DateTime::parse_from_rfc3339(&last_accessed_str)
            .map(|dt| dt.with_timezone(&Utc))
            .unwrap_or_else(|_| Utc::now());

        Ok(Memory {
            id: MemoryId(id),
            content,
            layer: string_to_layer(&layer_str),
            owner: AgentId::new(owner_str),
            visibility: string_to_visibility(&visibility_str),
            importance: importance as f32,
            tags,
            source: MemorySource::from_str_lossy(&source_str),
            identity_id,
            source_session_id,
            source_message_ids,
            created_at,
            event_time,
            last_accessed,
            access_count,
            recurrence_count,
            last_recurrence_at,
            deleted,
        })
    }
}

// ─── Conversion Helpers ──────────────────────────────────────────────────────

/// Helper to "clone" a boxed ToSql param. We only use String and f64 params.
fn clone_sql_param(p: &dyn rusqlite::types::ToSql) -> Box<dyn rusqlite::types::ToSql> {
    use rusqlite::types::{ToSqlOutput, ValueRef};
    // Convert to a Value and re-box
    match p
        .to_sql()
        .unwrap_or(ToSqlOutput::Owned(rusqlite::types::Value::Null))
    {
        ToSqlOutput::Borrowed(ValueRef::Text(bytes)) => {
            Box::new(String::from_utf8_lossy(bytes).to_string())
        }
        ToSqlOutput::Owned(rusqlite::types::Value::Text(s)) => Box::new(s),
        ToSqlOutput::Owned(rusqlite::types::Value::Real(f)) => Box::new(f),
        ToSqlOutput::Owned(rusqlite::types::Value::Integer(i)) => Box::new(i),
        _ => Box::new(rusqlite::types::Value::Null),
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::MemoryLayer;

    fn test_config() -> SqliteMemoryConfig {
        SqliteMemoryConfig {
            embedding_provider: "none".to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn an_openai_embedder_opens_only_with_a_key_at_its_source() {
        let config = SqliteMemoryConfig {
            embedding_provider: "openai".to_string(),
            ..Default::default()
        };
        let open = |key: Option<&'static str>| {
            let source: ApiKeySource = Arc::new(move || key.map(str::to_string));
            backend_from_config(&config, source)
        };
        assert!(open(Some("sk-test")).unwrap().is_some());
        for missing in [None, Some(" ")] {
            let error = open(missing).err().unwrap().to_string();
            assert!(error.contains("requires an API key"), "{error}");
        }
        let keyword_only = SqliteMemoryConfig::default();
        assert!(backend_from_config(&keyword_only, Arc::new(|| None))
            .unwrap()
            .is_none());
    }

    fn make_store() -> SqliteMemoryStore {
        SqliteMemoryStore::in_memory(&test_config()).expect("Failed to create in-memory store")
    }

    fn make_memory(content: &str, layer: MemoryLayer, owner: &str) -> Memory {
        Memory::new(content, layer, AgentId::new(owner))
    }

    // ─── Storage & Retrieval ─────────────────────────────────────────────

    #[tokio::test]
    async fn remember_and_get() {
        let store = make_store();
        let mem = make_memory("Rust uses the borrow checker", MemoryLayer::Semantic, "dev");
        let id = store.remember(mem.clone()).await.expect("remember failed");

        let retrieved = store
            .get(&id)
            .await
            .expect("get failed")
            .expect("not found");
        assert_eq!(retrieved.content, "Rust uses the borrow checker");
        assert_eq!(retrieved.layer, MemoryLayer::Semantic);
        assert_eq!(retrieved.owner, AgentId::new("dev"));
    }

    #[tokio::test]
    async fn get_nonexistent_returns_none() {
        let store = make_store();
        let result = store
            .get(&MemoryId("nonexistent".to_string()))
            .await
            .expect("get failed");
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn remember_preserves_metadata() {
        let store = make_store();
        let mem = make_memory("important fact", MemoryLayer::Team, "arch")
            .with_importance(0.95)
            .with_tags(vec!["design".to_string(), "arch".to_string()])
            .with_visibility(Visibility::Team("backend".to_string()));

        let id = store.remember(mem).await.expect("remember failed");
        let retrieved = store
            .get(&id)
            .await
            .expect("get failed")
            .expect("not found");

        assert_eq!(retrieved.importance, 0.95);
        assert_eq!(retrieved.tags, vec!["design", "arch"]);
        assert_eq!(
            retrieved.visibility,
            Visibility::Team("backend".to_string())
        );
    }

    // ─── Keyword Search (FTS5) ──────────────────────────────────────────

    #[tokio::test]
    async fn recall_keyword_search() {
        let store = make_store();

        store
            .remember(make_memory(
                "Rust uses the borrow checker for memory safety",
                MemoryLayer::Semantic,
                "dev",
            ))
            .await
            .expect("remember failed");
        store
            .remember(make_memory(
                "PostgreSQL supports JSONB columns",
                MemoryLayer::Semantic,
                "dev",
            ))
            .await
            .expect("remember failed");

        let results = store
            .recall("borrow checker", MemoryFilter::system_unscoped())
            .await
            .expect("recall failed");

        assert_eq!(results.len(), 1);
        assert!(results[0].memory.content.contains("borrow checker"));
        assert!(results[0].score > 0.0);
    }

    #[tokio::test]
    async fn recall_no_results_for_unmatched_query() {
        let store = make_store();
        store
            .remember(make_memory("Rust is great", MemoryLayer::Semantic, "dev"))
            .await
            .expect("remember failed");

        let results = store
            .recall("dinosaurs", MemoryFilter::system_unscoped())
            .await
            .expect("recall failed");

        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn recall_multiple_results_ranked() {
        let store = make_store();

        store
            .remember(make_memory(
                "Rust error handling uses Result and Option types",
                MemoryLayer::Semantic,
                "dev",
            ))
            .await
            .expect("remember failed");
        store
            .remember(make_memory(
                "Error handling in Go uses multiple return values",
                MemoryLayer::Semantic,
                "dev",
            ))
            .await
            .expect("remember failed");
        store
            .remember(make_memory(
                "Python uses exceptions for error handling",
                MemoryLayer::Semantic,
                "dev",
            ))
            .await
            .expect("remember failed");

        let results = store
            .recall("error handling", MemoryFilter::system_unscoped())
            .await
            .expect("recall failed");

        assert_eq!(results.len(), 3);
        // All should have positive scores
        for r in &results {
            assert!(r.score > 0.0);
        }
    }

    // ─── Filter Tests ────────────────────────────────────────────────────

    #[tokio::test]
    async fn recall_with_layer_filter() {
        let store = make_store();

        store
            .remember(make_memory("Rust tip", MemoryLayer::Semantic, "dev"))
            .await
            .expect("remember failed");
        store
            .remember(make_memory("Rust experience", MemoryLayer::Episodic, "dev"))
            .await
            .expect("remember failed");

        let filter = MemoryFilter {
            layer: Some(MemoryLayer::Semantic),
            ..MemoryFilter::system_unscoped()
        };
        let results = store.recall("Rust", filter).await.expect("recall failed");

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].memory.layer, MemoryLayer::Semantic);
    }

    #[tokio::test]
    async fn recall_with_owner_filter() {
        let store = make_store();

        store
            .remember(make_memory(
                "Shared knowledge",
                MemoryLayer::Semantic,
                "alice",
            ))
            .await
            .expect("remember failed");
        store
            .remember(make_memory("Other knowledge", MemoryLayer::Semantic, "bob"))
            .await
            .expect("remember failed");

        let filter = MemoryFilter {
            owner: Some(AgentId::new("alice")),
            ..MemoryFilter::system_unscoped()
        };
        let results = store
            .recall("knowledge", filter)
            .await
            .expect("recall failed");

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].memory.owner, AgentId::new("alice"));
    }

    #[tokio::test]
    async fn recall_with_tag_filter() {
        let store = make_store();

        store
            .remember(
                make_memory("Tagged memory", MemoryLayer::Semantic, "dev")
                    .with_tags(vec!["rust".to_string(), "tips".to_string()]),
            )
            .await
            .expect("remember failed");
        store
            .remember(
                make_memory("Untagged memory", MemoryLayer::Semantic, "dev")
                    .with_tags(vec!["python".to_string()]),
            )
            .await
            .expect("remember failed");

        let filter = MemoryFilter {
            tags: Some(vec!["rust".to_string()]),
            ..MemoryFilter::system_unscoped()
        };
        let results = store.recall("memory", filter).await.expect("recall failed");

        assert_eq!(results.len(), 1);
        assert!(results[0].memory.tags.contains(&"rust".to_string()));
    }

    #[tokio::test]
    async fn recall_with_importance_filter() {
        let store = make_store();

        store
            .remember(
                make_memory("High importance", MemoryLayer::Semantic, "dev").with_importance(0.9),
            )
            .await
            .expect("remember failed");
        store
            .remember(
                make_memory("Low importance", MemoryLayer::Semantic, "dev").with_importance(0.2),
            )
            .await
            .expect("remember failed");

        let filter = MemoryFilter {
            min_importance: Some(0.5),
            ..MemoryFilter::system_unscoped()
        };
        let results = store
            .recall("importance", filter)
            .await
            .expect("recall failed");

        assert_eq!(results.len(), 1);
        assert!(results[0].memory.importance >= 0.5);
    }

    #[tokio::test]
    async fn recall_respects_limit() {
        let store = make_store();

        for i in 0..10 {
            store
                .remember(make_memory(
                    &format!("Memory number {i} about testing"),
                    MemoryLayer::Semantic,
                    "dev",
                ))
                .await
                .expect("remember failed");
        }

        let filter = MemoryFilter {
            limit: Some(3),
            ..MemoryFilter::system_unscoped()
        };
        let results = store
            .recall("testing", filter)
            .await
            .expect("recall failed");

        assert_eq!(results.len(), 3);
    }

    #[tokio::test]
    async fn identity_memories_are_reassigned_and_purged_forgotten_ones_included() {
        crate::store::identity_operations_contract(&make_store()).await;
    }

    // ─── Forget (Soft Delete) ────────────────────────────────────────────

    #[tokio::test]
    async fn forget_excludes_from_recall() {
        let store = make_store();

        let mem = make_memory("secret info", MemoryLayer::Episodic, "dev");
        let id = store.remember(mem).await.expect("remember failed");

        let results = store
            .recall("secret", MemoryFilter::system_unscoped())
            .await
            .expect("recall failed");
        assert_eq!(results.len(), 1);

        store.forget(&id).await.expect("forget failed");

        let results = store
            .recall("secret", MemoryFilter::system_unscoped())
            .await
            .expect("recall failed");
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn forget_excludes_from_count() {
        let store = make_store();

        let mem = make_memory("to delete", MemoryLayer::Episodic, "dev");
        let id = store.remember(mem).await.expect("remember failed");

        assert_eq!(
            store
                .count(MemoryFilter::system_unscoped())
                .await
                .expect("count failed"),
            1
        );

        store.forget(&id).await.expect("forget failed");

        assert_eq!(
            store
                .count(MemoryFilter::system_unscoped())
                .await
                .expect("count failed"),
            0
        );
    }

    // ─── Share (Visibility) ──────────────────────────────────────────────

    #[tokio::test]
    async fn share_changes_visibility() {
        let store = make_store();
        let mem = make_memory("private info", MemoryLayer::Episodic, "dev");
        let id = store.remember(mem).await.expect("remember failed");

        let stored = store
            .get(&id)
            .await
            .expect("get failed")
            .expect("not found");
        assert_eq!(stored.visibility, Visibility::Private);

        store
            .share(&id, Visibility::Team("backend".to_string()))
            .await
            .expect("share failed");

        let updated = store
            .get(&id)
            .await
            .expect("get failed")
            .expect("not found");
        assert_eq!(updated.visibility, Visibility::Team("backend".to_string()));
    }

    // ─── Count ───────────────────────────────────────────────────────────

    #[tokio::test]
    async fn count_with_filter() {
        let store = make_store();

        for i in 0..5 {
            let layer = if i < 3 {
                MemoryLayer::Semantic
            } else {
                MemoryLayer::Episodic
            };
            store
                .remember(make_memory(&format!("Memory {i}"), layer, "dev"))
                .await
                .expect("remember failed");
        }

        let count = store
            .count(MemoryFilter {
                layer: Some(MemoryLayer::Semantic),
                ..MemoryFilter::system_unscoped()
            })
            .await
            .expect("count failed");
        assert_eq!(count, 3);
    }

    #[tokio::test]
    async fn count_all() {
        let store = make_store();

        for i in 0..4 {
            store
                .remember(make_memory(
                    &format!("Memory {i}"),
                    MemoryLayer::Episodic,
                    "dev",
                ))
                .await
                .expect("remember failed");
        }

        let count = store
            .count(MemoryFilter::system_unscoped())
            .await
            .expect("count failed");
        assert_eq!(count, 4);
    }

    // ─── Vector Utilities ────────────────────────────────────────────────

    #[test]
    fn embedding_roundtrip() {
        let original = vec![0.1_f32, 0.2, 0.3, -0.5, 1.0];
        let encoded = encode_embedding(&original);
        let decoded = decode_embedding(&encoded);
        assert_eq!(original, decoded);
    }

    #[test]
    fn cosine_similarity_identical() {
        let a = vec![1.0_f32, 0.0, 0.0];
        assert!((cosine_similarity(&a, &a) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn cosine_similarity_orthogonal() {
        let a = vec![1.0_f32, 0.0, 0.0];
        let b = vec![0.0_f32, 1.0, 0.0];
        assert!(cosine_similarity(&a, &b).abs() < 1e-6);
    }

    #[test]
    fn cosine_similarity_opposite() {
        let a = vec![1.0_f32, 0.0];
        let b = vec![-1.0_f32, 0.0];
        assert!((cosine_similarity(&a, &b) + 1.0).abs() < 1e-6);
    }

    #[test]
    fn cosine_similarity_empty() {
        let a: Vec<f32> = vec![];
        let b: Vec<f32> = vec![];
        assert_eq!(cosine_similarity(&a, &b), 0.0);
    }

    #[test]
    fn cosine_similarity_length_mismatch() {
        let a = vec![1.0_f32, 2.0];
        let b = vec![1.0_f32, 2.0, 3.0];
        assert_eq!(cosine_similarity(&a, &b), 0.0);
    }

    // ─── File-based persistence ──────────────────────────────────────────

    #[tokio::test]
    async fn adjust_importance_round_trips_and_clamps() {
        let store = make_store();
        let mem = Memory::new("x", MemoryLayer::Semantic, AgentId::new("a")).with_importance(0.5);
        let id = store.remember(mem).await.unwrap();

        let after = store.adjust_importance(&id, 0.2).await.unwrap();
        assert!((after - 0.7).abs() < 1e-6);

        let high = store.adjust_importance(&id, 5.0).await.unwrap();
        assert!((high - 1.0).abs() < 1e-6);

        let low = store.adjust_importance(&id, -10.0).await.unwrap();
        assert!((low - 0.05).abs() < 1e-6);
    }

    #[tokio::test]
    async fn adjust_importance_errors_when_missing() {
        let store = make_store();
        let result = store
            .adjust_importance(&MemoryId("nope".to_string()), 0.1)
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn adjust_importance_is_atomic_under_concurrent_updates() {
        let store = Arc::new(make_store());
        let mem = Memory::new("x", MemoryLayer::Semantic, AgentId::new("a")).with_importance(0.5);
        let id = store.remember(mem).await.unwrap();

        let tasks: Vec<_> = (0..20)
            .map(|_| {
                let store = Arc::clone(&store);
                let id = id.clone();
                tokio::spawn(async move { store.adjust_importance(&id, 0.01).await.unwrap() })
            })
            .collect();
        for t in tasks {
            t.await.unwrap();
        }

        let final_importance = store.get(&id).await.unwrap().unwrap().importance;
        assert!(
            (final_importance - 0.70).abs() < 1e-5,
            "expected 0.70 after 20 atomic +0.01 increments, got {final_importance}"
        );
    }

    #[tokio::test]
    async fn source_filter_honoured() {
        let store = make_store();
        store
            .remember(
                Memory::new(
                    "When working with bedrock provider switching, pass aws_profile explicitly.",
                    MemoryLayer::Semantic,
                    AgentId::new("a"),
                )
                .with_source(MemorySource::Lesson),
            )
            .await
            .unwrap();
        store
            .remember(
                Memory::new(
                    "user asked about bedrock today",
                    MemoryLayer::Episodic,
                    AgentId::new("a"),
                )
                .with_source(MemorySource::Passive),
            )
            .await
            .unwrap();

        let lessons = store
            .recall(
                "bedrock",
                MemoryFilter {
                    source: Some(MemorySource::Lesson),
                    ..MemoryFilter::system_unscoped()
                },
            )
            .await
            .unwrap();
        assert_eq!(lessons.len(), 1);
        assert_eq!(lessons[0].memory.source, MemorySource::Lesson);

        let passive = store
            .recall(
                "bedrock",
                MemoryFilter {
                    source: Some(MemorySource::Passive),
                    ..MemoryFilter::system_unscoped()
                },
            )
            .await
            .unwrap();
        assert_eq!(passive.len(), 1);
        assert_eq!(passive[0].memory.source, MemorySource::Passive);
    }

    #[tokio::test]
    async fn persistence_across_reopen() {
        let dir = tempfile::tempdir().expect("Failed to create temp dir");
        let db_path = dir.path().join("test.db");

        let config = SqliteMemoryConfig {
            db_path: db_path.to_string_lossy().to_string(),
            embedding_provider: "none".to_string(),
            ..Default::default()
        };

        // Write some data
        {
            let store = SqliteMemoryStore::open(&config).expect("Failed to open store");
            store
                .remember(make_memory("persistent fact", MemoryLayer::Semantic, "dev"))
                .await
                .expect("remember failed");
        }

        // Reopen and verify data persists
        {
            let store = SqliteMemoryStore::open(&config).expect("Failed to reopen store");
            let count = store
                .count(MemoryFilter::system_unscoped())
                .await
                .expect("count failed");
            assert_eq!(count, 1);

            let results = store
                .recall("persistent", MemoryFilter::system_unscoped())
                .await
                .expect("recall failed");
            assert_eq!(results.len(), 1);
            assert!(results[0].memory.content.contains("persistent"));
        }
    }
}
