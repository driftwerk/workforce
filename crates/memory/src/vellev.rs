//! Memories in a vellev collection, one tenant's, held as the same raw
//! objects Weaviate holds, so the two stores sync with each other.
//!
//! A memory's object id is its memory id, but every lookup goes by the
//! `memoryId` property, so objects imported from Weaviate keep theirs.
//! Unlike Weaviate, vellev filters text exactly, compares event times on
//! the index and increments atomically, so tags, event windows, counts and
//! importance all mean here what they mean in SQLite.

use std::collections::{BTreeMap, HashSet};
use std::path::Path;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use eyre::{eyre, Result};
use serde_json::{json, Map, Value};
use tracing::{debug, info, warn};
use uuid::Uuid;
use vellev_client::api::{
    Change, Changes, Found, Hybrid, Keyword, Nearest, OnlyIf, Outcome, Page, Results, Stored,
    Weighted, Written,
};
use vellev_client::{Records, Tenant};
use vellev_core::{
    Collection, CollectionName, DataType, Embedding, EmbeddingProvider, ObjectId, Property,
};
use vellev_index::filter::Filter;
use workforce_common::shadow::index_call;
use workforce_common::vectorizable::VectorizableText;
use workforce_core::types::AgentId;

use crate::access_audit::AuditOperation;
use crate::admin::{
    self, MemoryAdmin, RecoveryOutcome, Switchable, SyncSource, SyncTarget, EXPORT_PAGE,
    IMPORT_BATCH_SIZE,
};
use crate::embedding::{
    certainty, map_score, rank_embedded, EmbedKind, EmbeddingBackend, Reranker,
};
use crate::shape::{self, decode_memory, layer_to_string, visibility_to_string};
use crate::store::{
    apply_pollution_default, check_and_audit_scope, post_filtered, MemoryStore, IMPORTANCE_FLOOR,
};
use crate::types::{Memory, MemoryFilter, MemoryId, MemorySearchResult, Visibility};
use workforce_vellev::{
    collection, is_conflict, local_embedding, Asked, Cap, Timeouts, MAX_RESULTS,
};

/// The collection memories live in when the node embeds them: Weaviate's,
/// so that a tenant imported from Weaviate is read where it was put.
pub const DEFAULT_COLLECTION: &str = "WorkforceMemory";

/// Text embedded once to learn how many dimensions the hub's vectors have.
const DIMENSION_PROBE: &str = "dimension probe";

/// Objects per page of a walk through a collection.
const SCAN_PAGE: usize = 500;

/// Memory ids per purge request.
const PURGE_CHUNK: usize = 100;

/// What to do about a memory collection holding vectors copied from
/// Weaviate that the node cannot embed queries to match: memories are not
/// rebuilt from anywhere, so they are copied again for the node to embed.
const REIMPORT: &str = "import it again without its vectors, or mirror the Weaviate collection into a new one through the memory switch, so that the node embeds every memory";

/// How a compare-and-set that other writers keep beating is tried again:
/// up to `tries` times, after a jittered wait growing to `ceiling`. Twenty
/// writers adjusting one importance at once each need up to twenty tries,
/// one winning each round.
#[derive(Debug, Clone, Copy)]
struct Retry {
    tries: u32,
    ceiling: Duration,
}

impl Retry {
    const DEFAULT: Retry = Retry {
        tries: 32,
        ceiling: Duration::from_millis(250),
    };

    /// The jittered wait after try `attempt`.
    fn wait(&self, attempt: u32) -> Duration {
        let grown = (self.ceiling / 32).saturating_mul(2u32.saturating_pow(attempt));
        grown.min(self.ceiling).mul_f64(rand::random::<f64>())
    }
}

/// The properties a memory is stored with, as in Weaviate's schema.
const PROPERTIES: &[(&str, DataType)] = &[
    ("content", DataType::Text),
    ("memoryId", DataType::Text),
    ("layer", DataType::Text),
    ("owner", DataType::Text),
    ("visibility", DataType::Text),
    ("importance", DataType::Number),
    ("tags", DataType::TextArray),
    ("deleted", DataType::Bool),
    ("accessCount", DataType::Int),
    ("createdAt", DataType::Date),
    ("lastAccessed", DataType::Date),
    ("source", DataType::Text),
    ("identityId", DataType::Text),
    ("sourceSessionId", DataType::Text),
    ("sourceMessageIds", DataType::TextArray),
    ("recurrenceCount", DataType::Int),
    ("lastRecurrenceAt", DataType::Date),
    ("eventTime", DataType::Date),
];

/// vellev-backed memory store.
pub struct VellevStore {
    tenant: Tenant,
    /// The tenant as writes the node embeds, a batch at a time, reach it.
    writes: Tenant,
    collection: CollectionName,
    records: Records,
    /// The most results the node gives for one request.
    cap: Cap,
    /// When set, the hub embeds content and queries; the collection holds
    /// these vectors. Otherwise the node embeds with `local`.
    embedder: Option<Arc<dyn EmbeddingBackend>>,
    /// The model the node runs for this collection when the hub supplies no
    /// embedder: bge-base unless [`Self::with_local_model`] names another.
    local: Embedding,
    reranker: Option<Arc<dyn Reranker>>,
    /// Vector weight in hybrid recall (0 = keywords only, 1 = vectors only).
    alpha: f32,
    rerank_candidates: usize,
    /// The collection this one replaced, kept for rollback: forgetting a
    /// memory here forgets it there too, so rolling back never brings a
    /// forgotten memory back.
    previous: OnceLock<Arc<dyn MemoryStore>>,
    /// Maps the hub's certainty onto the scale thresholds are tuned on.
    score_map: Vec<(f32, f32)>,
    /// How many dimensions the hub's vectors have, once probed.
    dimensions: tokio::sync::OnceCell<u32>,
    retry: Retry,
}

/// Where a recall's query vector comes from.
enum QueryVector {
    /// Made by the store's embedder.
    Embed,
    /// Given (`None`: it could not be made: by keywords alone).
    Given(Option<Vec<f32>>),
}

impl VellevStore {
    /// The store of `tenant`'s memories in `collection` on the node at
    /// `url`, presenting `api_key`. The collection is named as it is in
    /// Weaviate, so a name vellev would have to change is refused: the
    /// switch marker of a collection imported from Weaviate is found by it.
    pub fn new(url: &str, api_key: &str, tenant: &str, collection: &str) -> Result<Self> {
        Self::new_with_timeouts(url, api_key, tenant, collection, Timeouts::default())
    }

    /// As [`Self::new`], each try of a request bounded by `timeouts`.
    pub fn new_with_timeouts(
        url: &str,
        api_key: &str,
        tenant: &str,
        collection: &str,
        timeouts: Timeouts,
    ) -> Result<Self> {
        let tenants = workforce_vellev::connect_with(url, api_key, tenant, timeouts)?;
        let collection = CollectionName::new(collection)?;
        Ok(Self {
            records: tenants.reads.collection(&collection),
            tenant: tenants.reads,
            writes: tenants.writes,
            collection,
            cap: Cap::default(),
            embedder: None,
            local: local_embedding(&["content"]),
            reranker: None,
            alpha: 0.75,
            rerank_candidates: 30,
            previous: OnceLock::new(),
            score_map: Vec::new(),
            dimensions: tokio::sync::OnceCell::new(),
            retry: Retry::DEFAULT,
        })
    }

    /// Have the node embed with `model` at `dimensions` instead of bge-base,
    /// prefixing queries with `query_prefix` when the model wants one. The
    /// edge's thresholds are tuned on MiniLM, so it names that here. Ignored
    /// once [`Self::with_embeddings`] supplies the vectors.
    pub fn with_local_model(
        mut self,
        model: &str,
        dimensions: u32,
        query_prefix: Option<&str>,
    ) -> Self {
        self.local = Embedding {
            model: model.to_string(),
            dimensions,
            query_prefix: query_prefix.map(str::to_string),
            ..local_embedding(&["content"])
        };
        self
    }

    /// Embed with the hub's model: the collection holds the hub's vectors;
    /// recall is hybrid at `alpha`, optionally reranked among at most
    /// [`MAX_RESULTS`] candidates.
    pub fn with_embeddings(
        mut self,
        embedder: Arc<dyn EmbeddingBackend>,
        reranker: Option<Arc<dyn Reranker>>,
        alpha: f32,
        rerank_candidates: usize,
    ) -> Self {
        self.embedder = Some(embedder);
        self.reranker = reranker;
        self.alpha = alpha.clamp(0.0, 1.0);
        if rerank_candidates > MAX_RESULTS {
            warn!(
                rerank_candidates,
                max = MAX_RESULTS,
                "rerank candidates cut to what vellev gives for one search"
            );
        }
        self.rerank_candidates = rerank_candidates.clamp(1, MAX_RESULTS);
        self
    }

    /// Report certainties through `knots` (see [`crate::embedding::map_score`]).
    pub fn with_score_map(mut self, knots: Vec<(f32, f32)>) -> Self {
        self.score_map = knots;
        self
    }

    /// How the collection embeds: with the hub's embedder, it holds the
    /// hub's vectors, whose size is learnt by embedding a probe once.
    async fn embedding(&self) -> Result<Embedding> {
        let Some(embedder) = &self.embedder else {
            return Ok(self.local.clone());
        };
        let dimensions = self
            .dimensions
            .get_or_try_init(|| async {
                Ok::<_, eyre::Report>(u32::try_from(embedder.embed(DIMENSION_PROBE).await?.len())?)
            })
            .await?;
        Ok(Embedding {
            provider: EmbeddingProvider::None,
            model: embedder.model_id().to_string(),
            dimensions: *dimensions,
            query_prefix: None,
            document_prefix: None,
            properties: Vec::new(),
        })
    }

    /// Delete every memory passing `filter`, however many match (see
    /// [`workforce_vellev::delete_where`]), and return how many went.
    async fn delete_where(&self, filter: &Filter) -> Result<u64> {
        let deleted =
            workforce_vellev::delete_where(&self.tenant, &self.writes, &self.collection, filter)
                .await;
        self.answered(deleted).await
    }

    /// A failed request, saying what to fix when an operator has to.
    fn failed(&self, error: vellev_client::Error) -> eyre::Report {
        workforce_vellev::failure(error, &self.tenant, Asked::Request(&self.collection))
    }

    /// What a request came to, its failure explained as [`Self::failed`]
    /// does; a request not found is told apart by asking whether the
    /// tenant is there, so that a tenant gone is named as such.
    async fn answered<T>(&self, answer: vellev_client::Result<T>) -> Result<T> {
        let error = match answer {
            Ok(answered) => return Ok(answered),
            Err(error) => error,
        };
        if error.is_not_found() {
            if let Err(gone) = self.tenant.collections().await {
                return Err(workforce_vellev::failure(gone, &self.tenant, Asked::Tenant));
            }
        }
        Err(self.failed(error))
    }

    /// What `erase` erased; or nothing, if it failed because the
    /// collection has not been made: a collection never made holds
    /// nothing to erase, as a shadow replica the reconcile has not yet
    /// seeded does not. Any other failure, the tenant gone among them,
    /// stays a failure.
    ///
    /// Not made means the tenant answered its list of collections and
    /// this one is not in it. [`Self::exists`] cannot say so: a schema
    /// read of a tenant that is not there is not found as well, and
    /// would pass a dropped or misnamed tenant for an empty one.
    async fn unless_unmade(
        &self,
        erase: impl std::future::Future<Output = Result<usize>>,
    ) -> Result<usize> {
        let failed = match erase.await {
            Ok(erased) => return Ok(erased),
            Err(failed) => failed,
        };
        match self.tenant.collections().await {
            Ok(held) if !held.iter().any(|c| c.name == self.collection) => Ok(0),
            _ => Err(failed),
        }
    }

    /// Mark memories as promoted to this collection: served from it, no
    /// longer a replica of Weaviate (DW-676, `workforce shadow promote`).
    /// Idempotent.
    pub async fn mark_promoted(&self) -> Result<()> {
        workforce_vellev::mark_promoted(&self.tenant, self.collection.as_str()).await
    }

    /// Take the promotion's mark away (`workforce shadow rollback`).
    pub async fn unmark_promoted(&self) -> Result<()> {
        workforce_vellev::unmark_promoted(&self.tenant, self.collection.as_str()).await
    }

    /// Drop the collection if it holds nothing at all, deleted memories
    /// included; `Ok(true)` once it is gone, or was never made, and
    /// `Ok(false)` if it holds anything, which is never dropped.
    pub async fn drop_if_empty(&self) -> Result<bool> {
        if !self.exists().await? {
            return Ok(true);
        }
        if !self.holds_nothing().await? {
            return Ok(false);
        }
        self.delete_collection().await?;
        Ok(true)
    }

