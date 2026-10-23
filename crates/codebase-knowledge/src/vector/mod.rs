//! Vector stores for code knowledge semantic search, on Weaviate or vellev.
//!
//! Each provides two collections:
//! - `CodeEntity`: semantic search over code entities (functions, structs, etc.)
//! - `CodeSummary`: semantic search over generated summaries at various granularity levels

pub mod health;
mod vellev;

use async_trait::async_trait;
use eyre::Result;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tracing::{debug, info, warn};

use workforce_common::vectorizable::VectorizableText;

use crate::graph::entities::{CodeEntity, EntityKind};
use health::{InsertErrorSink, InsertErrorSlot};

pub use health::InsertError;
pub use vellev::VellevCodeVectorStore;

const CODE_ENTITY_COLLECTION: &str = "CodeEntity";
const CODE_SUMMARY_COLLECTION: &str = "CodeSummary";

/// Safety cap on `batch_delete_until_drained` iterations. Weaviate's
/// per-call limit (`QUERY_MAXIMUM_RESULTS`, default 10000) means this
/// covers up to 160k objects per drain call.
const MAX_BATCH_DELETE_PASSES: usize = 16;

/// Outcome of a (potentially multi-pass) batch delete.
#[derive(Debug, Default, Clone, Copy, Serialize)]
pub struct DeleteOutcome {
    pub successful: usize,
    pub failed: usize,
}

impl DeleteOutcome {
    pub fn total(&self) -> usize {
        self.successful + self.failed
    }
}

/// Subset of Weaviate's `/v1/batch/objects` DELETE response we care
/// about. The `results` block carries one `BatchDeleteResults` per call.
#[derive(Debug, Deserialize)]
struct BatchDeleteResponse {
    results: BatchDeleteResults,
}

#[derive(Debug, Deserialize)]
struct BatchDeleteResults {
    /// Total objects matched by `where` (capped at the server's
    /// per-call limit). Zero means the criteria has been drained.
    matches: usize,
    /// Successfully deleted in this call.
    successful: usize,
    /// Matched but failed to delete (concurrent reindex, conflict, etc.).
    failed: usize,
}

/// A searchable code entity stored in a vector store for vector search.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct VectorCodeEntity {
    pub entity_id: String,
    pub repo_id: String,
    pub qualified_name: String,
    pub name: String,
    pub kind: String,
    pub file_path: String,
    pub language: String,
    pub signature: String,
    pub doc_comment: String,
    pub summary: String,
    /// Combined searchable content: name + signature + doc_comment + summary
    pub searchable_content: String,
    pub start_line: u32,
    pub end_line: u32,
    /// The fill of the repo's vectors this was written in or since (see
    /// [`crate::graph::vectors::VectorFill`]); empty before any.
    #[serde(default)]
    pub generation: String,
}

/// A hash of `parts`, the same for the same parts in the same order.
fn hash_of(parts: &[&str]) -> String {
    let named = serde_json::json!(parts).to_string();
    uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_URL, named.as_bytes())
        .simple()
        .to_string()
}

impl VectorCodeEntity {
    /// A hash of everything a store keeps of this entity but its fill
    /// generation: the text that is embedded, and where the entity is.
    /// Two records of one entity hash alike exactly when a store holding
    /// one holds the other.
    pub fn record_hash(&self) -> String {
        let (start, end) = (self.start_line.to_string(), self.end_line.to_string());
        hash_of(&[
            &self.searchable_content,
            &self.entity_id,
            &self.repo_id,
            &self.qualified_name,
            &self.name,
            &self.kind,
            &self.file_path,
            &self.language,
            &self.signature,
            &self.doc_comment,
            &self.summary,
            &start,
            &end,
        ])
    }

    /// As written in or since the fill `generation`.
    pub fn of_generation(mut self, generation: &str) -> Self {
        self.generation = generation.to_string();
        self
    }

    /// Build a searchable entity from a CodeEntity and optional summary.
    pub fn from_code_entity(entity: &CodeEntity, repo_id: &str, summary: Option<&str>) -> Self {
        let searchable = format!(
            "{} {} {} {}",
            entity.qualified_name,
            entity.signature.as_deref().unwrap_or(""),
            entity.doc_comment.as_deref().unwrap_or(""),
            summary.unwrap_or(""),
        );

        Self {
            entity_id: entity.id.0.clone(),
            repo_id: repo_id.to_string(),
            qualified_name: entity.qualified_name.clone(),
            name: entity.name.clone(),
            kind: format!("{:?}", entity.kind),
            file_path: entity.file_path.clone(),
            language: format!("{:?}", entity.language),
            signature: entity.signature.clone().unwrap_or_default(),
            doc_comment: entity.doc_comment.clone().unwrap_or_default(),
            summary: summary.unwrap_or("").to_string(),
            searchable_content: searchable,
            start_line: entity.start_line as u32,
            end_line: entity.end_line as u32,
            generation: String::new(),
        }
    }
}

/// A searchable code summary stored in a vector store for vector search.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct VectorCodeSummary {
    pub entity_id: String,
    pub repo_id: String,
    pub granularity: String,
    pub file_path: String,
    pub summary_text: String,
    pub is_stale: bool,
    /// As [`VectorCodeEntity::generation`].
    #[serde(default)]
    pub generation: String,
}

impl VectorCodeSummary {
    /// What tells this summary from every other of its repo: its entity
    /// and granularity.
    pub fn key(&self) -> String {
        format!("{}|{}", self.entity_id, self.granularity)
    }

    /// As [`VectorCodeEntity::record_hash`].
    pub fn record_hash(&self) -> String {
        hash_of(&[
            &self.summary_text,
            &self.entity_id,
            &self.repo_id,
            &self.granularity,
            &self.file_path,
            if self.is_stale { "stale" } else { "fresh" },
        ])
    }

    /// A summary as the graph stores it, written in or since the fill
    /// `generation`.
    pub fn of(
        stored: &crate::graph::vectors::StoredSummary,
        repo_id: &str,
        generation: &str,
    ) -> Self {
        Self {
            entity_id: stored.entity_id.clone(),
            repo_id: repo_id.to_string(),
            granularity: stored.granularity.clone(),
            file_path: stored.file_path.clone(),
            summary_text: stored.text.clone(),
            is_stale: stored.is_stale,
            generation: generation.to_string(),
        }
    }
}

/// How a page of what a store should hold differs from what it does.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Differing {
    /// The places in the page of what the store lacks or holds otherwise.
    pub places: Vec<usize>,
    /// How many vectors the store holds under the page's keys: over a
    /// whole repo, what the store holds beyond their sum is of entities
    /// the graph does not have.
    pub held: usize,
}

/// Result from a vector search query.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VectorSearchResult {
    pub entity_id: String,
    pub repo_id: String,
    pub qualified_name: String,
    pub kind: String,
    pub file_path: String,
    pub start_line: u32,
    pub end_line: u32,
    pub score: f32,
}

/// Semantic search over a codebase's entities and summaries, whichever
/// vector database holds them.
#[async_trait]
pub trait CodeVectorStore: Send + Sync {
    /// Ensure the entity and summary collections exist.
    async fn ensure_collections(&self) -> Result<()>;

    /// Whether the backing server is reachable.
    async fn health_check(&self) -> Result<bool>;

    /// The address the store points at, for status output.
    fn base_url(&self) -> &str;

    /// Wire a sink that receives every non-fatal insert error. Later
    /// calls replace the earlier sink. The sink must not block.
    fn set_insert_error_sink(&self, sink: Arc<dyn InsertErrorSink>);

    /// Index entities, returning how many were stored. Entities with
    /// nothing to embed are skipped.
    async fn index_entities(&self, entities: &[VectorCodeEntity]) -> Result<usize>;

    /// Search for code entities semantically.
    async fn search_entities(
        &self,
        query: &str,
        repo_id: Option<&str>,
        kind: Option<EntityKind>,
        limit: usize,
    ) -> Result<Vec<VectorSearchResult>>;

    /// Delete all entities for a repo.
    async fn delete_repo_entities(&self, repo_id: &str) -> Result<DeleteOutcome>;

    /// Delete all entities for a (repo, file).
    async fn delete_entities_by_file(
        &self,
        repo_id: &str,
        file_path: &str,
    ) -> Result<DeleteOutcome>;

    /// Index summaries, returning how many were stored. Summaries with
    /// nothing to embed are skipped.
    async fn index_summaries(&self, summaries: &[VectorCodeSummary]) -> Result<usize>;

    /// Delete all summaries for a repo.
    async fn delete_repo_summaries(&self, repo_id: &str) -> Result<DeleteOutcome>;

    /// Delete all summaries for a (repo, file).
    async fn delete_summaries_by_file(
        &self,
        repo_id: &str,
        file_path: &str,
    ) -> Result<DeleteOutcome>;

    /// How many entities a repo has.
    async fn entity_count(&self, repo_id: &str) -> Result<usize>;

    /// Whether a repo's vectors may be missing until they are indexed from
    /// the graph, so that searching relies on them only once the graph
    /// records them complete in the collection this store now has (see
    /// [`Self::identity`]). A store that has always held every repo's
    /// vectors, as Weaviate's has, says no.
    fn tracks_completeness(&self) -> bool {
        false
    }

    /// Which making of its entity collection the store now has: another
    /// once the collection was dropped, or left out of a restored backup,
    /// and made again, empty. What the graph records as complete is
    /// complete in the collection it names only. A store may answer from
    /// what it read a short while ago, unless asked `afresh`. One that does
    /// not track completeness has one collection, always the same, and
    /// names it by where it is, so that what is recorded of one store is
    /// never taken for another's.
    async fn identity(&self, _afresh: bool) -> Result<String> {
        Ok(format!("untracked:{}", self.base_url()))
    }

    /// Keep the vectors for the code graph of this identity
    /// ([`crate::graph::CodeGraph::identity`]) and no other, where a store
    /// can tell: said once, before anything is written.
    fn keep_for_graph(&self, _graph: &str) {}

    /// Take the vectors over for this store's graph from whichever graph
    /// they were kept for: at an operator's word, when the graph they were
    /// kept for is no more. What they hold is then to be filled again.
    async fn take_over(&self) -> Result<()> {
        Ok(())
    }

    /// Of `entities`, all of the repo `repo_id`, those the store lacks or
    /// holds otherwise than so (by [`VectorCodeEntity::record_hash`]), as
    /// their places in `entities`, and how many vectors it holds of them
    /// in all. Asked without reading what the store holds where it holds
    /// them all. A store that keeps no track finds none differing.
    async fn differing_entities(
        &self,
        _repo_id: &str,
        entities: &[VectorCodeEntity],
    ) -> Result<Differing> {
        Ok(Differing {
            places: Vec::new(),
            held: entities.len(),
        })
    }

    /// As [`Self::differing_entities`], of summaries.
    async fn differing_summaries(
        &self,
        _repo_id: &str,
        summaries: &[VectorCodeSummary],
    ) -> Result<Differing> {
        Ok(Differing {
            places: Vec::new(),
            held: summaries.len(),
        })
    }

    /// A page of what the store holds of the repo, entities or
    /// `summaries`, after `after` (the start if `None`): each as its key
    /// (an entity's id, or [`VectorCodeSummary::key`]) and its file, with
    /// where the next page starts if there is one. Read only to find what
    /// the store holds that the graph does not, when it holds more than it
    /// should. A store that keeps no track lists nothing.
    async fn held_after(
        &self,
        _repo_id: &str,
        _summaries: bool,
        _after: Option<String>,
        _limit: usize,
    ) -> Result<(Vec<(String, String)>, Option<String>)> {
        Ok((Vec::new(), None))
    }