    /// Whether the collection holds no object at all, deleted memories
    /// included, or is not there.
    pub async fn holds_nothing(&self) -> Result<bool> {
        if !self.exists().await? {
            return Ok(true);
        }
        Ok(self.answered(self.records.count(None).await).await? == 0)
    }

    /// Whether the collection exists in the tenant.
    pub async fn exists(&self) -> Result<bool> {
        Ok(self.answered(self.records.schema().await).await?.is_some())
    }

    /// The collection as this store needs it.
    async fn wanted(&self) -> Result<Collection> {
        Ok(collection(
            self.collection.clone(),
            memory_properties(),
            self.embedding().await?,
        ))
    }

    /// Up to `limit` objects passing `filter`, deleted ones included unless
    /// the filter says otherwise, read a page at a time.
    async fn listed(&self, filter: Filter, limit: usize) -> Result<Vec<Stored>> {
        self.listed_with(filter, limit, false).await
    }

    /// As [`Self::listed`], with their vectors when asked for.
    async fn listed_with(
        &self,
        filter: Filter,
        limit: usize,
        vectors: bool,
    ) -> Result<Vec<Stored>> {
        let mut listed = Vec::new();
        let mut after = None;
        while listed.len() < limit {
            let wanted = (limit - listed.len()).min(SCAN_PAGE);
            let page = self
                .answered(
                    self.cap
                        .ask(wanted, |limit| {
                            let page = Page {
                                filter: Some(filter.clone()),
                                after,
                                limit,
                                vectors,
                            };
                            async move { self.records.list(&page).await }
                        })
                        .await,
                )
                .await?;
            listed.extend(page.objects);
            match page.next {
                Some(next) => after = Some(next),
                None => break,
            }
        }
        Ok(listed)
    }

    /// Every object passing `filter`.
    async fn every(&self, filter: Filter) -> Result<Vec<Stored>> {
        self.listed(filter, usize::MAX).await
    }

    /// The objects holding memory `id`.
    async fn objects_of(&self, id: &MemoryId) -> Result<Vec<ObjectId>> {
        Ok(self
            .every(by_memory_id(id))
            .await?
            .into_iter()
            .map(|object| object.id)
            .collect())
    }

    /// The object holding memory `id`; `Err` when there is none.
    async fn object_of(&self, id: &MemoryId) -> Result<Stored> {
        self.listed(by_memory_id(id), 1)
            .await?
            .into_iter()
            .next()
            .ok_or_else(|| eyre!("memory {} not found", id.0))
    }

    /// Merge `properties` into every object in `ids`, a batch at a time.
    /// Returns how many there were.
    async fn patch_all(&self, ids: &[ObjectId], properties: Map<String, Value>) -> Result<usize> {
        for chunk in ids.chunks(IMPORT_BATCH_SIZE) {
            self.answered(
                self.writes
                    .apply(
                        chunk
                            .iter()
                            .map(|id| self.patch_change(*id, properties.clone(), None))
                            .collect(),
                    )
                    .await,
            )
            .await?;
        }
        Ok(ids.len())
    }

    fn patch_change(
        &self,
        id: ObjectId,
        properties: Map<String, Value>,
        only_if: Option<OnlyIf>,
    ) -> Change {
        Change::Patch {
            collection: self.collection.clone(),
            id,
            properties,
            vector: None,
            only_if,
        }
    }

    /// The hub-vectored candidates for `query`, scored by the certainty of
    /// their stored vectors against the query's, and whether they were: if
    /// the query cannot be embedded, they are found by keywords alone.
    /// [`MemoryStore::recall`], the query's vector made here or given.
    async fn recall_given(
        &self,
        query: &str,
        mut filter: MemoryFilter,
        vector: QueryVector,
    ) -> Result<Vec<MemorySearchResult>> {
        let query = match VectorizableText::try_new(query) {
            Ok(q) => q,
            Err(e) => {
                warn!(
                    target: "vectorizable",
                    operation = "memory_recall",
                    reason = e.as_metric_label(),
                    "rejected non-vectorizable query, returning empty results"
                );
                return Ok(Vec::new());
            }
        };
        if !check_and_audit_scope(&filter, AuditOperation::Recall) {
            return Ok(Vec::new());
        }
        apply_pollution_default(&mut filter);
        let limit = filter.limit.unwrap_or(10);
        let where_filter = where_filter(&filter);
        let (candidates, vectored) = match &self.embedder {
            Some(embedder) => {
                self.embedded_candidates(
                    embedder.as_ref(),
                    query.as_str(),
                    vector,
                    &filter,
                    where_filter,
                    limit,
                )
                .await?
            }
            None => {
                self.local_candidates(query.as_str(), &filter, where_filter, limit)
                    .await?
            }
        };
        let ranked = rank_embedded(
            self.reranker.as_deref(),
            query.as_str(),
            candidates,
            filter.min_certainty,
            vectored,
        )
        .await;
        Ok(post_filtered(
            &filter,
            AuditOperation::Recall,
            ranked,
            limit,
        ))
    }

    async fn embedded_candidates(
        &self,
        embedder: &dyn EmbeddingBackend,
        query: &str,
        vector: QueryVector,
        filter: &MemoryFilter,
        where_filter: Filter,
        limit: usize,
    ) -> Result<(Vec<MemorySearchResult>, bool)> {
        let query_vector = match vector {
            QueryVector::Given(given) => given,
            QueryVector::Embed => embedder
                .embed_query(query)
                .await
                .map_err(|e| {
                    warn!(target: "embeddings", error = %e, "query embedding failed; recalling by keywords only")
                })
                .ok(),
        };
        let limit = match self.reranker {
            Some(_) => limit.max(self.rerank_candidates),
            None => limit,
        };
        let found = match &query_vector {
            Some(vector) => {
                let alpha = filter.hybrid_alpha.unwrap_or(self.alpha);
                self.answered(
                    self.hybrid(query, Some(vector), alpha, &where_filter, limit)
                        .await,
                )
                .await?
            }
            None => {
                self.answered(
                    self.cap
                        .ask(limit, |limit| {
                            let keyword = Keyword {
                                text: query.to_string(),
                                properties: content_only(),
                                limit,
                                filter: Some(where_filter.clone()),
                                vectors: false,
                            };
                            async move { index_call(self.records.keyword(&keyword)).await }
                        })
                        .await,
                )
                .await?
            }
        };
        let scored = found
            .results
            .into_iter()
            .filter_map(|found| {
                let score = match (&query_vector, &found.object.vector) {
                    (Some(query), Some(stored)) => {
                        map_score(&self.score_map, certainty(query, stored))
                    }
                    // Keyword fallback (or a memory not yet embedded): no
                    // absolute relevance, so nothing clears a threshold.
                    _ => 0.0,
                };
                decode(found.object).map(|memory| MemorySearchResult { memory, score })
            })
            .collect();
        Ok((scored, query_vector.is_some()))
    }

    /// Candidates for `query` from a collection the node embeds for, and
    /// whether they are scored by certainty. As in Weaviate, without a
    /// hybrid alpha the search is by meaning alone and every result has a
    /// certainty; with one, it is hybrid, and the fused score orders.
    async fn local_candidates(
        &self,
        query: &str,
        filter: &MemoryFilter,
        where_filter: Filter,
        limit: usize,
    ) -> Result<(Vec<MemorySearchResult>, bool)> {
        let scored = |found: Vec<Found>, score: fn(&Found) -> Option<f32>| {
            found
                .into_iter()
                .filter_map(|found| {
                    let score = score(&found).unwrap_or(0.0);
                    decode(found.object).map(|memory| MemorySearchResult { memory, score })
                })
                .collect()
        };
        Ok(match filter.hybrid_alpha {
            None => {
                let found = self
                    .answered(
                        self.cap
                            .ask(limit, |limit| {
                                let nearest = Nearest {
                                    vector: None,
                                    text: Some(query.to_string()),
                                    limit,
                                    filter: Some(where_filter.clone()),
                                    oversample: None,
                                    vectors: false,
                                };
                                async move { index_call(self.records.nearest(&nearest)).await }
                            })
                            .await,
                    )
                    .await?;
                // The model's certainty, on the scale the caller's floors
                // were tuned on.
                let mut scored: Vec<MemorySearchResult> =
                    scored(found.results, |found| found.certainty);
                for hit in &mut scored {
                    hit.score = map_score(&self.score_map, hit.score);
                }
                (scored, true)
            }
            Some(alpha) => {
                let found = self
                    .answered(self.hybrid(query, None, alpha, &where_filter, limit).await)
                    .await?;
                (scored(found.results, |found| found.score), false)
            }
        })
    }

    /// A hybrid search for `query` on content at `alpha`: with the query's
    /// `vector`, the results carry their own for scoring; without, the node
    /// embeds the query.
    async fn hybrid(
        &self,
        query: &str,
        vector: Option<&[f32]>,
        alpha: f32,
        where_filter: &Filter,
        limit: usize,
    ) -> vellev_client::Result<Results> {
        self.cap
            .ask(limit, |limit| {
                let hybrid = Hybrid {
                    text: query.to_string(),
                    vectors: vector.is_some(),
                    vector: vector.map(<[f32]>::to_vec),
                    alpha: Some(alpha),
                    fusion: Default::default(),
                    properties: content_only(),
                    limit,
                    candidates: None,
                    filter: Some(where_filter.clone()),
                    oversample: None,
                };
                async move { index_call(self.records.hybrid(&hybrid)).await }
            })
            .await
    }