    /// Forget these entities, each as it would be stored.
    async fn remove_entities(&self, _entities: &[VectorCodeEntity]) -> Result<()> {
        Ok(())
    }

    /// Forget these summaries, each as it would be stored.
    async fn remove_summaries(&self, _summaries: &[VectorCodeSummary]) -> Result<()> {
        Ok(())
    }

    /// Forget every entity and summary of a repo not written in or since
    /// the fill `generation`: what was stored of the repo before it and
    /// not stored again. Returns how many went.
    async fn sweep(&self, _repo_id: &str, _generation: &str) -> Result<usize> {
        Ok(0)
    }

    /// How many summaries a repo has.
    async fn summary_count(&self, _repo_id: &str) -> Result<usize> {
        Ok(0)
    }
}

/// The [`CodeVectorStore`] on Weaviate.
pub struct WeaviateCodeVectorStore {
    client: Client,
    base_url: String,
    /// Optional sink for non-fatal insert-batch errors. Set after
    /// construction via [`set_insert_error_sink`] so the hub can wire
    /// in its health probe without changing every existing
    /// `WeaviateCodeVectorStore::new` call site.
    insert_errors: InsertErrorSlot,
}

/// `records` whose `text` can be embedded, each with that text, and how
/// many were passed over.
fn partition_vectorizable<T>(
    records: &[T],
    text: fn(&T) -> &str,
) -> (Vec<(&T, VectorizableText)>, usize) {
    let kept: Vec<_> = records
        .iter()
        .filter_map(|record| Some((record, VectorizableText::try_new(text(record)).ok()?)))
        .collect();
    let skipped = records.len() - kept.len();
    (kept, skipped)
}

/// As [`partition_vectorizable`], warning of those passed over as
/// `records` (`"entities"`, `"summaries"`).
fn vectorizable<'a, T>(
    records: &'a [T],
    text: fn(&T) -> &str,
    operation: &'static str,
    what: &str,
) -> Vec<(&'a T, VectorizableText)> {
    let (kept, skipped) = partition_vectorizable(records, text);
    if skipped > 0 {
        warn!(
            target: "vectorizable",
            operation,
            filtered = skipped,
            kept = kept.len(),
            "skipped non-vectorizable {what} in batch"
        );
    }
    kept
}

fn entity_text(entity: &VectorCodeEntity) -> &str {
    &entity.searchable_content
}

fn summary_text(summary: &VectorCodeSummary) -> &str {
    &summary.summary_text
}

/// `query` as text to search by, or `None`, warned of as `operation`, if
/// it has nothing to embed.
fn search_text(query: &str, operation: &'static str) -> Option<VectorizableText> {
    VectorizableText::try_new(query)
        .map_err(|e| {
            warn!(
                target: "vectorizable",
                operation,
                reason = e.as_metric_label(),
                "rejected non-vectorizable query, returning empty results"
            )
        })
        .ok()
}

/// A search hit from an entity's stored properties, scored `score`.
fn search_result(item: &serde_json::Value, score: f32) -> Option<VectorSearchResult> {
    Some(VectorSearchResult {
        entity_id: item["entityId"].as_str()?.to_string(),
        repo_id: item["repoId"].as_str()?.to_string(),
        qualified_name: item["qualifiedName"].as_str().unwrap_or("").to_string(),
        kind: item["kind"].as_str().unwrap_or("Unknown").to_string(),
        file_path: item["filePath"].as_str().unwrap_or("").to_string(),
        start_line: item["startLine"].as_u64().unwrap_or(0) as u32,
        end_line: item["endLine"].as_u64().unwrap_or(0) as u32,
        score,
    })
}

impl WeaviateCodeVectorStore {
    pub fn new(base_url: impl Into<String>, api_key: Option<&str>) -> Self {
        use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION};

        let mut headers = HeaderMap::new();
        if let Some(key) = api_key {
            if let Ok(val) = HeaderValue::from_str(&format!("Bearer {}", key)) {
                headers.insert(AUTHORIZATION, val);
            }
        }