    /// One object of an export as Weaviate gives it.
    /// Store `batch` as one step: each object with the vector it came
    /// with, or one the hub's embedder makes, or none where the node
    /// embeds.
    async fn import_batch(&self, batch: &[(ObjectId, Value)]) -> Result<()> {
        let unvectored: Vec<String> = batch
            .iter()
            .filter(|(_, o)| !o["vector"].is_array())
            .map(|(_, o)| {
                o["properties"]["content"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string()
            })
            .collect();
        let embedded: Vec<Option<Vec<f32>>> = match SyncTarget::vectors(self, &unvectored).await? {
            Some(vectors) if vectors.len() != unvectored.len() => {
                return Err(eyre!(
                    "the embedder gave {} vectors for {} memories",
                    vectors.len(),
                    unvectored.len()
                ))
            }
            Some(vectors) => vectors.into_iter().map(Some).collect(),
            None => vec![None; unvectored.len()],
        };
        let mut embedded = embedded.into_iter();
        let changes = batch
            .iter()
            .map(|(id, object)| {
                let vector = if object["vector"].is_array() {
                    serde_json::from_value(object["vector"].clone()).ok()
                } else {
                    embedded.next().flatten()
                };
                Change::Upsert {
                    collection: self.collection.clone(),
                    id: *id,
                    properties: declared(&object["properties"]),
                    vector,
                }
            })
            .collect();
        self.writes
            .apply(changes)
            .await
            .map_err(|e| self.failed(e))?;
        Ok(())
    }

    fn exported(&self, object: Stored) -> Value {
        let mut exported = json!({
            "id": object.id.to_string(),
            "class": self.collection.as_str(),
            "properties": weaviate_shaped(object.properties),
        });
        if let Some(vector) = object.vector {
            exported["vector"] = json!(vector);
        }
        exported
    }
}

/// The properties of a memory as recall needs them indexed: content
/// searched by its words and never filtered on, everything else filtered on.
fn memory_properties() -> Vec<Property> {
    PROPERTIES
        .iter()
        .map(|(name, data_type)| match *name {
            "content" => Property {
                filterable: false,
                ..Property::new(name, *data_type).searchable()
            },
            _ => Property::new(name, *data_type),
        })
        .collect()
}

fn declared_type(name: &str) -> Option<DataType> {
    PROPERTIES
        .iter()
        .find(|(declared, _)| *declared == name)
        .map(|(_, data_type)| *data_type)
}

fn eq(property: &str, value: impl Into<Value>) -> Filter {
    Filter::Eq {
        property: property.to_string(),
        value: value.into(),
    }
}

fn is_null(property: &str) -> Filter {
    Filter::IsNull {
        property: property.to_string(),
    }
}

fn gte(property: &str, at: &DateTime<Utc>) -> Filter {
    Filter::Gte {
        property: property.to_string(),
        value: at.to_rfc3339().into(),
    }
}

fn lte(property: &str, at: &DateTime<Utc>) -> Filter {
    Filter::Lte {
        property: property.to_string(),
        value: at.to_rfc3339().into(),
    }
}

/// A bound on when a memory's event happened: its `eventTime`, or for a
/// memory without one, when it was made.
fn event(bound: fn(&str, &DateTime<Utc>) -> Filter, at: &DateTime<Utc>) -> Filter {
    Filter::Or(vec![
        Filter::And(vec![
            Filter::Not(Box::new(is_null("eventTime"))),
            bound("eventTime", at),
        ]),
        Filter::And(vec![is_null("eventTime"), bound("createdAt", at)]),
    ])
}

fn by_memory_id(id: &MemoryId) -> Filter {
    eq("memoryId", id.0.as_str())
}

fn about(owner: &AgentId, identity: &str) -> Filter {
    Filter::And(vec![
        eq("owner", owner.as_str()),
        eq("identityId", identity),
    ])
}

/// The filter `filter` describes, never matching a forgotten memory.
fn where_filter(filter: &MemoryFilter) -> Filter {
    let identity = filter
        .identity_id
        .as_deref()
        .map(|id| match filter.team_id.as_deref() {
            Some(team) => Filter::Or(vec![
                eq("identityId", id),
                eq("visibility", format!("team:{team}")),
            ]),
            None => eq("identityId", id),
        });
    let tagged = filter
        .tags
        .as_ref()
        .filter(|tags| !tags.is_empty())
        .map(|tags| Filter::Or(tags.iter().map(|tag| eq("tags", tag.as_str())).collect()));
    let conditions = [
        Some(eq("deleted", false)),
        filter
            .layer
            .as_ref()
            .map(|l| eq("layer", layer_to_string(l))),
        filter.owner.as_ref().map(|o| eq("owner", o.as_str())),
        filter
            .visibility
            .as_ref()
            .map(|v| eq("visibility", visibility_to_string(v))),
        filter.min_importance.map(|min| Filter::Gte {
            property: "importance".to_string(),
            value: json!(decimal(min)),
        }),
        filter.source.as_ref().map(|s| eq("source", s.as_str())),
        identity,
        filter.created_after.map(|at| gte("createdAt", &at)),
        filter.created_before.map(|at| lte("createdAt", &at)),
        filter.event_after.map(|at| event(gte, &at)),
        filter.event_before.map(|at| event(lte, &at)),
        tagged,
    ];
    let excluded = filter.excluded_tags.iter().map(|tag| Filter::Ne {
        property: "tags".to_string(),
        value: tag.as_str().into(),
    });
    Filter::And(conditions.into_iter().flatten().chain(excluded).collect())
}

/// `x` as the decimal it was written as, rather than the binary fraction
/// an `f32` widened to `f64` would be: `0.3`, not `0.30000001192092896`.
fn decimal(x: f32) -> f64 {
    x.to_string().parse().unwrap_or(f64::from(x))
}

fn content_only() -> Vec<Weighted> {
    vec![Weighted {
        property: "content".to_string(),
        weight: 1.0,
    }]
}

fn props<const N: usize>(pairs: [(&str, Value); N]) -> Map<String, Value> {
    pairs
        .into_iter()
        .map(|(name, value)| (name.to_string(), value))
        .collect()
}

fn decode(object: Stored) -> Option<Memory> {
    decode_memory(&Value::Object(object.properties))
}

fn object_id(id: &str) -> Result<ObjectId> {
    Ok(ObjectId(Uuid::parse_str(id)?))
}

/// The value the change at `index` of `written` left, as a number.
fn value_of(written: &Written, index: usize) -> Result<f64> {
    match written.outcomes.get(index) {
        Some(Outcome::Value { value }) => value
            .as_f64()
            .ok_or_else(|| eyre!("vellev answered an increment with {value}")),
        other => Err(eyre!("vellev answered an increment with {other:?}")),
    }
}

/// Of a raw object's properties, those a memory has, so that one stray
/// property does not fail a whole batch.
fn declared(properties: &Value) -> Map<String, Value> {
    properties
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(name, _)| declared_type(name).is_some())
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect()
}

/// Properties as Weaviate exports them, so that the same memory compares
/// equal in both stores: dates in UTC with as many fractional digits as
/// they need, whole numbers without a fraction, and no nulls.
fn weaviate_shaped(properties: Map<String, Value>) -> Map<String, Value> {
    properties
        .into_iter()
        .filter(|(_, value)| !value.is_null())
        .map(|(name, value)| {
            let shaped = match (declared_type(&name), &value) {
                (Some(DataType::Date), Value::String(at)) => DateTime::parse_from_rfc3339(at)
                    .map(|at| json!(weaviate_date(at.with_timezone(&Utc))))
                    .unwrap_or(value),
                (Some(DataType::Number), Value::Number(n)) => n
                    .as_f64()
                    .filter(|n| n.fract() == 0.0 && n.abs() < 2f64.powi(53))
                    .map_or(value, |n| json!(n as i64)),
                _ => value,
            };
            (name, shaped)
        })
        .collect()
}

/// `at` as Go's RFC 3339 with nanoseconds writes it, which Weaviate uses.
fn weaviate_date(at: DateTime<Utc>) -> String {
    let seconds = at.format("%Y-%m-%dT%H:%M:%S");
    let nanos = format!("{:09}", at.timestamp_subsec_nanos());
    match nanos.trim_end_matches('0') {
        "" => format!("{seconds}Z"),
        fraction => format!("{seconds}.{fraction}Z"),
    }
}

#[async_trait]
impl MemoryAdmin for VellevStore {
    fn collection(&self) -> &str {
        self.collection.as_str()
    }

    /// Create the collection, or add what an existing one lacks and check
    /// the rest (see [`workforce_vellev::ensure`]); then, where the node
    /// embeds memories, embed one probe, so that a node that cannot run its
    /// model fails readiness, saying so, rather than every write after.
    async fn ensure_collection(&self) -> Result<()> {
        let wanted = self.wanted().await?;
        workforce_vellev::ensure(&self.tenant, &self.records, &wanted, REIMPORT).await?;
        workforce_vellev::probe_model(&self.tenant, &self.records, &wanted).await
    }

    /// vellev changes a collection in place, so there is nothing to rebuild.
    async fn ensure_collection_with_recovery(&self, _: Option<&Path>) -> Result<RecoveryOutcome> {
        self.ensure_collection().await?;
        Ok(RecoveryOutcome::AlreadyValid)
    }

    async fn doctor(&self) -> Result<()> {
        workforce_vellev::doctor(&self.tenant, &self.records, &self.wanted().await?, REIMPORT).await
    }

    /// Objects whose id is not a UUID are passed over, and so is a batch
    /// whose vectors the hub's embedder could not make; neither counts as
    /// imported.
    async fn import(&self, objects: &[Value]) -> Result<usize> {
        let prepared: Vec<(ObjectId, Value)> = objects
            .iter()
            .filter_map(admin::normalised)
            .filter_map(|object| match object["id"].as_str().map(object_id) {
                Some(Ok(id)) => Some((id, object)),
                _ => {
                    warn!(id = %object["id"], "an object without a UUID for an id is not imported");
                    None
                }
            })
            .collect();
        let filtered = objects.len() - prepared.len();
        let mut count = 0;
        // What could not be stored, by id, with why.
        let mut failed: Vec<(String, String)> = Vec::new();
        for chunk in prepared.chunks(IMPORT_BATCH_SIZE) {
            match self.import_batch(chunk).await {
                Ok(()) => count += chunk.len(),
                // One object at a time, so that the one the node or the
                // embedder will not take does not take the others with it.
                Err(e) => {
                    warn!(error = %e, batch = chunk.len(), "a batch was not imported into vellev; importing its objects one by one");
                    for one in chunk {
                        match self.import_batch(std::slice::from_ref(one)).await {
                            Ok(()) => count += 1,
                            Err(e) => failed.push((one.0 .0.to_string(), format!("{e:#}"))),
                        }
                    }
                }
            }
        }
        if filtered > 0 {
            warn!(
                target: "vectorizable",
                operation = "memory_import",
                filtered,
                imported = count,
                "skipped objects that cannot be embedded or have no usable id during import"
            );
        }
        admin::imported(count, failed)
    }

    async fn patch_tags(&self, id: &str, tags: &[String]) -> Result<()> {
        self.answered(
            self.records
                .patch(
                    object_id(id)?,
                    &Changes {
                        properties: props([("tags", json!(tags))]),
                        vector: None,
                        only_if: None,
                    },
                )
                .await,
        )
        .await?;
        Ok(())
    }

    async fn delete_collection(&self) -> Result<()> {
        match self.records.drop().await {
            Err(e) if !e.is_not_found() => Err(self.failed(e)),
            _ => {
                info!(collection = %self.collection, "Deleted vellev collection");
                Ok(())
            }
        }
    }
}

#[async_trait]
impl SyncSource for VellevStore {
    /// One page of the collection as Weaviate-shaped raw objects; vellev
    /// lists a collection in id order.
    async fn export_page(&self, after: Option<&str>, with_vectors: bool) -> Result<Vec<Value>> {
        let page = self
            .answered(
                self.records
                    .list(&Page {
                        filter: None,
                        after: after.map(object_id).transpose()?,
                        // Exactly a page: a shorter one is taken as the last, so a
                        // node that gives fewer fails the export rather than ending it.
                        limit: u32::try_from(EXPORT_PAGE)?,
                        vectors: with_vectors,
                    })
                    .await,
            )
            .await?;
        Ok(page
            .objects
            .into_iter()
            .map(|object| self.exported(object))
            .collect())
    }

    fn embedding_model(&self) -> Option<&str> {
        Some(
            self.embedder
                .as_ref()
                .map_or(self.local.model.as_str(), |embedder| embedder.model_id()),
        )
    }

    async fn objects_of_memory(&self, memory_id: &str, with_vectors: bool) -> Result<Vec<Value>> {
        let held = self
            .listed_with(
                by_memory_id(&MemoryId(memory_id.to_string())),
                usize::MAX,
                with_vectors,
            )
            .await?;
        Ok(held
            .into_iter()
            .map(|object| self.exported(object))
            .collect())
    }

    async fn objects_by_id(&self, ids: &[String], with_vectors: bool) -> Result<Vec<Value>> {
        let mut found = Vec::with_capacity(ids.len());
        for id in ids {
            let held = self
                .answered(self.records.get(object_id(id)?).await)
                .await?;
            if let Some(mut object) = held {
                if !with_vectors {
                    object.vector = None;
                }
                found.push(self.exported(object));
            }
        }
        Ok(found)
    }
}

#[async_trait]
impl SyncTarget for VellevStore {
    async fn upsert(&self, objects: &[Value]) -> Result<usize> {
        self.import(objects).await
    }

    /// Merge `properties` into an object, keeping its vector; an object
    /// gone already is left gone.
    async fn patch(&self, id: &str, properties: Value) -> Result<()> {
        let changes = Changes {
            properties: declared(&properties),
            vector: None,
            only_if: None,
        };
        match self.records.patch(object_id(id)?, &changes).await {
            Err(e) if !e.is_not_found() => Err(self.failed(e)),
            _ => Ok(()),
        }
    }

    async fn delete(&self, id: &str) -> Result<()> {
        self.answered(self.records.delete(object_id(id)?).await)
            .await?;
        Ok(())
    }

    async fn vectors(&self, texts: &[String]) -> Result<Option<Vec<Vec<f32>>>> {
        match &self.embedder {
            Some(embedder) if !texts.is_empty() => {
                Ok(Some(embedder.embed_many(texts, EmbedKind::Document).await?))
            }
            Some(_) => Ok(Some(Vec::new())),
            None => Ok(None),
        }
    }

    /// Whether memories have switched to this collection: a marker in the
    /// collection Weaviate keeps them in, so one imported comes along.
    async fn switched(&self) -> Result<bool> {
        workforce_vellev::marked(&self.tenant, self.collection.as_str()).await
    }

    /// Record the switch (see [`Self::switched`]); idempotent.
    async fn mark_switched(&self) -> Result<()> {
        workforce_vellev::mark(&self.tenant, self.collection.as_str()).await
    }

    /// A marker in workforce's own markers, which no import brings.
    async fn promoted(&self) -> Result<bool> {
        workforce_vellev::promoted(&self.tenant, self.collection.as_str()).await
    }
}

impl Switchable for VellevStore {
    /// The first store given is kept; a store switches from one other.
    fn keep_previous(&self, previous: Arc<dyn MemoryStore>) {
        let kept = self.previous.set(previous).is_ok();
        debug_assert!(kept, "a store switches from one other");
    }
}