        let timeout = std::time::Duration::from_secs(120);
        Self {
            client: Client::builder()
                .default_headers(headers.clone())
                .timeout(timeout)
                .build()
                .unwrap_or_else(|_| {
                    // Fallback without custom headers but keep the timeout —
                    // Client::new() has no timeout which can cause hangs.
                    Client::builder()
                        .timeout(timeout)
                        .build()
                        .expect("HTTP client with timeout must be constructible")
                }),
            base_url: base_url.into(),
            insert_errors: InsertErrorSlot::default(),
        }
    }

    // ── CodeEntity collection ──────────────────────────────────────

    async fn ensure_code_entity_collection(&self) -> Result<()> {
        let url = format!("{}/v1/schema/{}", self.base_url, CODE_ENTITY_COLLECTION);
        let resp = self.client.get(&url).send().await;

        match resp {
            Ok(r) if r.status().is_success() => {
                debug!(
                    "Weaviate collection '{}' already exists",
                    CODE_ENTITY_COLLECTION
                );
                Ok(())
            }
            _ => {
                info!("Creating Weaviate collection '{}'", CODE_ENTITY_COLLECTION);
                let schema = serde_json::json!({
                    "class": CODE_ENTITY_COLLECTION,
                    "description": "Code entities (functions, structs, classes, etc.) for semantic search",
                    "vectorizer": "text2vec-transformers",
                    "properties": [
                        {
                            "name": "searchableContent",
                            "dataType": ["text"],
                            "description": "Combined searchable content: name + signature + doc_comment + summary"
                        },
                        {
                            "name": "entityId",
                            "dataType": ["text"],
                            "description": "Unique entity ID",
                            "tokenization": "field"
                        },
                        {
                            "name": "repoId",
                            "dataType": ["text"],
                            "description": "Repository ID",
                            "tokenization": "field"
                        },
                        {
                            "name": "qualifiedName",
                            "dataType": ["text"],
                            "description": "Fully qualified entity name"
                        },
                        {
                            "name": "name",
                            "dataType": ["text"],
                            "description": "Short entity name"
                        },
                        {
                            "name": "kind",
                            "dataType": ["text"],
                            "description": "Entity kind (Function, Struct, Class, etc.)",
                            "tokenization": "field"
                        },
                        {
                            "name": "filePath",
                            "dataType": ["text"],
                            "description": "File path relative to workspace root",
                            "tokenization": "field"
                        },
                        {
                            "name": "language",
                            "dataType": ["text"],
                            "description": "Programming language",
                            "tokenization": "field"
                        },
                        {
                            "name": "signature",
                            "dataType": ["text"],
                            "description": "Full signature text"
                        },
                        {
                            "name": "docComment",
                            "dataType": ["text"],
                            "description": "Documentation comment"
                        },
                        {
                            "name": "summary",
                            "dataType": ["text"],
                            "description": "LLM-generated summary"
                        },
                        {
                            "name": "startLine",
                            "dataType": ["int"],
                            "description": "Start line number"
                        },
                        {
                            "name": "endLine",
                            "dataType": ["int"],
                            "description": "End line number"
                        }
                    ]
                });

                let url = format!("{}/v1/schema", self.base_url);
                let resp = self
                    .client
                    .post(&url)
                    .json(&schema)
                    .send()
                    .await
                    .map_err(|e| eyre::eyre!("Failed to create CodeEntity collection: {}", e))?;

                if !resp.status().is_success() {
                    let body = resp.text().await.unwrap_or_default();
                    return Err(eyre::eyre!(
                        "Failed to create CodeEntity collection: {}",
                        body
                    ));
                }

                info!("Created Weaviate collection '{}'", CODE_ENTITY_COLLECTION);
                Ok(())
            }
        }
    }

    // ── CodeSummary collection ─────────────────────────────────────

    async fn ensure_code_summary_collection(&self) -> Result<()> {
        let url = format!("{}/v1/schema/{}", self.base_url, CODE_SUMMARY_COLLECTION);
        let resp = self.client.get(&url).send().await;

        match resp {
            Ok(r) if r.status().is_success() => {
                debug!(
                    "Weaviate collection '{}' already exists",
                    CODE_SUMMARY_COLLECTION
                );
                Ok(())
            }
            _ => {
                info!("Creating Weaviate collection '{}'", CODE_SUMMARY_COLLECTION);
                let schema = serde_json::json!({
                    "class": CODE_SUMMARY_COLLECTION,
                    "description": "LLM-generated code summaries for semantic search",
                    "vectorizer": "text2vec-transformers",
                    "properties": [
                        {
                            "name": "summaryText",
                            "dataType": ["text"],
                            "description": "The summary text (vectorized for semantic search)"
                        },
                        {
                            "name": "entityId",
                            "dataType": ["text"],
                            "description": "Entity this summary describes",
                            "tokenization": "field"
                        },
                        {
                            "name": "repoId",
                            "dataType": ["text"],
                            "description": "Repository ID",
                            "tokenization": "field"
                        },
                        {
                            "name": "granularity",
                            "dataType": ["text"],
                            "description": "Summary granularity (function, file, module, architecture)",
                            "tokenization": "field"
                        },
                        {
                            "name": "filePath",
                            "dataType": ["text"],
                            "description": "File path this summary relates to",
                            "tokenization": "field"
                        },
                        {
                            "name": "isStale",
                            "dataType": ["boolean"],
                            "description": "Whether this summary needs regeneration"
                        }
                    ]
                });

                let url = format!("{}/v1/schema", self.base_url);
                let resp = self
                    .client
                    .post(&url)
                    .json(&schema)
                    .send()
                    .await
                    .map_err(|e| eyre::eyre!("Failed to create CodeSummary collection: {}", e))?;

                if !resp.status().is_success() {
                    let body = resp.text().await.unwrap_or_default();
                    return Err(eyre::eyre!(
                        "Failed to create CodeSummary collection: {}",
                        body
                    ));
                }

                info!("Created Weaviate collection '{}'", CODE_SUMMARY_COLLECTION);
                Ok(())
            }
        }
    }

    /// Issue a single batch delete-by-criteria call. Returns the raw
    /// `results` block from Weaviate. Caller is responsible for looping
    /// when the per-call cap (`QUERY_MAXIMUM_RESULTS`, default 10000)
    /// is hit.
    async fn batch_delete_once(
        &self,
        collection: &str,
        where_filter: &serde_json::Value,
    ) -> Result<BatchDeleteResults> {
        let url = format!("{}/v1/batch/objects", self.base_url);
        let body = serde_json::json!({
            "match": { "class": collection, "where": where_filter },
            "output": "minimal",
            "dryRun": false,
        });
        let resp = self.client.delete(&url).json(&body).send().await?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(eyre::eyre!(
                "Weaviate batch delete failed ({}): {}",
                status,
                text
            ));
        }
        let body: BatchDeleteResponse = resp.json().await?;
        Ok(body.results)
    }

    /// Drain `where_filter` from `collection` by repeatedly invoking
    /// the batch delete endpoint until no more matches remain.
    /// Capped at [`MAX_BATCH_DELETE_PASSES`] iterations as a runaway
    /// guard — at the default Weaviate per-call cap of 10000 that's
    /// 160k objects per call site, more than any real repo.
    async fn batch_delete_until_drained(
        &self,
        collection: &str,
        where_filter: serde_json::Value,
    ) -> Result<DeleteOutcome> {
        let mut total = DeleteOutcome::default();
        for _ in 0..MAX_BATCH_DELETE_PASSES {
            let r = self.batch_delete_once(collection, &where_filter).await?;
            total.successful += r.successful;
            total.failed += r.failed;
            if r.matches == 0 {
                break;
            }
        }
        if total.failed > 0 {
            warn!(
                collection = collection,
                successful = total.successful,
                failed = total.failed,
                "Weaviate batch delete reported failures"
            );
        }
        Ok(total)
    }
}