#[async_trait]
impl MemoryStore for VellevStore {
    async fn remember(&self, memory: Memory) -> Result<MemoryId> {
        let content = VectorizableText::try_new(&memory.content).map_err(|e| {
            warn!(
                target: "vectorizable",
                operation = "memory_remember",
                reason = e.as_metric_label(),
                memory_id = %memory.id,
                "rejected non-vectorizable content"
            );
            eyre!("refusing to store memory with non-vectorizable content: {e}")
        })?;
        let vector = match &self.embedder {
            // Only the content is embedded: ids, owner and tags carry no
            // meaning and would crowd it out of the vector.
            Some(embedder) => embedder
                .embed(&memory.content)
                .await
                .map_err(|e| warn!(
                    target: "embeddings",
                    error = %e,
                    memory_id = %memory.id,
                    "embedding failed; memory stored without a vector, found by keywords until backfilled"
                ))
                .ok(),
            None => None,
        };
        let Value::Object(properties) = shape::properties(&memory, &content) else {
            unreachable!("a memory's properties are an object");
        };
        // A memory id that is not a UUID, as none made here is, still
        // stores: lookups go by the `memoryId` property.
        let id = object_id(&memory.id.0).unwrap_or_else(|_| ObjectId::generate());
        // The node embeds the memory within the request unless the hub did.
        self.answered(
            self.writes
                .apply(vec![Change::Upsert {
                    collection: self.collection.clone(),
                    id,
                    properties,
                    vector,
                }])
                .await,
        )
        .await?;
        debug!(memory_id = %memory.id, "Stored memory in vellev");
        Ok(memory.id)
    }

    async fn recall(&self, query: &str, filter: MemoryFilter) -> Result<Vec<MemorySearchResult>> {
        self.recall_given(query, filter, QueryVector::Embed).await
    }

    fn query_model(&self) -> Option<String> {
        self.embedder.as_ref().map(|e| e.model_id().to_string())
    }

    fn recall_settings(&self) -> Option<crate::store::RecallSettings> {
        self.embedder
            .as_ref()
            .map(|_| crate::store::RecallSettings {
                alpha: self.alpha,
                reranked: self.reranker.is_some(),
                rerank_candidates: self.rerank_candidates,
                score_map: self.score_map.clone(),
            })
    }

    async fn recall_with_vector(
        &self,
        query: &str,
        vector: Option<Vec<f32>>,
        filter: MemoryFilter,
    ) -> Result<Vec<MemorySearchResult>> {
        self.recall_given(query, filter, QueryVector::Given(vector))
            .await
    }

    /// A system-internal lookup by id, bypassing per-user scoping: callers
    /// check ownership against the returned `identity_id`. A forgotten
    /// memory is not returned.
    async fn get(&self, id: &MemoryId) -> Result<Option<Memory>> {
        let page = self
            .answered(
                self.records
                    .list(&Page {
                        filter: Some(Filter::And(vec![by_memory_id(id), eq("deleted", false)])),
                        after: None,
                        limit: 1,
                        vectors: false,
                    })
                    .await,
            )
            .await?;
        Ok(page.objects.into_iter().next().and_then(decode))
    }

    async fn forget(&self, id: &MemoryId) -> Result<()> {
        let objects = self.objects_of(id).await?;
        self.patch_all(&objects, props([("deleted", json!(true))]))
            .await?;
        debug!(memory_id = %id, "Soft-deleted memory in vellev");
        if let Some(previous) = self.previous.get() {
            if let Err(e) = previous.forget(id).await {
                warn!(target: "embeddings", memory_id = %id, error = %e, "forgetting in the previous collection failed");
            }
        }
        Ok(())
    }

    async fn share(&self, id: &MemoryId, visibility: Visibility) -> Result<()> {
        let objects = self.objects_of(id).await?;
        self.patch_all(
            &objects,
            props([("visibility", json!(visibility_to_string(&visibility)))]),
        )
        .await?;
        Ok(())
    }

    /// Counts what [`Self::list`] would list, without its limit.
    async fn count(&self, mut filter: MemoryFilter) -> Result<usize> {
        if !check_and_audit_scope(&filter, AuditOperation::Count) {
            return Ok(0);
        }
        apply_pollution_default(&mut filter);
        let counted = self
            .answered(self.records.count(Some(&where_filter(&filter))).await)
            .await?;
        Ok(usize::try_from(counted)?)
    }

    async fn list(&self, mut filter: MemoryFilter) -> Result<Vec<Memory>> {
        if !check_and_audit_scope(&filter, AuditOperation::Recall) {
            return Ok(Vec::new());
        }
        apply_pollution_default(&mut filter);
        let limit = filter.limit.unwrap_or(10);
        let memories = self
            .listed(where_filter(&filter), limit)
            .await?
            .into_iter()
            .filter_map(decode)
            .collect();
        Ok(post_filtered(
            &filter,
            AuditOperation::Recall,
            memories,
            limit,
        ))
    }

    async fn update_tags(&self, id: &MemoryId, tags: Vec<String>) -> Result<()> {
        let objects = self.objects_of(id).await?;
        if objects.is_empty() {
            return Err(eyre!("Memory '{}' not found in vellev", id.0));
        }
        self.patch_all(&objects, props([("tags", json!(tags))]))
            .await?;
        Ok(())
    }

    /// A compare-and-set: the importance read is replaced by the clamped
    /// sum only if it is still what was read, tried again after a jittered
    /// wait while other writers change it first, so that concurrent
    /// adjustments compose to their net delta. Nothing is written unless
    /// this returns `Ok`. A memory stored without an importance counts as
    /// 0.5, as it reads.
    async fn adjust_importance(&self, id: &MemoryId, delta: f32) -> Result<f32> {
        let object = self.object_of(id).await?.id;
        for attempt in 0..self.retry.tries {
            let stored = self
                .answered(self.records.get(object).await)
                .await?
                .ok_or_else(|| eyre!("memory {} not found", id.0))?;
            let read = stored
                .properties
                .get("importance")
                .cloned()
                .unwrap_or(Value::Null);
            let current = read.as_f64().map_or(0.5, |v| v as f32);
            let adjusted = (current + delta).clamp(IMPORTANCE_FLOOR, 1.0);
            let only_if = OnlyIf {
                property: "importance".to_string(),
                equals: read,
            };
            let change = self.patch_change(
                object,
                props([("importance", json!(adjusted))]),
                Some(only_if),
            );
            match self.writes.apply(vec![change]).await {
                Ok(_) => return Ok(adjusted),
                Err(e) if is_conflict(&e) => {
                    tokio::time::sleep(self.retry.wait(attempt)).await;
                }
                Err(e) => return Err(self.failed(e)),
            }
        }
        Err(eyre!(
            "importance of memory {} kept changing under {} tries; left unchanged",
            id.0,
            self.retry.tries
        ))
    }

    async fn identity_memories(&self, owner: &AgentId, identity: &str) -> Result<Vec<MemoryId>> {
        Ok(self
            .every(about(owner, identity))
            .await?
            .into_iter()
            .filter_map(|object| {
                Some(MemoryId(
                    object.properties.get("memoryId")?.as_str()?.to_string(),
                ))
            })
            .collect())
    }

    /// The rollback copy mostly repeats this one, so it raises a count to
    /// its own rather than adding to it.
    async fn identity_counts(&self, owner: &AgentId) -> Result<BTreeMap<String, usize>> {
        let mut counts = BTreeMap::<String, usize>::new();
        self.every(eq("owner", owner.as_str()))
            .await?
            .iter()
            .filter_map(|object| object.properties.get("identityId")?.as_str())
            .filter(|identity| !identity.is_empty())
            .for_each(|identity| *counts.entry(identity.to_string()).or_default() += 1);
        if let Some(previous) = self.previous.get() {
            for (identity, held) in previous.identity_counts(owner).await? {
                let count = counts.entry(identity).or_default();
                *count = (*count).max(held);
            }
        }
        Ok(counts)
    }

    async fn reassign_identity(&self, owner: &AgentId, from: &str, to: &str) -> Result<usize> {
        let objects: Vec<ObjectId> = self
            .every(about(owner, from))
            .await?
            .into_iter()
            .map(|object| object.id)
            .collect();
        let moved = self
            .patch_all(&objects, props([("identityId", json!(to))]))
            .await?;
        if let Some(previous) = self.previous.get() {
            previous.reassign_identity(owner, from, to).await?;
        }
        Ok(moved)
    }

    async fn purge_identity(&self, owner: &AgentId, identity: &str) -> Result<usize> {
        let purged = self
            .unless_unmade(async {
                Ok(usize::try_from(
                    self.delete_where(&about(owner, identity)).await?,
                )?)
            })
            .await?;
        // Unlike `forget`, a failure here fails the purge: erasure that left
        // a rollback copy behind would not be erasure.
        if let Some(previous) = self.previous.get() {
            previous.purge_identity(owner, identity).await?;
        }
        Ok(purged)
    }

    /// Returns how many of `ids` were held, however many objects each.
    async fn purge_memories(&self, ids: &[MemoryId]) -> Result<usize> {
        let purged = self
            .unless_unmade(async {
                let mut purged = 0;
                for chunk in ids.chunks(PURGE_CHUNK) {
                    let filter = Filter::Or(chunk.iter().map(by_memory_id).collect());
                    let held: HashSet<String> = self
                        .every(filter.clone())
                        .await?
                        .iter()
                        .filter_map(|object| {
                            Some(object.properties.get("memoryId")?.as_str()?.to_string())
                        })
                        .collect();
                    if !held.is_empty() {
                        self.delete_where(&filter).await?;
                    }
                    purged += held.len();
                }
                Ok(purged)
            })
            .await?;
        if let Some(previous) = self.previous.get() {
            previous.purge_memories(ids).await?;
        }
        Ok(purged)
    }

    /// One atomic step: the count goes up, the message ids not yet linked
    /// are appended, and the memory is marked recurred and accessed now.
    async fn record_recurrence(
        &self,
        id: &MemoryId,
        additional_message_ids: Vec<String>,
    ) -> Result<u32> {
        let object = self.object_of(id).await?.id;
        let now = json!(Utc::now().to_rfc3339());
        let values: Vec<String> = additional_message_ids
            .into_iter()
            .filter(|message| !message.is_empty())
            .collect();
        let increment = Change::Increment {
            collection: self.collection.clone(),
            id: object,
            property: "recurrenceCount".to_string(),
            by: 1.0,
        };
        let append = (!values.is_empty()).then(|| Change::Append {
            collection: self.collection.clone(),
            id: object,
            property: "sourceMessageIds".to_string(),
            values,
            unique: true,
        });
        let touched = self.patch_change(
            object,
            props([("lastRecurrenceAt", now.clone()), ("lastAccessed", now)]),
            None,
        );
        let written = self
            .answered(
                self.writes
                    .apply(
                        std::iter::once(increment)
                            .chain(append)
                            .chain([touched])
                            .collect(),
                    )
                    .await,
            )
            .await?;
        let recurrences = value_of(&written, 0)? as u32;
        debug!(memory_id = %id.0, recurrence_count = recurrences, "Recorded memory recurrence");
        Ok(recurrences)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::MemoryLayer;

    fn at(text: &str) -> DateTime<Utc> {
        text.parse().unwrap()
    }

    fn conditions(filter: &MemoryFilter) -> Vec<Filter> {
        match where_filter(filter) {
            Filter::And(conditions) => conditions,
            other => panic!("not a conjunction: {other:?}"),
        }
    }

    #[test]
    fn a_forgotten_memory_never_matches() {
        assert_eq!(
            conditions(&MemoryFilter::default()),
            vec![eq("deleted", false)]
        );
    }

    #[test]
    fn scalar_fields_become_exact_conditions() {
        let filter = MemoryFilter {
            layer: Some(MemoryLayer::Semantic),
            owner: Some(AgentId::new("atlas")),
            visibility: Some(Visibility::Team("health".into())),
            min_importance: Some(0.5),
            source: Some(crate::types::MemorySource::Human),
            created_after: Some(at("2026-01-01T00:00:00Z")),
            created_before: Some(at("2026-02-01T00:00:00Z")),
            ..Default::default()
        };
        assert_eq!(
            conditions(&filter),
            vec![
                eq("deleted", false),
                eq("layer", "semantic"),
                eq("owner", "atlas"),
                eq("visibility", "team:health"),
                Filter::Gte {
                    property: "importance".into(),
                    value: json!(0.5)
                },
                eq("source", "human"),
                gte("createdAt", &at("2026-01-01T00:00:00Z")),
                lte("createdAt", &at("2026-02-01T00:00:00Z")),
            ]
        );
    }

    #[test]
    fn a_least_importance_is_sent_as_the_decimal_it_was_written_as() {
        let filter = MemoryFilter {
            min_importance: Some(0.3),
            ..Default::default()
        };
        assert_eq!(
            conditions(&filter)[1],
            Filter::Gte {
                property: "importance".into(),
                value: json!(0.3)
            }
        );
    }

    #[test]
    fn an_identity_with_a_team_also_admits_the_teams_memories() {
        let own = MemoryFilter {
            identity_id: Some("alice".into()),
            ..Default::default()
        };
        assert_eq!(conditions(&own)[1], eq("identityId", "alice"));
        let team = MemoryFilter {
            team_id: Some("health".into()),
            ..own
        };
        assert_eq!(
            conditions(&team)[1],
            Filter::Or(vec![
                eq("identityId", "alice"),
                eq("visibility", "team:health")
            ])
        );
    }

    #[test]
    fn an_event_window_reads_the_event_time_and_falls_back_to_creation() {
        let before = at("2026-02-01T00:00:00Z");
        let filter = MemoryFilter {
            event_before: Some(before),
            ..Default::default()
        };
        assert_eq!(
            conditions(&filter)[1],
            Filter::Or(vec![
                Filter::And(vec![
                    Filter::Not(Box::new(is_null("eventTime"))),
                    lte("eventTime", &before),
                ]),
                Filter::And(vec![is_null("eventTime"), lte("createdAt", &before)]),
            ])
        );
        let after = MemoryFilter {
            event_after: Some(before),
            ..Default::default()
        };
        assert!(matches!(&conditions(&after)[1], Filter::Or(both) if both.len() == 2));
    }

    #[test]
    fn required_tags_are_any_of_and_excluded_tags_each_none_of() {
        let filter = MemoryFilter {
            tags: Some(vec!["pinned".into(), "date:2026-07-05".into()]),
            excluded_tags: vec!["kind:recap_artifact".into()],
            ..Default::default()
        };
        assert_eq!(
            conditions(&filter)[1..],
            [
                Filter::Or(vec![eq("tags", "pinned"), eq("tags", "date:2026-07-05")]),
                Filter::Ne {
                    property: "tags".into(),
                    value: json!("kind:recap_artifact")
                },
            ]
        );
        let empty = MemoryFilter {
            tags: Some(Vec::new()),
            ..Default::default()
        };
        assert_eq!(conditions(&empty), vec![eq("deleted", false)]);
    }

    #[test]
    fn a_collection_is_named_as_in_weaviate_or_refused() {
        let store = |name: &str| VellevStore::new("http://127.0.0.1:1", "key", "workforce", name);
        assert_eq!(
            store("WorkforceMemory_amazon_titan_embed_text_v2_0")
                .unwrap()
                .collection(),
            "WorkforceMemory_amazon_titan_embed_text_v2_0"
        );
        assert!(store("Memory-v2").is_err());
        assert!(store("1Memory").is_err());
    }

    #[tokio::test]
    async fn the_node_embeds_with_bge_unless_another_local_model_is_named() {
        let store =
            VellevStore::new("http://127.0.0.1:1", "key", "workforce", "WorkforceMemory").unwrap();
        assert_eq!(store.embedding_model(), Some(workforce_vellev::LOCAL_MODEL));
        assert_eq!(
            store.embedding().await.unwrap(),
            local_embedding(&["content"])
        );

        let minilm = store.with_local_model("Qdrant/all-MiniLM-L6-v2-onnx", 384, None);
        assert_eq!(
            minilm.embedding_model(),
            Some("Qdrant/all-MiniLM-L6-v2-onnx")
        );
        let embedding = minilm.embedding().await.unwrap();
        assert_eq!(embedding.provider, EmbeddingProvider::Local);
        assert_eq!(embedding.dimensions, 384);
        assert_eq!(embedding.query_prefix, None);
        assert_eq!(
            embedding.properties,
            local_embedding(&["content"]).properties
        );
    }

    #[test]
    fn an_export_reads_as_weaviates() {
        let shaped = weaviate_shaped(props([
            ("createdAt", json!("2026-07-05T09:30:00.120Z")),
            ("eventTime", json!("2026-07-05T09:30:00Z")),
            ("lastAccessed", json!("2026-07-05T10:30:00.123456789+01:00")),
            ("importance", json!(1.0)),
            ("accessCount", json!(3)),
            ("identityId", Value::Null),
            ("content", json!("2026-07-05T09:30:00.120Z")),
        ]));
        assert_eq!(
            Value::Object(shaped),
            json!({
                "createdAt": "2026-07-05T09:30:00.12Z",
                "eventTime": "2026-07-05T09:30:00Z",
                "lastAccessed": "2026-07-05T09:30:00.123456789Z",
                "importance": 1,
                "accessCount": 3,
                "content": "2026-07-05T09:30:00.120Z",
            })
        );
        let half = weaviate_shaped(props([("importance", json!(0.5))]));
        assert_eq!(half["importance"], json!(0.5));
    }
}

#[cfg(test)]
pub(crate) mod node_tests {
    //! Against a real vellev node: `VELLEV_TEST_URL` and `VELLEV_TEST_KEY`
    //! name it, and each test skips without them. Each test has a tenant
    //! of its own, dropped when it passes.
    use super::*;
    use crate::admin::{sync, SyncMode, SyncReport};
    use crate::types::{MemoryLayer, MemorySearchResult};
    pub(crate) use workforce_vellev::testing::Node;

    /// Vectors from the words of a text, each word hashed to a dimension,
    /// so that texts sharing words are near.
    pub(crate) struct Words;

    #[async_trait]
    impl EmbeddingBackend for Words {
        async fn embed(&self, text: &str) -> Result<Vec<f32>> {
            let mut vector = vec![0.0; 32];
            for word in text
                .split(|c: char| !c.is_alphanumeric())
                .filter(|w| !w.is_empty())
            {
                let hash = word
                    .to_lowercase()
                    .bytes()
                    .fold(0xcbf2_9ce4_8422_2325_u64, |h, b| {
                        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
                    });
                vector[(hash % 32) as usize] += 1.0;
            }
            Ok(vector)
        }

        fn model_id(&self) -> &str {
            "words"
        }
    }

    /// Memory stores in a [`Node`]'s tenant.
    pub(crate) trait Stores {
        /// A store of `collection` the node embeds for, not yet ensured.
        fn plain(&self, collection: &str) -> VellevStore;

        /// A store of `collection` holding [`Words`]' vectors, ensured.
        async fn store(&self, collection: &str) -> VellevStore;
    }

    impl Stores for Node {
        fn plain(&self, collection: &str) -> VellevStore {
            VellevStore::new(&self.url, &self.key, &self.tenant, collection).unwrap()
        }

        async fn store(&self, collection: &str) -> VellevStore {
            let store = self
                .plain(collection)
                .with_embeddings(Arc::new(Words), None, 0.75, 30);
            store.ensure_collection().await.unwrap();
            store
        }
    }

    fn alices(content: &str) -> Memory {
        Memory::new(content, MemoryLayer::Episodic, AgentId::new("agent")).with_identity_id("alice")
    }

    fn for_alice() -> MemoryFilter {
        MemoryFilter {
            identity_id: Some("alice".into()),
            limit: Some(10),
            ..Default::default()
        }
    }

    fn contents(results: &[MemorySearchResult]) -> Vec<&str> {
        results.iter().map(|r| r.memory.content.as_str()).collect()
    }

    /// A vector given is what the store recalls by, not one of its own
    /// making from the query (DW-676: one embedding serves the user's
    /// recall and the shadow's). The query here shares no word with either
    /// memory, so only the given vector can find the bananas.
    #[tokio::test]
    async fn a_recall_given_its_vector_recalls_by_it() {
        let Some(node) = Node::fresh().await else {
            return;
        };
        let store = node.store("Given").await;
        assert_eq!(store.query_model().as_deref(), Some("words"));
        for content in ["bananas are yellow", "the server listens on port 8080"] {
            store.remember(alices(content)).await.unwrap();
        }
        let bananas = Words.embed_query("bananas yellow").await.unwrap();
        let found = store
            .recall_with_vector("zzz", Some(bananas), for_alice())
            .await
            .unwrap();
        assert_eq!(found[0].memory.content, "bananas are yellow", "{found:?}");
        let server = Words.embed_query("server port").await.unwrap();
        let found = store
            .recall_with_vector("zzz", Some(server), for_alice())
            .await
            .unwrap();
        assert_eq!(
            found[0].memory.content, "the server listens on port 8080",
            "{found:?}"
        );
    }

    /// A collection not yet made holds nothing to erase: a purge of it
    /// erases nothing and succeeds, and leaves it unmade, as it would a
    /// shadow replica the reconcile has not seeded yet.
    #[tokio::test]
    async fn a_purge_of_a_collection_not_yet_made_erases_nothing_and_succeeds() {
        let Some(node) = Node::fresh().await else {
            return;
        };
        let store = node
            .plain("NotYetMade")
            .with_embeddings(Arc::new(Words), None, 0.75, 30);
        assert!(!store.exists().await.unwrap());
        assert_eq!(
            store
                .purge_identity(&AgentId::new("agent"), "alice")
                .await
                .unwrap(),
            0
        );
        assert_eq!(store.purge_memories(&[MemoryId::new()]).await.unwrap(), 0);
        assert!(!store.exists().await.unwrap());
    }

    /// A tenant gone, or never made, is not a collection not yet made: a
    /// purge there reached nothing, and fails, so that the erasure is
    /// retried rather than taken as done.
    #[tokio::test]
    async fn a_purge_in_a_tenant_that_is_not_there_fails() {
        let Some(node) = Node::fresh().await else {
            return;
        };
        let gone = format!("wf-test-{}", uuid::Uuid::new_v4().simple());
        let store = VellevStore::new(&node.url, &node.key, &gone, "Memories").unwrap();
        assert!(store
            .purge_identity(&AgentId::new("agent"), "alice")
            .await
            .is_err());
        assert!(store.purge_memories(&[MemoryId::new()]).await.is_err());
    }

    /// vellev not answering is not a collection not yet made: the purge
    /// fails.
    #[tokio::test]
    async fn a_purge_with_vellev_not_answering_fails() {
        let Some(node) = Node::fresh().await else {
            return;
        };
        let (down, _) = Node::not_up_yet().await;
        let store = VellevStore::new(&down, &node.key, &node.tenant, "Memories").unwrap();
        assert!(store
            .purge_identity(&AgentId::new("agent"), "alice")
            .await
            .is_err());
        assert!(store.purge_memories(&[MemoryId::new()]).await.is_err());
    }

    #[tokio::test]
    async fn the_identity_operations_contract_holds() {
        let Some(node) = Node::fresh().await else {
            return;
        };
        crate::store::identity_operations_contract(&node.store("Memories").await).await;
    }

    #[tokio::test]
    async fn a_memory_is_remembered_got_and_recalled_by_its_content() {
        let Some(node) = Node::fresh().await else {
            return;
        };
        let store = node.store("Memories").await;
        let banana = store
            .remember(alices("bananas are yellow and sweet"))
            .await
            .unwrap();
        store
            .remember(alices("the server listens on port 8080"))
            .await
            .unwrap();
        store
            .remember(
                Memory::new(
                    "bananas for bob",
                    MemoryLayer::Episodic,
                    AgentId::new("agent"),
                )
                .with_identity_id("bob"),
            )
            .await
            .unwrap();

        let got = store.get(&banana).await.unwrap().unwrap();
        assert_eq!(got.content, "bananas are yellow and sweet");
        assert_eq!(got.identity_id.as_deref(), Some("alice"));

        let hits = store.recall("yellow bananas", for_alice()).await.unwrap();
        assert_eq!(
            contents(&hits)[0],
            "bananas are yellow and sweet",
            "{hits:?}"
        );
        assert!(hits
            .iter()
            .all(|h| h.memory.identity_id.as_deref() == Some("alice")));
        assert!(hits[0].score > hits[1].score && hits[0].score <= 1.0);

        let strict = MemoryFilter {
            min_certainty: Some(0.75),
            ..for_alice()
        };
        let strong = store.recall("yellow bananas", strict).await.unwrap();
        assert_eq!(contents(&strong), ["bananas are yellow and sweet"]);
    }