#[async_trait]
impl CodeVectorStore for WeaviateCodeVectorStore {
    /// Ensure both CodeEntity and CodeSummary collections exist.
    async fn ensure_collections(&self) -> Result<()> {
        self.ensure_code_entity_collection().await?;
        self.ensure_code_summary_collection().await?;
        Ok(())
    }

    /// Check if the Weaviate server is reachable.
    async fn health_check(&self) -> Result<bool> {
        let url = format!("{}/v1/.well-known/ready", self.base_url);
        match self.client.get(&url).send().await {
            Ok(resp) if resp.status().is_success() => Ok(true),
            _ => Ok(false),
        }
    }

    /// Return the base URL the store points at. Used by the hub-side
    /// health probe to surface "where am I checking?" in status output.
    fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Wire a sink that will receive every non-fatal insert-batch
    /// error. Idempotent; later calls overwrite the previous sink.
    /// The sink must be cheap and non-blocking — `record` is invoked
    /// from the indexer's async path.
    fn set_insert_error_sink(&self, sink: Arc<dyn InsertErrorSink>) {
        self.insert_errors.set(sink);
    }

    /// Batch index multiple entities (chunked into groups of 50 to avoid
    /// overwhelming the transformer vectoriser and hitting client timeouts).
    async fn index_entities(&self, entities: &[VectorCodeEntity]) -> Result<usize> {
        let vectorizable = vectorizable(entities, entity_text, "code_index_entities", "entities");
        if vectorizable.is_empty() {
            return Ok(0);
        }

        let url = format!("{}/v1/batch/objects", self.base_url);
        let mut total = 0;

        for chunk in vectorizable.chunks(50) {
            let objects: Vec<serde_json::Value> = chunk
                .iter()
                .map(|(entity, searchable)| {
                    serde_json::json!({
                        "class": CODE_ENTITY_COLLECTION,
                        "properties": {
                            "searchableContent": searchable,
                            "entityId": entity.entity_id,
                            "repoId": entity.repo_id,
                            "qualifiedName": entity.qualified_name,
                            "name": entity.name,
                            "kind": entity.kind,
                            "filePath": entity.file_path,
                            "language": entity.language,
                            "signature": entity.signature,
                            "docComment": entity.doc_comment,
                            "summary": entity.summary,
                            "startLine": entity.start_line,
                            "endLine": entity.end_line,
                        }
                    })
                })
                .collect();

            let resp = match self
                .client
                .post(&url)
                .json(&serde_json::json!({ "objects": objects }))
                .send()
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    let msg = format!("Weaviate batch index failed: {e}");
                    warn!(error = %e, "weaviate batch insert send error");
                    self.insert_errors.notify(&msg);
                    return Err(eyre::eyre!(msg));
                }
            };

            if !resp.status().is_success() {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                let msg = format!("Weaviate batch index failed: {body}");
                warn!(status = %status, body = %body, "weaviate batch insert non-success");
                self.insert_errors.notify(&msg);
                return Err(eyre::eyre!(msg));
            }