    #[tokio::test]
    async fn recall_honours_required_and_excluded_tags_and_hides_pollution_by_default() {
        let Some(node) = Node::fresh().await else {
            return;
        };
        let store = node.store("Memories").await;
        for (content, tag) in [
            ("pinned note about tea", "pinned"),
            ("dated note about tea", "date:2026-07-05"),
            ("transposed note about tea", "date:2026-05-07"),
            ("recap note about tea", crate::audit::RECAP_ARTIFACT_KIND),
        ] {
            store
                .remember(alices(content).with_tags(vec![tag.to_string()]))
                .await
                .unwrap();
        }
        let recalled = |filter: MemoryFilter| {
            let store = &store;
            async move {
                let mut found: Vec<String> = store
                    .recall("note about tea", filter)
                    .await
                    .unwrap()
                    .into_iter()
                    .map(|r| r.memory.content)
                    .collect();
                found.sort();
                found
            }
        };
        assert_eq!(
            recalled(for_alice()).await,
            [
                "dated note about tea",
                "pinned note about tea",
                "transposed note about tea"
            ]
        );
        let dated = MemoryFilter {
            tags: Some(vec!["date:2026-07-05".into()]),
            ..for_alice()
        };
        assert_eq!(recalled(dated).await, ["dated note about tea"]);
        let unpinned = MemoryFilter {
            excluded_tags: vec!["pinned".into()],
            ..for_alice()
        };
        assert_eq!(
            recalled(unpinned).await,
            ["dated note about tea", "transposed note about tea"]
        );
        let forensic = MemoryFilter {
            include_polluted: true,
            ..for_alice()
        };
        assert_eq!(recalled(forensic).await.len(), 4);
    }

    #[tokio::test]
    async fn a_memory_whose_event_came_long_before_it_was_made_is_found_by_its_event() {
        let Some(node) = Node::fresh().await else {
            return;
        };
        let store = node.store("Memories").await;
        let mut backfilled = alices("backfilled note about january");
        backfilled.event_time = at("2026-01-10T00:00:00Z");
        backfilled.created_at = at("2026-06-01T00:00:00Z");
        let mut recent = alices("recent note about june");
        recent.event_time = at("2026-06-02T00:00:00Z");
        recent.created_at = at("2026-01-05T00:00:00Z");
        store.remember(backfilled).await.unwrap();
        store.remember(recent).await.unwrap();

        let january = MemoryFilter {
            event_before: Some(at("2026-02-01T00:00:00Z")),
            ..for_alice()
        };
        let listed = store.list(january.clone()).await.unwrap();
        assert_eq!(
            listed
                .iter()
                .map(|m| m.content.as_str())
                .collect::<Vec<_>>(),
            ["backfilled note about january"]
        );
        let recalled = store.recall("note", january.clone()).await.unwrap();
        assert_eq!(contents(&recalled), ["backfilled note about january"]);
        assert_eq!(store.count(january).await.unwrap(), 1);

        let june = MemoryFilter {
            event_after: Some(at("2026-05-01T00:00:00Z")),
            ..for_alice()
        };
        assert_eq!(
            store.list(june).await.unwrap()[0].content,
            "recent note about june"
        );
    }

    fn at(text: &str) -> DateTime<Utc> {
        text.parse().unwrap()
    }

    #[tokio::test]
    async fn a_count_agrees_with_the_list_it_counts() {
        let Some(node) = Node::fresh().await else {
            return;
        };
        let store = node.store("Memories").await;
        for (content, tags) in [
            ("one", vec![]),
            ("two", vec!["pinned".to_string()]),
            ("three", vec![crate::audit::RECAP_ARTIFACT_KIND.to_string()]),
        ] {
            store
                .remember(alices(content).with_tags(tags))
                .await
                .unwrap();
        }
        let forgotten = store.remember(alices("four")).await.unwrap();
        store.forget(&forgotten).await.unwrap();
        for filter in [
            for_alice(),
            MemoryFilter {
                include_polluted: true,
                ..for_alice()
            },
            MemoryFilter {
                tags: Some(vec!["pinned".into()]),
                ..for_alice()
            },
        ] {
            assert_eq!(
                store.count(filter.clone()).await.unwrap(),
                store.list(filter.clone()).await.unwrap().len(),
                "{filter:?}"
            );
        }
        assert_eq!(store.count(for_alice()).await.unwrap(), 2);
    }

    #[tokio::test]
    async fn importance_is_clamped_and_concurrent_adjustments_all_land() {
        let Some(node) = Node::fresh().await else {
            return;
        };
        let store = node.store("Memories").await;
        let id = store
            .remember(alices("a lesson").with_importance(0.5))
            .await
            .unwrap();
        let adjustments = (0..20).map(|_| store.adjust_importance(&id, 0.01));
        for adjusted in futures::future::join_all(adjustments).await {
            adjusted.unwrap();
        }
        let importance = store.get(&id).await.unwrap().unwrap().importance;
        assert!((importance - 0.7).abs() < 1e-4, "{importance}");

        assert_eq!(store.adjust_importance(&id, 5.0).await.unwrap(), 1.0);
        assert_eq!(store.get(&id).await.unwrap().unwrap().importance, 1.0);
        assert_eq!(
            store.adjust_importance(&id, -5.0).await.unwrap(),
            IMPORTANCE_FLOOR
        );
        assert!(store
            .adjust_importance(&MemoryId::new(), 0.1)
            .await
            .is_err());
    }

    #[tokio::test]
    async fn a_recurrence_counts_and_links_each_message_once() {
        let Some(node) = Node::fresh().await else {
            return;
        };
        let store = node.store("Memories").await;
        let mut memory = alices("a recurring thought");
        memory.source_message_ids = vec!["m1".into()];
        let id = store.remember(memory).await.unwrap();
        let once = store
            .record_recurrence(&id, vec!["m1".into(), "m2".into(), String::new()])
            .await
            .unwrap();
        let twice = store
            .record_recurrence(&id, vec!["m2".into(), "m3".into()])
            .await
            .unwrap();
        assert_eq!((once, twice), (1, 2));
        let held = store.get(&id).await.unwrap().unwrap();
        assert_eq!(held.source_message_ids, ["m1", "m2", "m3"]);
        assert_eq!(held.recurrence_count, 2);
        assert!(held.last_recurrence_at.is_some());
    }

    #[tokio::test]
    async fn a_forgotten_memory_is_not_got_and_a_shared_one_changes_visibility() {
        let Some(node) = Node::fresh().await else {
            return;
        };
        let store = node.store("Memories").await;
        let shared = store.remember(alices("for everyone")).await.unwrap();
        let forgotten = store.remember(alices("for nobody")).await.unwrap();
        store.share(&shared, Visibility::Global).await.unwrap();
        store.forget(&forgotten).await.unwrap();
        assert_eq!(
            store.get(&shared).await.unwrap().unwrap().visibility,
            Visibility::Global
        );
        assert!(store.get(&forgotten).await.unwrap().is_none());
        assert_eq!(
            store
                .identity_memories(&AgentId::new("agent"), "alice")
                .await
                .unwrap()
                .len(),
            2,
            "forgotten, not erased"
        );
    }

    #[tokio::test]
    async fn an_export_is_in_id_order_and_shaped_as_weaviates() {
        let Some(node) = Node::fresh().await else {
            return;
        };
        let store = node.store("Memories").await;
        for content in ["one", "two", "three"] {
            store
                .remember(alices(content).with_importance(1.0))
                .await
                .unwrap();
        }
        let page = store.export_page(None, true).await.unwrap();
        let ids: Vec<&str> = page.iter().map(|o| o["id"].as_str().unwrap()).collect();
        let mut sorted = ids.clone();
        sorted.sort();
        assert_eq!(ids, sorted);
        let first = &page[0];
        assert_eq!(first["class"], "Memories");
        assert_eq!(first["vector"].as_array().unwrap().len(), 32);
        let properties = first["properties"].as_object().unwrap();
        assert!(properties.values().all(|v| !v.is_null()));
        assert!(!properties.contains_key("sourceSessionId"));
        assert_eq!(properties["importance"], json!(1));
        assert!(properties["createdAt"].as_str().unwrap().ends_with('Z'));

        let rest = store.export_page(Some(ids[0]), false).await.unwrap();
        assert_eq!(rest.len(), 2);
        assert!(rest
            .iter()
            .all(|o| o.get("vector").is_none() && o["id"] != ids[0]));
    }

    /// A memory is stored under its own id: what a sync matches the two
    /// stores' memories by.
    #[tokio::test]
    async fn a_memory_is_stored_under_its_own_id() {
        let Some(node) = Node::fresh().await else {
            return;
        };
        let store = node.store("Memories").await;
        let id = store.remember(alices("likes tea")).await.unwrap();
        let name = CollectionName::new("Memories").unwrap();
        let held = node.tenant().collection(&name);
        let object = held.get(ObjectId(Uuid::parse_str(&id.0).unwrap())).await;
        let object = object.unwrap().expect("held under the memory's id");
        assert_eq!(object.properties["memoryId"], id.0.as_str());
        let exported = store.export(false).await.unwrap();
        assert_eq!(exported[0]["id"], id.0.as_str());
    }

    /// An import that cannot store one object stores the others, each on
    /// its own once their batch was refused, and fails, saying which it
    /// could not store and why.
    #[tokio::test]
    async fn an_import_stores_what_it_can_and_fails_naming_what_it_could_not() {
        let Some(node) = Node::fresh().await else {
            return;
        };
        let store = node.store("Memories").await;
        let mut bad = raw(2, json!({}));
        bad["vector"] = json!([1.0, 2.0, 3.0]);
        let objects = [raw(1, json!({})), bad.clone(), raw(3, json!({}))];
        let failed = store.import(&objects).await.unwrap_err();
        let short = failed
            .downcast_ref::<admin::ImportShort>()
            .expect("which, and why");
        assert_eq!(short.stored, 2);
        assert_eq!(short.failed.len(), 1, "{short}");
        assert_eq!(short.failed[0].0, bad["id"].as_str().unwrap());
        let held: Vec<Value> = store.export(false).await.unwrap();
        let ids: Vec<&str> = held.iter().map(|o| o["id"].as_str().unwrap()).collect();
        assert_eq!(
            ids,
            [
                objects[0]["id"].as_str().unwrap(),
                objects[2]["id"].as_str().unwrap()
            ]
        );
        // A sync that meets it fails, and the switch is not for recording.
        assert!(!store.switched().await.unwrap());
    }

    /// A sync's patch of a memory that is gone leaves it gone and goes on;
    /// and a property patched to nothing is taken off the memory.
    #[tokio::test]
    async fn a_patch_leaves_a_missing_memory_gone_and_takes_off_what_is_nulled() {
        let Some(node) = Node::fresh().await else {
            return;
        };
        let store = node.store("Memories").await;
        let gone = Uuid::new_v4().to_string();
        SyncTarget::patch(&store, &gone, json!({ "accessCount": 3 }))
            .await
            .expect("left gone");
        assert!(store.export(false).await.unwrap().is_empty());

        let id = store.remember(alices("likes tea")).await.unwrap();
        let before = store.export(false).await.unwrap();
        assert_eq!(before[0]["properties"]["identityId"], "alice");
        SyncTarget::patch(
            &store,
            &id.0,
            json!({ "identityId": null, "accessCount": 4 }),
        )
        .await
        .unwrap();
        let after = store.export(true).await.unwrap();
        assert!(
            after[0]["properties"].get("identityId").is_none(),
            "{after:?}"
        );
        assert_eq!(after[0]["properties"]["accessCount"], 4);
        assert!(after[0]["vector"].is_array(), "its vector kept");
    }

    /// A hub embedder whose queries all point one way, and whose memories
    /// point that way only if they are about zebras: so that what is near
    /// by meaning and what shares the query's words are different
    /// memories.
    struct Skewed;

    #[async_trait]
    impl EmbeddingBackend for Skewed {
        async fn embed(&self, text: &str) -> Result<Vec<f32>> {
            let mut vector = vec![0.0; 32];
            vector[usize::from(!text.contains("zebra"))] = 1.0;
            Ok(vector)
        }

        async fn embed_query(&self, _: &str) -> Result<Vec<f32>> {
            let mut vector = vec![0.0; 32];
            vector[0] = 1.0;
            Ok(vector)
        }

        fn model_id(&self) -> &str {
            "words"
        }
    }

    /// A reranker that keeps the order it is given and says how many
    /// texts it was given.
    #[derive(Default)]
    struct Counting(std::sync::Mutex<Vec<usize>>);

    #[async_trait]
    impl Reranker for Counting {
        async fn rerank(&self, _: &str, texts: &[String]) -> Result<Vec<(usize, f32)>> {
            self.0.lock().unwrap().push(texts.len());
            Ok((0..texts.len()).map(|at| (at, 0.9)).collect())
        }
    }