            total += chunk.len();
            debug!(
                chunk_size = chunk.len(),
                total = total,
                "Batch indexed entity chunk in Weaviate"
            );
        }

        info!(count = total, "Batch indexed entities in Weaviate");
        Ok(total)
    }

    /// Search for code entities semantically.
    async fn search_entities(
        &self,
        query: &str,
        repo_id: Option<&str>,
        kind: Option<EntityKind>,
        limit: usize,
    ) -> Result<Vec<VectorSearchResult>> {
        let Some(query) = search_text(query, "code_search_entities") else {
            return Ok(Vec::new());
        };
        let mut where_conditions = Vec::new();

        if let Some(rid) = repo_id {
            where_conditions.push(format!(
                r#"{{ path: ["repoId"], operator: Equal, valueText: "{}" }}"#,
                rid
            ));
        }

        if let Some(k) = kind {
            where_conditions.push(format!(
                r#"{{ path: ["kind"], operator: Equal, valueText: "{:?}" }}"#,
                k
            ));
        }

        let where_clause = if where_conditions.is_empty() {
            String::new()
        } else if where_conditions.len() == 1 {
            format!("where: {}", where_conditions[0])
        } else {
            format!(
                "where: {{ operator: And, operands: [{}] }}",
                where_conditions.join(", ")
            )
        };

        let graphql = format!(
            r#"{{
                Get {{
                    {collection}(
                        nearText: {{ concepts: ["{query}"] }}
                        limit: {limit}
                        {where_clause}
                    ) {{
                        entityId
                        repoId
                        qualifiedName
                        kind
                        filePath
                        startLine
                        endLine
                        _additional {{
                            certainty
                        }}
                    }}
                }}
            }}"#,
            collection = CODE_ENTITY_COLLECTION,
            query = query.as_str().replace('"', "\\\""),
        );

        let graphql = graphql.split_whitespace().collect::<Vec<_>>().join(" ");

        let url = format!("{}/v1/graphql", self.base_url);
        let resp = self
            .client
            .post(&url)
            .json(&serde_json::json!({ "query": graphql }))
            .send()
            .await
            .map_err(|e| eyre::eyre!("Weaviate search failed: {}", e))?;

        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(eyre::eyre!("Weaviate search failed: {}", body));
        }

        let body: serde_json::Value = resp.json().await?;
        check_graphql_response(&body)?;

        let items = body["data"]["Get"][CODE_ENTITY_COLLECTION]
            .as_array()
            .cloned()
            .unwrap_or_default();

        let results = items
            .into_iter()
            .filter_map(|item| {
                let certainty = item["_additional"]["certainty"].as_f64().unwrap_or(0.0);
                search_result(&item, certainty as f32)
            })
            .collect();

        Ok(results)
    }

    /// Delete all entities for a given repo.
    async fn delete_repo_entities(&self, repo_id: &str) -> Result<DeleteOutcome> {
        let outcome = self
            .batch_delete_until_drained(CODE_ENTITY_COLLECTION, repo_filter_json(repo_id))
            .await?;
        info!(
            repo_id = repo_id,
            successful = outcome.successful,
            failed = outcome.failed,
            "Deleted repo entities from Weaviate"
        );
        Ok(outcome)
    }

    /// Delete all entities for a given (repo, file). Used by the
    /// incremental indexer when a file is deleted, modified, or renamed.
    async fn delete_entities_by_file(
        &self,
        repo_id: &str,
        file_path: &str,
    ) -> Result<DeleteOutcome> {
        let outcome = self
            .batch_delete_until_drained(
                CODE_ENTITY_COLLECTION,
                repo_and_file_filter_json(repo_id, file_path),
            )
            .await?;
        debug!(
            repo_id = repo_id,
            file_path = file_path,
            successful = outcome.successful,
            failed = outcome.failed,
            "Deleted file entities from Weaviate"
        );
        Ok(outcome)
    }

    /// Batch index multiple summaries (chunked into groups of 50 to avoid
    /// overwhelming the transformer vectoriser and hitting client timeouts).
    async fn index_summaries(&self, summaries: &[VectorCodeSummary]) -> Result<usize> {
        let vectorizable =
            vectorizable(summaries, summary_text, "code_index_summaries", "summaries");
        if vectorizable.is_empty() {
            return Ok(0);
        }

        let url = format!("{}/v1/batch/objects", self.base_url);
        let mut total = 0;

        for chunk in vectorizable.chunks(50) {
            let objects: Vec<serde_json::Value> = chunk
                .iter()
                .map(|(s, text)| {
                    serde_json::json!({
                        "class": CODE_SUMMARY_COLLECTION,
                        "properties": {
                            "summaryText": text,
                            "entityId": s.entity_id,
                            "repoId": s.repo_id,
                            "granularity": s.granularity,
                            "filePath": s.file_path,
                            "isStale": s.is_stale,
                        }
                    })
                })
                .collect();

            let resp = self
                .client
                .post(&url)
                .json(&serde_json::json!({ "objects": objects }))
                .send()
                .await
                .map_err(|e| eyre::eyre!("Weaviate batch index summaries failed: {}", e))?;

            if !resp.status().is_success() {
                let body = resp.text().await.unwrap_or_default();
                return Err(eyre::eyre!(
                    "Weaviate batch index summaries failed: {}",
                    body
                ));
            }

            total += chunk.len();
            debug!(
                chunk_size = chunk.len(),
                total = total,
                "Batch indexed summary chunk in Weaviate"
            );
        }

        info!(count = total, "Batch indexed summaries in Weaviate");
        Ok(total)
    }

    /// Delete all summaries for a given repo.
    async fn delete_repo_summaries(&self, repo_id: &str) -> Result<DeleteOutcome> {
        let outcome = self
            .batch_delete_until_drained(CODE_SUMMARY_COLLECTION, repo_filter_json(repo_id))
            .await?;
        info!(
            repo_id = repo_id,
            successful = outcome.successful,
            failed = outcome.failed,
            "Deleted repo summaries from Weaviate"
        );
        Ok(outcome)
    }

    /// Delete all summaries for a given (repo, file).
    async fn delete_summaries_by_file(
        &self,
        repo_id: &str,
        file_path: &str,
    ) -> Result<DeleteOutcome> {
        let outcome = self
            .batch_delete_until_drained(
                CODE_SUMMARY_COLLECTION,
                repo_and_file_filter_json(repo_id, file_path),
            )
            .await?;
        debug!(
            repo_id = repo_id,
            file_path = file_path,
            successful = outcome.successful,
            failed = outcome.failed,
            "Deleted file summaries from Weaviate"
        );
        Ok(outcome)
    }

    /// Get entity count for a repo.
    async fn entity_count(&self, repo_id: &str) -> Result<usize> {
        let graphql = format!(
            r#"{{ Aggregate {{ {collection}(where: {{ path: ["repoId"], operator: Equal, valueText: "{repo_id}" }}) {{ meta {{ count }} }} }} }}"#,
            collection = CODE_ENTITY_COLLECTION,
        );
        let graphql = graphql.split_whitespace().collect::<Vec<_>>().join(" ");

        let url = format!("{}/v1/graphql", self.base_url);
        let resp = self
            .client
            .post(&url)
            .json(&serde_json::json!({ "query": graphql }))
            .send()
            .await?;

        let body: serde_json::Value = resp.json().await?;
        check_graphql_response(&body)?;

        let count = body["data"]["Aggregate"][CODE_ENTITY_COLLECTION][0]["meta"]["count"]
            .as_u64()
            .unwrap_or(0) as usize;

        Ok(count)
    }
}

/// Structured `where` clause for Weaviate's REST batch endpoints.
fn repo_filter_json(repo_id: &str) -> serde_json::Value {
    serde_json::json!({
        "path": ["repoId"],
        "operator": "Equal",
        "valueText": repo_id,
    })
}

fn repo_and_file_filter_json(repo_id: &str, file_path: &str) -> serde_json::Value {
    serde_json::json!({
        "operator": "And",
        "operands": [
            { "path": ["repoId"], "operator": "Equal", "valueText": repo_id },
            { "path": ["filePath"], "operator": "Equal", "valueText": file_path },
        ],
    })
}