    /// Where the hub embeds, a recall's own hybrid alpha decides how far
    /// words and meaning each count, not the store's: by words alone the
    /// memory sharing the query's words leads, by meaning alone the one
    /// whose vector is the query's.
    #[tokio::test]
    async fn a_recall_is_weighted_by_its_own_alpha_where_the_hub_embeds() {
        let Some(node) = Node::fresh().await else {
            return;
        };
        let store = node
            .plain("Memories")
            .with_embeddings(Arc::new(Skewed), None, 0.5, 30);
        store.ensure_collection().await.unwrap();
        store.remember(alices("a zebra has stripes")).await.unwrap();
        store
            .remember(alices("the quarterly report is due"))
            .await
            .unwrap();
        let first = |alpha: f32| {
            let store = &store;
            async move {
                let filter = MemoryFilter {
                    hybrid_alpha: Some(alpha),
                    ..for_alice()
                };
                let found = store.recall("quarterly report", filter).await.unwrap();
                found[0].memory.content.clone()
            }
        };
        assert_eq!(first(0.0).await, "the quarterly report is due");
        assert_eq!(first(1.0).await, "a zebra has stripes");
    }

    /// A reranker is given the store's number of candidates to choose
    /// among, however few results were asked for.
    #[tokio::test]
    async fn a_reranker_chooses_among_more_candidates_than_are_asked_for() {
        let Some(node) = Node::fresh().await else {
            return;
        };
        let counting = Arc::new(Counting::default());
        let store = node.plain("Memories").with_embeddings(
            Arc::new(Words),
            Some(counting.clone() as Arc<dyn Reranker>),
            0.75,
            4,
        );
        store.ensure_collection().await.unwrap();
        for n in 0..6 {
            let content = format!("note {n} about the release");
            store.remember(alices(&content)).await.unwrap();
        }
        let filter = MemoryFilter {
            limit: Some(1),
            ..for_alice()
        };
        let found = store.recall("the release", filter).await.unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(*counting.0.lock().unwrap(), [4]);
    }

    #[tokio::test]
    async fn a_mirror_between_two_vellev_stores_copies_once_and_then_finds_nothing_to_do() {
        let Some(node) = Node::fresh().await else {
            return;
        };
        let source = node.store("Source").await;
        let target = node.store("Target").await;
        let mut ids = Vec::new();
        for content in ["one", "two", "three"] {
            ids.push(source.remember(alices(content)).await.unwrap());
        }
        source.forget(&ids[1]).await.unwrap();
        source.adjust_importance(&ids[0], 0.25).await.unwrap();

        let first = sync(&target, &source, SyncMode::Mirror).await.unwrap();
        assert_eq!(
            first,
            SyncReport {
                copied: 3,
                ..Default::default()
            }
        );
        let again = sync(&target, &source, SyncMode::Mirror).await.unwrap();
        assert_eq!(
            again,
            SyncReport {
                unchanged: 3,
                ..Default::default()
            }
        );
        assert_eq!(
            target.export(true).await.unwrap(),
            source
                .export(true)
                .await
                .unwrap()
                .into_iter()
                .map(|mut o| {
                    o["class"] = json!("Target");
                    o
                })
                .collect::<Vec<_>>(),
            "the target holds the source's memories, vectors and all"
        );
        assert!(target.get(&ids[1]).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn the_switch_is_marked_once_per_collection() {
        let Some(node) = Node::fresh().await else {
            return;
        };
        let store = node.store("Memories").await;
        let other = node.store("Others").await;
        assert!(!store.switched().await.unwrap());
        store.mark_switched().await.unwrap();
        store.mark_switched().await.unwrap();
        assert!(store.switched().await.unwrap());
        assert!(!other.switched().await.unwrap());
    }

    fn unscoped(limit: usize) -> MemoryFilter {
        MemoryFilter {
            system_unscoped: true,
            limit: Some(limit),
            ..Default::default()
        }
    }

    /// A raw object as Weaviate exports a memory, with a vector.
    fn raw(n: usize, properties: Value) -> Value {
        let mut object = json!({
            "id": Uuid::from_u128(n as u128 + 1).to_string(),
            "properties": {
                "content": format!("note {n}"),
                "memoryId": Uuid::from_u128(n as u128 + 1).to_string(),
                "owner": "agent",
                "deleted": false,
            },
            "vector": vec![1.0f32; 32],
        });
        for (name, value) in properties.as_object().into_iter().flatten() {
            object["properties"][name] = value.clone();
        }
        object
    }

    /// A key the node does not accept, or a tenant gone while the hub runs,
    /// is named as such on an ordinary request, not left as a bare status.
    #[tokio::test]
    async fn a_refused_key_or_a_tenant_gone_is_named_on_any_request() {
        let Some(node) = Node::fresh().await else {
            return;
        };
        let wrong_key =
            VellevStore::new(&node.url, "no-such-key", &node.tenant, "Memories").unwrap();
        let refused = wrong_key
            .get(&MemoryId::new())
            .await
            .unwrap_err()
            .to_string();
        assert!(
            refused.contains("does not accept the key for tenant"),
            "{refused}"
        );

        let store = node.store("Memories").await;
        node.tenant().drop().await.unwrap();
        let gone = store.get(&MemoryId::new()).await.unwrap_err();
        assert!(
            matches!(
                gone.downcast_ref::<workforce_vellev::Unusable>(),
                Some(workforce_vellev::Unusable::NoTenant { .. })
            ),
            "{gone}"
        );
    }

    /// A tagless memory as Weaviate 1.28.4, which driftwerk runs, exports it
    /// (empty arrays kept, dates in Go's form, whole importance as an
    /// integer) compares equal once copied into vellev and exported again.
    #[tokio::test]
    async fn a_tagless_memory_exported_by_weaviate_1_28_compares_equal_from_vellev() {
        let Some(node) = Node::fresh().await else {
            return;
        };
        let store = node.store("Memories").await;
        let exported: Value =
            serde_json::from_str(include_str!("fixtures/weaviate-1.28.4-tagless-memory.json"))
                .unwrap();
        assert_eq!(
            store.import(std::slice::from_ref(&exported)).await.unwrap(),
            1
        );
        let again = store.export(false).await.unwrap();
        assert_eq!(again.len(), 1);
        assert!(
            admin::same_properties(&again[0]["properties"], &exported["properties"]),
            "{} != {}",
            again[0]["properties"],
            exported["properties"]
        );
        assert_eq!(again[0]["properties"]["tags"], json!([]));
    }

    /// A memory of importance exactly 0.3, as Weaviate or an import may
    /// hold it, passes a least importance of 0.3.
    #[tokio::test]
    async fn a_memory_at_the_least_importance_asked_for_is_listed() {
        let Some(node) = Node::fresh().await else {
            return;
        };
        let store = node.store("Memories").await;
        store
            .import(&[raw(3, json!({ "importance": 0.3 }))])
            .await
            .unwrap();
        let at_least = MemoryFilter {
            min_importance: Some(0.3),
            ..unscoped(10)
        };
        assert_eq!(store.list(at_least).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_list_past_what_one_request_gives_is_read_a_page_at_a_time() {
        let Some(node) = Node::fresh().await else {
            return;
        };
        let store = node.store("Memories").await;
        let objects: Vec<Value> = (0..1200).map(|n| raw(n, json!({}))).collect();
        assert_eq!(store.import(&objects).await.unwrap(), 1200);
        assert_eq!(store.list(unscoped(1500)).await.unwrap().len(), 1200);
        assert_eq!(store.list(unscoped(1100)).await.unwrap().len(), 1100);
        let recalled = store.recall("note", unscoped(1500)).await.unwrap();
        assert_eq!(
            recalled.len(),
            MAX_RESULTS,
            "a search is cut to what one request gives"
        );
    }

    #[tokio::test]
    async fn concurrent_adjustments_across_a_bound_stop_at_it() {
        let Some(node) = Node::fresh().await else {
            return;
        };
        let store = node.store("Memories").await;
        let high = store
            .remember(alices("near the top").with_importance(0.98))
            .await
            .unwrap();
        let low = store
            .remember(alices("near the bottom").with_importance(0.5))
            .await
            .unwrap();
        let ups = (0..10).map(|_| store.adjust_importance(&high, 0.01));
        let downs = (0..10).map(|_| store.adjust_importance(&low, -0.1));
        let (ups, downs) = futures::future::join(
            futures::future::join_all(ups),
            futures::future::join_all(downs),
        )
        .await;
        for adjusted in ups.into_iter().chain(downs) {
            let adjusted = adjusted.unwrap();
            assert!((IMPORTANCE_FLOOR..=1.0).contains(&adjusted), "{adjusted}");
        }
        assert_eq!(store.get(&high).await.unwrap().unwrap().importance, 1.0);
        assert_eq!(
            store.get(&low).await.unwrap().unwrap().importance,
            IMPORTANCE_FLOOR
        );
    }

    #[tokio::test]
    async fn a_legacy_memory_without_an_importance_adjusts_from_a_half() {
        let Some(node) = Node::fresh().await else {
            return;
        };
        let store = node.store("Memories").await;
        let legacy = raw(7, json!({}));
        store.import(std::slice::from_ref(&legacy)).await.unwrap();
        let id = MemoryId(legacy["properties"]["memoryId"].as_str().unwrap().into());
        let adjusted = store.adjust_importance(&id, 0.1).await.unwrap();
        assert!((adjusted - 0.6).abs() < 1e-6, "{adjusted}");
        let stored = store.get(&id).await.unwrap().unwrap().importance;
        assert!((stored - 0.6).abs() < 1e-6, "{stored}");
    }

    /// The hub's vectors of another size.
    struct Narrow;

    #[async_trait]
    impl EmbeddingBackend for Narrow {
        async fn embed(&self, _: &str) -> Result<Vec<f32>> {
            Ok(vec![1.0; 16])
        }

        fn model_id(&self) -> &str {
            "words"
        }
    }

    #[tokio::test]
    async fn a_collection_of_another_size_or_one_that_embeds_for_itself_is_refused() {
        let Some(node) = Node::fresh().await else {
            return;
        };
        node.store("Memories").await;
        let narrow = node
            .plain("Memories")
            .with_embeddings(Arc::new(Narrow), None, 0.75, 30);
        let refused = narrow.ensure_collection().await.unwrap_err().to_string();
        assert!(refused.contains("32 dimensions, expected 16"), "{refused}");
        assert!(narrow.doctor().await.is_err());

        node.plain("Local").ensure_collection().await.unwrap();
        let hubs = node
            .plain("Local")
            .with_embeddings(Arc::new(Words), None, 0.75, 30);
        let refused = hubs.ensure_collection().await.unwrap_err().to_string();
        assert!(refused.contains("embeds for itself"), "{refused}");
    }

    #[tokio::test]
    async fn the_switch_marker_of_a_collection_imported_from_weaviate_is_found() {
        let Some(node) = Node::fresh().await else {
            return;
        };
        let store = node.store("Memories").await;
        let markers = CollectionName::new(workforce_vellev::MARKERS).unwrap();
        let imported = collection(
            markers.clone(),
            vec![
                Property::new("collection", DataType::Text).searchable(),
                Property::new("switchedAt", DataType::Date),
            ],
            Embedding {
                provider: EmbeddingProvider::None,
                model: "weaviate:none".into(),
                dimensions: 1,
                query_prefix: None,
                document_prefix: None,
                properties: Vec::new(),
            },
        );
        let switches = node.tenant().collection(&markers);
        switches.create(&imported).await.unwrap();
        assert!(!store.switched().await.unwrap());
        switches
            .put(
                ObjectId(workforce_vellev::marker_id("Memories")),
                &vellev_client::api::Content {
                    properties: props([
                        ("collection", json!("Memories")),
                        ("switchedAt", json!("2026-07-05T09:30:00.1Z")),
                    ]),
                    vector: None,
                },
            )
            .await
            .unwrap();
        assert!(store.switched().await.unwrap());
        store.mark_switched().await.unwrap();
    }

    /// A memory collection vellev's importer copied from Weaviate with its
    /// text2vec-transformers vectors is refused, saying to copy it again
    /// for the node to embed rather than to drop it: memories have no
    /// source to be rebuilt from.
    #[tokio::test]
    async fn imported_memories_the_node_cannot_embed_for_are_refused_without_being_dropped() {
        let Some(node) = Node::fresh().await else {
            return;
        };
        let store = node.plain("WorkforceMemory");
        let tenant = node.tenant();
        let imported = collection(
            CollectionName::new("WorkforceMemory").unwrap(),
            memory_properties(),
            Embedding {
                provider: EmbeddingProvider::None,
                model: "weaviate:text2vec-transformers".into(),
                dimensions: 384,
                query_prefix: None,
                document_prefix: None,
                properties: Vec::new(),
            },
        );
        tenant
            .collection(&imported.name)
            .create(&imported)
            .await
            .unwrap();
        for refused in [
            store.ensure_collection().await.unwrap_err(),
            store.doctor().await.unwrap_err(),
        ] {
            let refused = refused.to_string();
            assert!(refused.contains(REIMPORT), "{refused}");
            assert!(!refused.contains("index again"), "{refused}");
        }
        assert!(
            tenant
                .collection(&imported.name)
                .schema()
                .await
                .unwrap()
                .is_some(),
            "left as it is"
        );
    }

    /// Needs a node that runs the local model: `VELLEV_TEST_LOCAL_EMBED=1`.
    #[tokio::test]
    async fn the_node_embeds_and_recalls_by_meaning_or_by_hybrid() {
        if std::env::var("VELLEV_TEST_LOCAL_EMBED").as_deref() != Ok("1") {
            eprintln!("skipping: VELLEV_TEST_LOCAL_EMBED=1 names no node with a local model");
            return;
        }
        let Some(node) = Node::fresh().await else {
            return;
        };
        let store = node.plain("Memories");
        store.ensure_collection().await.unwrap();
        for content in [
            "bananas are yellow and sweet",
            "the server listens on port 8080",
            "kiwis are green and tart",
        ] {
            store.remember(alices(content)).await.unwrap();
        }
        let near = store
            .recall("which fruit is yellow?", for_alice())
            .await
            .unwrap();
        assert_eq!(
            near[0].memory.content, "bananas are yellow and sweet",
            "{near:?}"
        );
        assert!(near.iter().all(|r| (0.0..=1.0).contains(&r.score)));
        assert!(near.windows(2).all(|w| w[0].score >= w[1].score));
        let strict = MemoryFilter {
            min_certainty: Some(near[0].score - 1e-4),
            ..for_alice()
        };
        assert_eq!(
            contents(
                &store
                    .recall("which fruit is yellow?", strict)
                    .await
                    .unwrap()
            ),
            ["bananas are yellow and sweet"]
        );
        let hybrid = MemoryFilter {
            hybrid_alpha: Some(0.5),
            ..for_alice()
        };
        let fused = store.recall("yellow bananas", hybrid).await.unwrap();
        assert_eq!(fused[0].memory.content, "bananas are yellow and sweet");
        store.doctor().await.unwrap();
    }
}

#[cfg(test)]
mod failure_tests {
    //! Against a fake node that answers every write, or everything, with
    //! one failure.
    use std::sync::atomic::{AtomicUsize, Ordering};

    use axum::http::StatusCode;
    use axum::response::IntoResponse;

    use super::*;

    const ID: &str = "00000000-0000-0000-0000-000000000001";

    struct Fake {
        /// What every write is answered with; with `everything`, every
        /// request.
        failure: (StatusCode, &'static str),
        everything: bool,
        /// When set, every write succeeds after this long instead.
        slow_writes: Option<Duration>,
        writes: AtomicUsize,
        requests: AtomicUsize,
    }

    async fn answer(
        axum::extract::State(fake): axum::extract::State<Arc<Fake>>,
        uri: axum::http::Uri,
    ) -> axum::response::Response {
        fake.requests.fetch_add(1, Ordering::SeqCst);
        let failed = || {
            let (status, code) = fake.failure;
            (
                status,
                [("retry-after", "0")],
                axum::Json(json!({ "error": { "code": code, "message": "fake" } })),
            )
                .into_response()
        };
        let object = json!({ "id": ID, "properties": { "memoryId": "m", "importance": 0.5 } });
        if fake.everything {
            return failed();
        }
        match uri.path() {
            path if path.ends_with("/changes") => {
                fake.writes.fetch_add(1, Ordering::SeqCst);
                match fake.slow_writes {
                    Some(delay) => {
                        tokio::time::sleep(delay).await;
                        axum::Json(json!({
                            "incarnation": 1,
                            "position": 1,
                            "outcomes": [{ "outcome": "value", "value": 1 }]
                        }))
                        .into_response()
                    }
                    None => failed(),
                }
            }
            path if path.ends_with("/list") => {
                axum::Json(json!({ "objects": [object] })).into_response()
            }
            _ => axum::Json(object).into_response(),
        }
    }

    async fn store(
        failure: (StatusCode, &'static str),
        everything: bool,
    ) -> (VellevStore, Arc<Fake>) {
        serving(
            Fake {
                failure,
                everything,
                slow_writes: None,
                writes: AtomicUsize::new(0),
                requests: AtomicUsize::new(0),
            },
            Timeouts::default(),
        )
        .await
    }

    async fn serving(fake: Fake, timeouts: Timeouts) -> (VellevStore, Arc<Fake>) {
        let fake = Arc::new(fake);
        let app = axum::Router::new()
            .fallback(answer)
            .with_state(Arc::clone(&fake));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (
            VellevStore::new_with_timeouts(&url, "key", "workforce", "Memories", timeouts).unwrap(),
            fake,
        )
    }

    /// Writes the node embeds, which may take longer than a read is given,
    /// are given the longer timeout.
    #[tokio::test]
    async fn writes_the_node_embeds_wait_longer_than_reads() {
        let (store, _) = serving(
            Fake {
                failure: (StatusCode::OK, ""),
                everything: false,
                slow_writes: Some(Duration::from_millis(600)),
                writes: AtomicUsize::new(0),
                requests: AtomicUsize::new(0),
            },
            Timeouts {
                read: Duration::from_millis(200),
                write: Duration::from_secs(3),
                ..Timeouts::default()
            },
        )
        .await;
        let memory = Memory::new(
            "a note",
            crate::types::MemoryLayer::Episodic,
            AgentId::new("a"),
        );
        store.remember(memory).await.expect("remember waits");
        store
            .adjust_importance(&MemoryId("m".into()), 0.1)
            .await
            .expect("adjusting importance waits");
        store
            .record_recurrence(&MemoryId("m".into()), vec!["m1".into()])
            .await
            .expect("recording a recurrence waits");
    }

    /// A node that cannot embed memories fails readiness, naming the model.
    #[tokio::test]
    async fn a_node_that_cannot_run_the_model_fails_readiness_saying_so() {
        let app = axum::Router::new().fallback(
            |method: axum::http::Method, uri: axum::http::Uri| async move {
                let path = uri.path();
                if path.ends_with("/collections") {
                    return axum::Json(json!({ "collections": [] })).into_response();
                }
                if path.ends_with("/nearest") {
                    let refused = json!({ "error": {
                        "code": "unavailable",
                        "message": "the collection's embedding model could not be used just now"
                    } });
                    return (StatusCode::SERVICE_UNAVAILABLE, axum::Json(refused)).into_response();
                }
                if method == axum::http::Method::GET {
                    let missing = json!({ "error": { "code": "not_found", "message": "collection Memories does not exist" } });
                    return (StatusCode::NOT_FOUND, axum::Json(missing)).into_response();
                }
                axum::Json(json!({ "incarnation": 1, "position": 1, "outcomes": [{ "outcome": "applied" }] }))
                    .into_response()
            },
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let timeouts = Timeouts {
            read: Duration::from_millis(500),
            write: Duration::from_millis(500),
            ..Timeouts::default()
        };
        let store =
            VellevStore::new_with_timeouts(&url, "key", "workforce", "Memories", timeouts).unwrap();
        let refused = store.ensure_collection().await.unwrap_err();
        assert!(
            refused
                .downcast_ref::<workforce_vellev::ModelUnavailable>()
                .is_some(),
            "{refused}"
        );
        assert!(
            refused.to_string().contains(workforce_vellev::LOCAL_MODEL),
            "{refused}"
        );
    }

    #[tokio::test]
    async fn an_importance_that_keeps_changing_is_left_unchanged_after_its_tries() {
        let (mut store, fake) = store((StatusCode::CONFLICT, "conflict"), false).await;
        store.retry.ceiling = Duration::from_millis(1);
        let refused = store
            .adjust_importance(&MemoryId("m".into()), 0.1)
            .await
            .unwrap_err();
        assert!(refused.to_string().contains("left unchanged"), "{refused}");
        assert_eq!(
            fake.writes.load(Ordering::SeqCst),
            Retry::DEFAULT.tries as usize,
            "each try is one conditional write, refused"
        );
    }

    #[tokio::test]
    async fn a_node_that_is_unavailable_or_limiting_fails_after_the_clients_tries() {
        for failure in [
            (StatusCode::SERVICE_UNAVAILABLE, "unavailable"),
            (StatusCode::TOO_MANY_REQUESTS, "limited"),
        ] {
            let (store, fake) = store(failure, true).await;
            assert!(store.get(&MemoryId("m".into())).await.is_err());
            assert_eq!(
                fake.requests.load(Ordering::SeqCst),
                vellev_client::Options::default().attempts as usize
            );
            let memory = Memory::new(
                "a note",
                crate::types::MemoryLayer::Episodic,
                AgentId::new("a"),
            );
            assert!(store.remember(memory).await.is_err());
        }
    }
}

#[cfg(test)]
mod index_time_tests {
    //! A shadow's recall times the node's search as its index call
    //! (DW-676, PR 9b): each way a recall searches, against a fake node
    //! whose every search takes [`SEARCH`].
    use std::sync::Arc;

    use workforce_common::shadow::{
        MemorySink, Observed, Ranked, Shadow, ShadowSettings, ShadowSink,
    };

    use super::*;
    use crate::types::MemoryFilter;

    const SEARCH: Duration = Duration::from_millis(60);

    /// A node whose searches answer, with nothing found, after [`SEARCH`].
    async fn node() -> String {
        let app = axum::Router::new().fallback(|uri: axum::http::Uri| async move {
            let path = uri.path();
            if ["/nearest", "/hybrid", "/keyword"]
                .iter()
                .any(|search| path.ends_with(search))
            {
                tokio::time::sleep(SEARCH).await;
                return axum::Json(json!({ "results": [] }));
            }
            panic!("a recall asked the node for {path}");
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        url
    }

    struct Embeds(bool);

    #[async_trait]
    impl EmbeddingBackend for Embeds {
        async fn embed(&self, _text: &str) -> Result<Vec<f32>> {
            if self.0 {
                Ok(vec![1.0, 0.0, 0.0, 0.0])
            } else {
                Err(eyre!("the embedder is down"))
            }
        }
    }

    /// `recall` run as a shadow; its comparison.
    async fn shadowed(
        store: VellevStore,
        alpha: Option<f32>,
    ) -> workforce_common::shadow::Comparison {
        let sink = Arc::new(MemorySink::default());
        let settings = ShadowSettings {
            sample: 1.0,
            permits: 1,
            deadline: Duration::from_secs(10),
            trip_after: 3,
            rest: Duration::from_secs(60),
            k: 10,
            rbo_p: 0.9,
        };
        let shadow = Shadow::new(
            "memories",
            settings,
            Arc::clone(&sink) as Arc<dyn ShadowSink>,
        )
        .unwrap();
        let filter = MemoryFilter {
            identity_id: Some("bob".into()),
            hybrid_alpha: alpha,
            ..MemoryFilter::default()
        };
        shadow.observe(Observed::default(), move || async move {
            store.recall("otters asleep", filter).await?;
            Ok(Ranked::default())
        });
        let began = std::time::Instant::now();
        while sink.is_empty() {
            assert!(
                began.elapsed() < Duration::from_secs(10),
                "the shadow ended"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        sink.taken().remove(0)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_shadows_recall_times_each_kind_of_search_as_its_index_call() {
        let url = node().await;
        let plain = || VellevStore::new(&url, "key", "workforce", "Memories").unwrap();
        let embedded =
            |works: bool| plain().with_embeddings(Arc::new(Embeds(works)), None, 0.5, 10);
        for (how, store, alpha) in [
            ("nearest, the node embedding", plain(), None),
            ("hybrid, the node embedding", plain(), Some(0.5)),
            ("hybrid, the hub embedding", embedded(true), None),
            ("by keywords, the embedder down", embedded(false), None),
        ] {
            let c = shadowed(store, alpha).await;
            assert!(
                matches!(
                    c.outcome,
                    workforce_common::shadow::Outcome::Answered { .. }
                ),
                "{how}: {:?}",
                c.outcome
            );
            let index = c.index.unwrap_or_else(|| panic!("{how}: no index time"));
            assert!(index >= SEARCH, "{how}: {index:?}");
            assert!(
                index <= c.shadow_latency,
                "{how}: {index:?} of {:?}",
                c.shadow_latency
            );
        }
    }
}