/// Check a Weaviate GraphQL response for errors.
fn check_graphql_response(body: &serde_json::Value) -> Result<()> {
    if let Some(errors) = body["errors"].as_array() {
        if !errors.is_empty() {
            let messages: Vec<&str> = errors
                .iter()
                .filter_map(|e| e["message"].as_str())
                .collect();
            return Err(eyre::eyre!(
                "Weaviate GraphQL error: {}",
                messages.join("; ")
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::entities::{CodeVisibility, EntityId, Language};
    use std::sync::Mutex;

    #[test]
    fn vector_code_entity_from_code_entity() {
        let entity = CodeEntity {
            id: EntityId("test-entity-1".to_string()),
            kind: EntityKind::Function,
            name: "process_message".to_string(),
            qualified_name: "crate::hub::serve::process_message".to_string(),
            file_path: "src/serve.rs".to_string(),
            start_line: 100,
            end_line: 150,
            language: Language::Rust,
            signature: Some(
                "pub async fn process_message(&self, msg: &str) -> Result<String>".to_string(),
            ),
            doc_comment: Some("Process an incoming message".to_string()),
            visibility: CodeVisibility::Public,
            parent_id: None,
            repo_id: "test-repo".to_string(),
            content_hash: "abc123".to_string(),
        };

        let vec_entity = VectorCodeEntity::from_code_entity(
            &entity,
            "test-repo",
            Some("Handles incoming messages"),
        );
        assert_eq!(vec_entity.entity_id, "test-entity-1");
        assert_eq!(vec_entity.repo_id, "test-repo");
        assert_eq!(vec_entity.name, "process_message");
        assert!(vec_entity.searchable_content.contains("process_message"));
        assert!(vec_entity
            .searchable_content
            .contains("Handles incoming messages"));
    }

    #[test]
    fn vector_code_entity_without_summary() {
        let entity = CodeEntity {
            id: EntityId("test-entity-2".to_string()),
            kind: EntityKind::Struct,
            name: "Config".to_string(),
            qualified_name: "crate::config::Config".to_string(),
            file_path: "src/config.rs".to_string(),
            start_line: 10,
            end_line: 30,
            language: Language::Rust,
            signature: Some("pub struct Config".to_string()),
            doc_comment: None,
            visibility: CodeVisibility::Public,
            parent_id: None,
            repo_id: "test-repo".to_string(),
            content_hash: "def456".to_string(),
        };

        let vec_entity = VectorCodeEntity::from_code_entity(&entity, "test-repo", None);
        assert_eq!(vec_entity.summary, "");
        assert!(vec_entity.searchable_content.contains("Config"));
    }

    #[test]
    fn vector_search_result_serialization() {
        let result = VectorSearchResult {
            entity_id: "ent-1".to_string(),
            repo_id: "repo-1".to_string(),
            qualified_name: "foo::bar".to_string(),
            kind: "Function".to_string(),
            file_path: "src/lib.rs".to_string(),
            start_line: 10,
            end_line: 20,
            score: 0.95,
        };

        let json = serde_json::to_string(&result).unwrap();
        let parsed: VectorSearchResult = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.entity_id, "ent-1");
        assert_eq!(parsed.score, 0.95);
    }

    #[test]
    fn code_vector_store_creation() {
        let store = WeaviateCodeVectorStore::new("http://localhost:8080", None);
        assert_eq!(store.base_url, "http://localhost:8080");
    }

    #[test]
    fn repo_filter_json_shape() {
        let f = repo_filter_json("acme-12345678");
        assert_eq!(f["path"], serde_json::json!(["repoId"]));
        assert_eq!(f["operator"], "Equal");
        assert_eq!(f["valueText"], "acme-12345678");
    }

    #[test]
    fn repo_filter_json_quotes_are_safe() {
        // serde_json handles escaping; a hostile id with quotes/backslashes
        // round-trips to a normal string, not into the surrounding JSON.
        let raw = r#"weird"id\with"slashes"#;
        let f = repo_filter_json(raw);
        assert_eq!(f["valueText"], raw);
        let serialised = serde_json::to_string(&f).unwrap();
        let reparsed: serde_json::Value = serde_json::from_str(&serialised).unwrap();
        assert_eq!(reparsed["valueText"], raw);
    }

    #[test]
    fn repo_and_file_filter_json_shape() {
        let f = repo_and_file_filter_json("acme-12345678", "src/lib.rs");
        assert_eq!(f["operator"], "And");
        let operands = f["operands"].as_array().expect("operands array");
        assert_eq!(operands.len(), 2);
        assert_eq!(operands[0]["path"], serde_json::json!(["repoId"]));
        assert_eq!(operands[0]["valueText"], "acme-12345678");
        assert_eq!(operands[1]["path"], serde_json::json!(["filePath"]));
        assert_eq!(operands[1]["valueText"], "src/lib.rs");
    }

    #[test]
    fn delete_outcome_total_sums_successful_and_failed() {
        let o = DeleteOutcome {
            successful: 7,
            failed: 3,
        };
        assert_eq!(o.total(), 10);
    }

    #[test]
    fn batch_delete_response_parses_minimal_shape() {
        // Minimal shape Weaviate returns under output=minimal.
        let body = serde_json::json!({
            "match": { "class": "CodeEntity", "where": {} },
            "output": "minimal",
            "dryRun": false,
            "results": {
                "matches": 12930,
                "limit": 10000,
                "successful": 10000,
                "failed": 0,
                "objects": []
            }
        });
        let parsed: BatchDeleteResponse = serde_json::from_value(body).unwrap();
        assert_eq!(parsed.results.matches, 12930);
        assert_eq!(parsed.results.successful, 10000);
        assert_eq!(parsed.results.failed, 0);
    }

    // ─── InsertErrorSink wiring ───────────────────────────────────────

    struct CapturingSink {
        messages: Mutex<Vec<String>>,
    }
    impl CapturingSink {
        fn new() -> Self {
            Self {
                messages: Mutex::new(Vec::new()),
            }
        }
        fn snapshot(&self) -> Vec<String> {
            self.messages.lock().unwrap().clone()
        }
    }
    impl InsertErrorSink for CapturingSink {
        fn record(&self, message: String) {
            self.messages.lock().unwrap().push(message);
        }
    }

    fn fixture_entity() -> VectorCodeEntity {
        VectorCodeEntity {
            entity_id: "e1".into(),
            repo_id: "r1".into(),
            qualified_name: "q".into(),
            name: "n".into(),
            kind: "Function".into(),
            file_path: "f.rs".into(),
            language: "rust".into(),
            signature: String::new(),
            doc_comment: String::new(),
            summary: String::new(),
            start_line: 1,
            end_line: 2,
            generation: String::new(),
            searchable_content: "n".into(),
        }
    }

    #[tokio::test]
    async fn index_entities_records_error_on_send_failure() {
        // 127.0.0.1:1 has no listener — send() fails fast.
        let store = WeaviateCodeVectorStore::new("http://127.0.0.1:1", None);
        let sink = Arc::new(CapturingSink::new());
        store.set_insert_error_sink(Arc::clone(&sink) as Arc<dyn InsertErrorSink>);
        let res = store.index_entities(&[fixture_entity()]).await;
        assert!(res.is_err());
        let recorded = sink.snapshot();
        assert_eq!(recorded.len(), 1);
        assert!(
            recorded[0].contains("Weaviate batch index failed"),
            "{recorded:?}"
        );
    }

    #[tokio::test]
    async fn index_entities_no_record_on_empty_input() {
        let store = WeaviateCodeVectorStore::new("http://127.0.0.1:1", None);
        let sink = Arc::new(CapturingSink::new());
        store.set_insert_error_sink(Arc::clone(&sink) as Arc<dyn InsertErrorSink>);
        let n = store.index_entities(&[]).await.unwrap();
        assert_eq!(n, 0);
        assert!(sink.snapshot().is_empty());
    }

    #[tokio::test]
    async fn set_insert_error_sink_overwrites_previous() {
        let store = WeaviateCodeVectorStore::new("http://127.0.0.1:1", None);
        let first = Arc::new(CapturingSink::new());
        let second = Arc::new(CapturingSink::new());
        store.set_insert_error_sink(Arc::clone(&first) as Arc<dyn InsertErrorSink>);
        store.set_insert_error_sink(Arc::clone(&second) as Arc<dyn InsertErrorSink>);
        let _ = store.index_entities(&[fixture_entity()]).await;
        assert!(first.snapshot().is_empty());
        assert_eq!(second.snapshot().len(), 1);
    }

    fn fixture_entity_with_content(searchable: &str) -> VectorCodeEntity {
        VectorCodeEntity {
            searchable_content: searchable.into(),
            ..fixture_entity()
        }
    }

    fn fixture_summary(text: &str) -> VectorCodeSummary {
        VectorCodeSummary {
            entity_id: "e1".into(),
            repo_id: "r1".into(),
            granularity: "file".into(),
            file_path: "f.rs".into(),
            summary_text: text.into(),
            is_stale: false,
            generation: String::new(),
        }
    }

    #[tokio::test]
    async fn index_entities_skips_non_vectorizable_records_in_batch() {
        let store = WeaviateCodeVectorStore::new("http://127.0.0.1:1", None);
        let inputs = vec![
            fixture_entity_with_content(""),
            fixture_entity_with_content("   "),
            fixture_entity_with_content("🚀"),
        ];
        let count = store
            .index_entities(&inputs)
            .await
            .expect("batch of all-bad records must return Ok(0), not error");
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn index_summaries_skips_non_vectorizable_records_in_batch() {
        let store = WeaviateCodeVectorStore::new("http://127.0.0.1:1", None);
        let inputs = vec![
            fixture_summary(""),
            fixture_summary("\t\n"),
            fixture_summary("---"),
        ];
        let count = store.index_summaries(&inputs).await.unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn search_entities_returns_empty_for_non_vectorizable_query() {
        let store = WeaviateCodeVectorStore::new("http://127.0.0.1:1", None);
        for q in ["", "  ", "!?", "🚀"] {
            let out = store
                .search_entities(q, None, None, 10)
                .await
                .expect("blank query must not error");
            assert!(out.is_empty());
        }
    }

    /// What is logged while `f` runs, as text.
    async fn logged<F: std::future::Future>(f: F) -> String {
        #[derive(Clone, Default)]
        struct Captured(Arc<Mutex<Vec<u8>>>);
        impl std::io::Write for Captured {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let captured = Captured::default();
        let writer = captured.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || writer.clone())
            .with_ansi(false)
            .finish();
        let _logging = tracing::subscriber::set_default(subscriber);
        f.await;
        let bytes = captured.0.lock().unwrap().clone();
        String::from_utf8(bytes).unwrap()
    }

    /// Operators alert on these words, so they stay as they were.
    #[tokio::test]
    async fn skipped_records_are_logged_in_weaviates_words() {
        let store = WeaviateCodeVectorStore::new("http://127.0.0.1:1", None);
        let entities = logged(store.index_entities(&[fixture_entity_with_content("")])).await;
        assert!(
            entities.contains("skipped non-vectorizable entities in batch")
                && entities.contains("operation=\"code_index_entities\"")
                && entities.contains("filtered=1"),
            "{entities}"
        );
        let summaries = logged(store.index_summaries(&[fixture_summary("")])).await;
        assert!(
            summaries.contains("skipped non-vectorizable summaries in batch"),
            "{summaries}"
        );
    }

    #[test]
    fn partition_entities_separates_good_and_bad() {
        let inputs = vec![
            fixture_entity_with_content("hello"),
            fixture_entity_with_content(""),
            fixture_entity_with_content("🚀"),
            fixture_entity_with_content("world"),
        ];
        let (kept, skipped) = partition_vectorizable(&inputs, entity_text);
        assert_eq!(kept.len(), 2);
        assert_eq!(skipped, 2);
        let kept_searchable: Vec<&str> = kept.iter().map(|(_, v)| v.as_str()).collect();
        assert_eq!(kept_searchable, vec!["hello", "world"]);
    }
}
