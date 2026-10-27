use std::path::{Path, PathBuf};

use async_trait::async_trait;
use eyre::Result;
use futures::StreamExt;
use reqwest::Client;
use thiserror::Error;
use tracing::{debug, error, info, warn};

use workforce_common::vectorizable::VectorizableText;

use crate::admin::{
    switch_marker_id, MemoryAdmin, Switchable, SyncSource, SyncTarget, EXPORT_PAGE,
    IMPORT_BATCH_SIZE, SWITCH_COLLECTION,
};
pub use crate::admin::{RecoveryOutcome, SyncMode, SyncReport};
use crate::shape::{self, decode_memory, layer_to_string, visibility_to_string};
use crate::store::{apply_pollution_default, post_filtered, MemoryStore};
use crate::types::{Memory, MemoryFilter, MemoryId, MemorySearchResult, Visibility};
use workforce_core::types::AgentId;

const COLLECTION_NAME: &str = "WorkforceMemory";

/// Objects per read of one identity's memories.
const IDENTITY_PAGE: usize = 500;
/// Objects per read of the objects holding one memory.
const MEMORY_OBJECTS_PAGE: usize = 100;
/// Objects read by id at once.
const OBJECT_READS: usize = 8;

/// Objects per step of a walk over the whole collection.
const SCAN_PAGE: usize = 500;

/// Objects per batch when vectors are absent and Weaviate must embed each
/// object inside the request — small enough that a batch finishes within the
/// client's 30s timeout even on a cold text2vec backend.
const IMPORT_BATCH_SIZE_REVECTORIZE: usize = 20;

/// Hard schema-shape error: a pre-existing Weaviate class has wrong
/// `dataType` or `tokenization` on one or more properties. Returned by
/// `validate_schema_shape` so callers can downcast off the eyre Report and
/// trigger an automatic rebuild instead of falling back to SQLite.
#[derive(Debug, Error)]
#[error(
    "Weaviate class '{class}' has {} schema mismatch(es): {}",
    mismatches.len(),
    mismatches.join("; "),
)]
pub struct SchemaMismatchError {
    pub class: String,
    pub mismatches: Vec<String>,
}

/// The shape `ensure_collection` guarantees. If a pre-existing class on the
/// Weaviate server diverges from this — for example because Weaviate's
/// auto-schema created it from the first write instead of our explicit
/// definition — `ensure_collection` will refuse to use it rather than
/// silently writing into a structurally-wrong class.
///
/// `(property_name, expected_dataType, expected_tokenization)`. Tokenization
/// is `Some` only for fields that must not be word-split.
const EXPECTED_PROPERTIES: &[(&str, &str, Option<&str>)] = &[
    ("content", "text", None),
    ("memoryId", "text", Some("field")),
    ("layer", "text", Some("field")),
    ("owner", "text", Some("field")),
    ("visibility", "text", Some("field")),
    ("importance", "number", None),
    // Created as `field`-tokenized since 2026-07 (exact whole-tag matching);
    // deliberately NOT validated here: flagging legacy word-tokenized
    // collections would force a boot-time rebuild, which under start-first
    // deploys deletes/re-imports the collection while the previous hub is
    // still serving it. Exactness on legacy collections is enforced in Rust
    // via `passes_required_tags`; upgrade offline with
    // `workforce memory rebuild-schema --yes`.
    ("tags", "text[]", None),
    ("deleted", "boolean", None),
    ("accessCount", "int", None),
    ("createdAt", "date", None),
    ("lastAccessed", "date", None),
    ("source", "text", Some("field")),
    ("identityId", "text", Some("field")),
    ("sourceSessionId", "text", Some("field")),
    // Same story as `tags`: `field`-tokenized at creation, not validated.
    ("sourceMessageIds", "text[]", None),
    ("recurrenceCount", "int", None),
    ("lastRecurrenceAt", "date", None),
    ("eventTime", "date", None),
];

/// Weaviate-backed memory store with real vector search.
pub struct WeaviateStore {
    client: Client,
    base_url: String,
    /// The collection memories live in: `WorkforceMemory` with Weaviate's
    /// vectorizer, or the hub-vectored collection when `embedder` is set.
    collection: String,
    /// When set, the hub embeds content and queries; the collection holds
    /// these vectors and has no vectorizer of its own.
    embedder: Option<std::sync::Arc<dyn crate::embedding::EmbeddingBackend>>,
    reranker: Option<std::sync::Arc<dyn crate::embedding::Reranker>>,
    /// Keyword weight in hybrid recall (0 = keywords, 1 = vectors).
    alpha: f32,
    rerank_candidates: usize,
    /// The collection this one replaced, kept for rollback: forgetting a
    /// memory here forgets it there too, so rolling back never brings a
    /// forgotten memory back.
    previous: std::sync::OnceLock<std::sync::Arc<dyn MemoryStore>>,
    /// Maps the hub's certainty onto the scale thresholds are tuned on.
    score_map: Vec<(f32, f32)>,
}

/// Where a recall's query vector comes from.
enum QueryVector {
    /// Made by the store's embedder.
    Embed,
    /// Given (`None`: it could not be made: by keywords alone).
    Given(Option<Vec<f32>>),
}

impl WeaviateStore {
    /// [`MemoryStore::recall`], the query's vector made here or given.
    async fn recall_given(
        &self,
        query: &str,
        mut filter: MemoryFilter,
        vector: QueryVector,
    ) -> Result<Vec<MemorySearchResult>> {
        let collection = self.collection.as_str();
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
        if !crate::store::check_and_audit_scope(
            &filter,
            crate::access_audit::AuditOperation::Recall,
        ) {
            return Ok(Vec::new());
        }
        apply_pollution_default(&mut filter);
        let limit = filter.limit.unwrap_or(10);
        // Excluded tags are pushed into the Weaviate `where` clause (see
        // build_where_filter), so nearText already returns only allowed
        // memories — no over-fetch buffer needed for THOSE. Positive tags do
        // need one on legacy word-tokenized collections (`fetch_limit_for`);
        // the result set is truncated back to `limit` after post-filtering.
        let mut fetch_limit = fetch_limit_for(&filter, limit);
        let where_filter = Self::build_where_filter(&filter);
        let mut query_vector: Option<Vec<f32>> = None;
        if let Some(embedder) = &self.embedder {
            match vector {
                QueryVector::Given(given) => query_vector = given,
                QueryVector::Embed => match embedder.embed_query(query.as_str()).await {
                    Ok(v) => query_vector = Some(v),
                    Err(e) => warn!(
                        target: "embeddings",
                        error = %e,
                        "query embedding failed; recalling by keywords only"
                    ),
                },
            }
            if self.reranker.is_some() {
                fetch_limit = fetch_limit.max(self.rerank_candidates);
            }
        }
        // Hybrid (BM25 + vector) when an alpha is supplied; otherwise the legacy
        // pure-vector `nearText`. Hybrid exposes relevance via `_additional.score`
        // (a string under relative-score fusion, ~[0,1]) and leaves `certainty`
        // null, so the two paths read different fields when decoding below.
        let escaped_query = graphql_escape(query.as_str());
        let hybrid = filter.hybrid_alpha;
        let embedded = self.embedder.is_some();
        let (search_clause, additional_field) = if embedded {
            // Hub-vectored: hybrid orders the candidates; the score each
            // result carries is computed below from the stored vector, so it
            // stays on an absolute scale the inject thresholds can read.
            // Keywords match `content` only: matching word-tokenised tags is
            // what cross-matched transposed dates.
            match &query_vector {
                Some(v) => (
                    format!(
                        r#"hybrid: {{ query: "{escaped_query}", vector: {}, alpha: {}, properties: ["content"] }}"#,
                        serde_json::to_string(v).unwrap_or_default(),
                        hybrid.unwrap_or(self.alpha)
                    ),
                    "vector",
                ),
                None => (
                    format!(r#"bm25: {{ query: "{escaped_query}", properties: ["content"] }}"#),
                    "score",
                ),
            }
        } else if let Some(alpha) = hybrid {
            (
                format!(r#"hybrid: {{ query: "{escaped_query}", alpha: {alpha} }}"#),
                "score",
            )
        } else {
            let certainty_clause = filter
                .min_certainty
                .map(|c| format!(", certainty: {c}"))
                .unwrap_or_default();
            (
                format!(r#"nearText: {{ concepts: ["{escaped_query}"]{certainty_clause} }}"#),
                "certainty",
            )
        };

        let mut graphql = format!(
            r#"{{
                Get {{
                    {collection}(
                        {search_clause}
                        limit: {fetch_limit}
                        {where_clause}
                    ) {{
                        {MEMORY_PROPERTY_FIELDS}
                        _additional {{
                            {additional_field}
                        }}
                    }}
                }}
            }}"#,
            where_clause = where_filter
                .map(|w| format!("where: {}", w))
                .unwrap_or_default(),
        );

        // Remove extra whitespace
        graphql = graphql.split_whitespace().collect::<Vec<_>>().join(" ");

        let url = format!("{}/v1/graphql", self.base_url);
        let resp = self
            .client
            .post(&url)
            .json(&serde_json::json!({ "query": graphql }))
            .send()
            .await
            .map_err(|e| eyre::eyre!("Weaviate GraphQL request failed: {}", e))?;

        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(eyre::eyre!("Weaviate recall failed: {}", body));
        }

        let body: serde_json::Value = resp.json().await?;
        Self::check_graphql_response(&body)?;

        if body["data"]["Get"][collection].is_null() {
            warn!(
                response = %body,
                "Weaviate Get returned null — query may be invalid"
            );
        }

        let items = body["data"]["Get"][collection]
            .as_array()
            .cloned()
            .unwrap_or_default();

        let results = items
            .into_iter()
            .filter_map(|item| {
                // Hybrid returns `_additional.score` as a STRING; nearText returns
                // `_additional.certainty` as a float. Read whichever this query used.
                let add = &item["_additional"];
                let score = if embedded {
                    match (&query_vector, add["vector"].as_array()) {
                        (Some(q), Some(stored)) => {
                            let stored: Vec<f32> = stored
                                .iter()
                                .filter_map(|x| x.as_f64().map(|v| v as f32))
                                .collect();
                            crate::embedding::map_score(
                                &self.score_map,
                                crate::embedding::certainty(q, &stored),
                            )
                        }
                        // Keyword fallback (or a row not yet embedded): no
                        // absolute relevance, so nothing clears a threshold.
                        _ => 0.0,
                    }
                } else if hybrid.is_some() {
                    add["score"]
                        .as_str()
                        .and_then(|s| s.parse::<f32>().ok())
                        .or_else(|| add["score"].as_f64().map(|v| v as f32))
                        .unwrap_or(0.0)
                } else {
                    add["certainty"].as_f64().unwrap_or(0.0) as f32
                };
                decode_memory(&item).map(|memory| MemorySearchResult { memory, score })
            })
            .collect();
        let results: Vec<MemorySearchResult> = if embedded {
            crate::embedding::rank_embedded(
                self.reranker.as_deref(),
                query.as_str(),
                results,
                filter.min_certainty,
                query_vector.is_some(),
            )
            .await
        } else {
            results
        };

        // event_after/event_before semantics are applied in-process: the
        // Weaviate `where` uses `createdAt` to keep IsNull out of the
        // filter (see `build_where_filter`), so `post_filtered` tightens
        // back to `event_time`, which already falls back to `created_at`
        // for legacy rows. It also trims the tag overfetch to `limit`.
        Ok(post_filtered(
            &filter,
            crate::access_audit::AuditOperation::Recall,
            results,
            limit,
        ))
    }

    pub fn new(base_url: impl Into<String>, api_key: Option<&str>) -> Self {
        use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION};

        let mut headers = HeaderMap::new();
        if let Some(key) = api_key {
            if let Ok(val) = HeaderValue::from_str(&format!("Bearer {}", key)) {
                headers.insert(AUTHORIZATION, val);
            }
        }

        // Bound every request so an unready/hung Weaviate (accepts the
        // connection then stalls) surfaces as an error the startup retry can
        // act on, instead of blocking forever. Generous enough for a paginated
        // schema-drift rebuild (100 objects/request).
        let client = Client::builder()
            .default_headers(headers)
            .connect_timeout(std::time::Duration::from_secs(10))
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .unwrap_or_else(|_| Client::new());

        Self {
            client,
            base_url: base_url.into(),
            collection: COLLECTION_NAME.to_string(),
            embedder: None,
            reranker: None,
            alpha: 0.75,
            rerank_candidates: 30,
            previous: std::sync::OnceLock::new(),
            score_map: Vec::new(),
        }
    }

    /// Keep memories in `collection`, vectorized by Weaviate as the default
    /// class is: for a store of its own beside the hub's (the edge's).
    pub fn in_collection(mut self, collection: &str) -> Self {
        self.collection = collection.to_string();
        self
    }

    /// Whether the collection exists, whatever its shape.
    pub async fn collection_exists(&self) -> Result<bool> {
        let url = format!("{}/v1/schema/{}", self.base_url, self.collection);
        let resp = self.client.get(&url).send().await?;
        match resp.status().as_u16() {
            200 => Ok(true),
            404 => Ok(false),
            status => Err(eyre::eyre!(
                "Weaviate schema read of '{}' returned {status}",
                self.collection
            )),
        }
    }

    /// Embed with the hub's model: memories move to `collection`, which holds
    /// the hub's vectors and no vectorizer; recall is hybrid at `alpha`,
    /// optionally reranked.
    pub fn with_embeddings(
        mut self,
        collection: &str,
        embedder: std::sync::Arc<dyn crate::embedding::EmbeddingBackend>,
        reranker: Option<std::sync::Arc<dyn crate::embedding::Reranker>>,
        alpha: f32,
        rerank_candidates: usize,
    ) -> Self {
        self.collection = collection.to_string();
        self.embedder = Some(embedder);
        self.reranker = reranker;
        self.alpha = alpha.clamp(0.0, 1.0);
        self.rerank_candidates = rerank_candidates.max(1);
        self
    }

    /// The rows one GraphQL `Get` returns from this collection.
    async fn get_rows(&self, graphql: String) -> Result<Vec<serde_json::Value>> {
        let body: serde_json::Value = self
            .client
            .post(format!("{}/v1/graphql", self.base_url))
            .json(&serde_json::json!({ "query": graphql }))
            .send()
            .await?
            .json()
            .await?;
        Self::check_graphql_response(&body)?;
        Ok(body["data"]["Get"][self.collection.as_str()]
            .as_array()
            .cloned()
            .unwrap_or_default())
    }

    /// One page of `owner`'s objects about `identity`, deleted ones included,
    /// from `offset`. The rows carry both values so callers can keep only the
    /// exact matches: what the index matched is never acted on unchecked.
    async fn identity_page(
        &self,
        owner: &AgentId,
        identity: &str,
        offset: usize,
    ) -> Result<Vec<serde_json::Value>> {
        self.get_rows(format!(
            r#"{{ Get {{ {collection}(limit: {IDENTITY_PAGE}, offset: {offset}, where: {{ operator: And, operands: [{{ path: ["owner"], operator: Equal, valueText: "{owner}" }}, {{ path: ["identityId"], operator: Equal, valueText: "{identity}" }}] }}) {{ memoryId owner identityId _additional {{ id }} }} }} }}"#,
            collection = self.collection,
            owner = graphql_escape(owner.as_str()),
            identity = graphql_escape(identity),
        ))
        .await
    }

    /// Every object of `owner`'s about `identity` (Weaviate id, memory id),
    /// deleted ones included. Paged by offset, so a history past Weaviate's
    /// per-query result cap fails the read instead of coming back short.
    async fn identity_objects(
        &self,
        owner: &AgentId,
        identity: &str,
    ) -> Result<Vec<(String, MemoryId)>> {
        let mut found = Vec::new();
        let mut offset = 0;
        loop {
            let page = self.identity_page(owner, identity, offset).await?;
            offset += page.len();
            found.extend(
                page.iter()
                    .filter(|item| is_about(item, owner, identity))
                    .filter_map(|item| {
                        Some((
                            item["_additional"]["id"].as_str()?.to_string(),
                            MemoryId(item["memoryId"].as_str()?.to_string()),
                        ))
                    }),
            );
            if page.len() < IDENTITY_PAGE {
                return Ok(found);
            }
        }
    }

    /// Send `request` for every object of `owner`'s about `identity`, deleted
    /// ones included, where each request takes its object out of that set (a
    /// delete, or a move to another identity). Each page is read from the start
    /// of what is left, past only the objects it stepped over, so however many
    /// there are, no read goes beyond Weaviate's per-query result cap. Returns
    /// how many objects were sent.
    async fn drain_identity(
        &self,
        owner: &AgentId,
        identity: &str,
        request: impl Fn(&str) -> reqwest::RequestBuilder,
    ) -> Result<usize> {
        let mut sent = std::collections::HashSet::new();
        let mut stepped_over = 0;
        loop {
            let page = self.identity_page(owner, identity, stepped_over).await?;
            if page.is_empty() {
                return Ok(sent.len());
            }
            let (fresh, rest): (Vec<_>, Vec<_>) = page.iter().partition(|item| {
                is_about(item, owner, identity)
                    && item["_additional"]["id"]
                        .as_str()
                        .is_some_and(|object| !sent.contains(object))
            });
            stepped_over += rest.len();
            for object in fresh
                .iter()
                .filter_map(|item| item["_additional"]["id"].as_str())
            {
                let resp = request(object).send().await?;
                if !resp.status().is_success() && resp.status() != reqwest::StatusCode::NOT_FOUND {
                    let body = resp.text().await.unwrap_or_default();
                    return Err(eyre::eyre!("Weaviate identity operation failed: {body}"));
                }
                sent.insert(object.to_string());
            }
        }
    }

    /// The URL of one object of this collection.
    fn object_url(&self, object: &str) -> String {
        format!("{}/v1/objects/{}/{object}", self.base_url, self.collection)
    }

    /// Report certainties through `knots` (see [`crate::embedding::map_score`]).
    pub fn with_score_map(mut self, knots: Vec<(f32, f32)>) -> Self {
        self.score_map = knots;
        self
    }

    /// Validate a pre-existing class against `EXPECTED_PROPERTIES`.
    /// Missing properties are not an error (migrate_schema adds them); wrong
    /// `dataType` or `tokenization` is a hard error because those can't be
    /// patched without recreating the class, and writing into a
    /// structurally-wrong class silently corrupts recall semantics.
    fn validate_schema_shape(&self, schema: &serde_json::Value) -> Result<()> {
        let collection = self.collection.as_str();
        let properties = schema["properties"].as_array().ok_or_else(|| {
            eyre::eyre!("Weaviate schema for '{collection}' has no properties array")
        })?;

        let mut mismatches: Vec<String> = Vec::new();

        for (name, expected_dtype, expected_tok) in EXPECTED_PROPERTIES {
            let Some(prop) = properties.iter().find(|p| p["name"].as_str() == Some(name)) else {
                continue;
            };

            let actual_dtype = prop["dataType"]
                .as_array()
                .and_then(|a| a.first())
                .and_then(|v| v.as_str())
                .unwrap_or("<unknown>");
            if actual_dtype != *expected_dtype {
                mismatches.push(format!(
                    "property '{name}': dataType is '{actual_dtype}', expected '{expected_dtype}'"
                ));
            }

            if let Some(expected_tok) = expected_tok {
                let actual_tok = prop["tokenization"].as_str().unwrap_or("<unset>");
                if actual_tok != *expected_tok {
                    mismatches.push(format!(
                        "property '{name}': tokenization is '{actual_tok}', expected '{expected_tok}'"
                    ));
                }
            }
        }

        // Non-fatal: classes created before we pinned replicationConfig and
        // shardingConfig lack those settings, and on Weaviate 1.36.x that can
        // surface as duplicate `nearText` results from multi-shard queries.
        // We warn rather than error so old deployments keep running; operators
        // should `workforce memory rebuild-schema --yes` to fix.
        let rep_factor = schema["replicationConfig"]["factor"].as_u64();
        if rep_factor != Some(1) {
            warn!(
                class = collection,
                replication_factor = ?rep_factor,
                "Weaviate class is missing replicationConfig.factor=1 pin; \
                 run `workforce memory rebuild-schema --yes` to eliminate \
                 duplicate-result risk on Weaviate 1.36+"
            );
        }
        let shard_count = schema["shardingConfig"]["desiredCount"].as_u64();
        if shard_count != Some(1) {
            warn!(
                class = collection,
                sharding_desired_count = ?shard_count,
                "Weaviate class is missing shardingConfig.desiredCount=1 pin; \
                 run `workforce memory rebuild-schema --yes` to eliminate \
                 duplicate-result risk on Weaviate 1.36+"
            );
        }

        if mismatches.is_empty() {
            return Ok(());
        }

        for m in &mismatches {
            error!(class = collection, "Weaviate schema mismatch: {}", m);
        }
        Err(eyre::Report::new(SchemaMismatchError {
            class: collection.to_string(),
            mismatches,
        }))
    }

    /// Add any missing properties to an existing collection (idempotent).
    async fn migrate_schema(&self) -> Result<()> {
        let collection = self.collection.as_str();
        let url = format!("{}/v1/schema/{}", self.base_url, collection);
        let resp = self.client.get(&url).send().await?;
        let schema: serde_json::Value = resp.json().await?;

        let existing: Vec<&str> = schema["properties"]
            .as_array()
            .map(|props| props.iter().filter_map(|p| p["name"].as_str()).collect())
            .unwrap_or_default();

        let migrations: Vec<serde_json::Value> = vec![
            serde_json::json!({
                "name": "createdAt",
                "dataType": ["date"],
                "description": "Timestamp when the memory was created"
            }),
            serde_json::json!({
                "name": "lastAccessed",
                "dataType": ["date"],
                "description": "Timestamp when the memory was last recalled"
            }),
            serde_json::json!({
                "name": "source",
                "dataType": ["text"],
                "description": "How the memory was produced (human, technical, curated, compaction, agent, unknown)",
                "tokenization": "field"
            }),
            serde_json::json!({
                "name": "identityId",
                "dataType": ["text"],
                "description": "Canonical identity UUID of the creator",
                "tokenization": "field"
            }),
            serde_json::json!({
                "name": "sourceSessionId",
                "dataType": ["text"],
                "description": "Session that produced this memory (for drill-down into the transcript)",
                "tokenization": "field"
            }),
            serde_json::json!({
                "name": "sourceMessageIds",
                "dataType": ["text[]"],
                "description": "Transcript message IDs within sourceSessionId covering this memory's turn",
                "tokenization": "field"
            }),
            serde_json::json!({
                "name": "recurrenceCount",
                "dataType": ["int"],
                "description": "How many times this memory has been re-extracted as a near-duplicate"
            }),
            serde_json::json!({
                "name": "lastRecurrenceAt",
                "dataType": ["date"],
                "description": "When this memory most recently recurred"
            }),
            serde_json::json!({
                "name": "eventTime",
                "dataType": ["date"],
                "description": "When the depicted event actually happened (latest source-message timestamp; falls back to createdAt)"
            }),
        ];

        let prop_url = format!("{}/v1/schema/{}/properties", self.base_url, collection);
        for prop in &migrations {
            let name = prop["name"].as_str().unwrap_or("");
            if existing.contains(&name) {
                continue;
            }
            info!(property = name, "Adding property to Weaviate schema");
            let resp = self.client.post(&prop_url).json(prop).send().await?;
            if !resp.status().is_success() {
                let body = resp.text().await.unwrap_or_default();
                warn!(property = name, error = %body, "Failed to add property (may already exist)");
            }
        }

        Ok(())
    }

    /// Check a Weaviate GraphQL response for errors.
    /// Returns Ok(()) on success, or an error with the Weaviate error message.
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

    fn build_where_filter(filter: &MemoryFilter) -> Option<String> {
        let mut conditions = vec![
            // Always exclude deleted
            r#"{ path: ["deleted"], operator: Equal, valueBoolean: false }"#.to_string(),
        ];

        if let Some(ref layer) = filter.layer {
            conditions.push(format!(
                r#"{{ path: ["layer"], operator: Equal, valueText: "{}" }}"#,
                layer_to_string(layer)
            ));
        }

        if let Some(ref owner) = filter.owner {
            conditions.push(format!(
                r#"{{ path: ["owner"], operator: Equal, valueText: "{}" }}"#,
                graphql_escape(owner.as_str())
            ));
        }

        if let Some(ref vis) = filter.visibility {
            conditions.push(format!(
                r#"{{ path: ["visibility"], operator: Equal, valueText: "{}" }}"#,
                visibility_to_string(vis)
            ));
        }

        if let Some(min_imp) = filter.min_importance {
            conditions.push(format!(
                r#"{{ path: ["importance"], operator: GreaterThanEqual, valueNumber: {} }}"#,
                min_imp
            ));
        }

        if let Some(ref source) = filter.source {
            conditions.push(format!(
                r#"{{ path: ["source"], operator: Equal, valueText: "{}" }}"#,
                source.as_str()
            ));
        }

        if let Some(ref identity_id) = filter.identity_id {
            let identity_id = graphql_escape(identity_id);
            let own = format!(
                r#"{{ path: ["identityId"], operator: Equal, valueText: "{identity_id}" }}"#
            );
            match filter.team_id.as_ref() {
                Some(team_id) => {
                    let team_id = graphql_escape(team_id);
                    let team = format!(
                        r#"{{ path: ["visibility"], operator: Equal, valueText: "team:{team_id}" }}"#
                    );
                    conditions.push(format!(r#"{{ operator: Or, operands: [{own}, {team}] }}"#));
                }
                None => conditions.push(own),
            }
        }

        if let Some(ref after) = filter.created_after {
            conditions.push(format!(
                r#"{{ path: ["createdAt"], operator: GreaterThanEqual, valueDate: "{}" }}"#,
                after.to_rfc3339()
            ));
        }

        if let Some(ref after) = filter.event_after {
            // Weaviate 1.28 has a filter bug where combining `IsNull` on one
            // property with `NotEqual` on another (which we use for tag
            // exclusion) silently returns zero rows. The cleanest workaround
            // is to use `createdAt` as a broad pre-cut at the index level
            // and apply precise event_time semantics in the Rust post-filter
            // (see `recall`). For pre-P3 memories `event_time` falls back
            // to `created_at`, so they're equivalent; for post-P3 backfills
            // of older turns the post-filter drops them correctly.
            conditions.push(format!(
                r#"{{ path: ["createdAt"], operator: GreaterThanEqual, valueDate: "{}" }}"#,
                after.to_rfc3339()
            ));
        }

        if let Some(ref before) = filter.event_before {
            conditions.push(format!(
                r#"{{ path: ["createdAt"], operator: LessThanEqual, valueDate: "{}" }}"#,
                before.to_rfc3339()
            ));
        }

        if let Some(ref before) = filter.created_before {
            conditions.push(format!(
                r#"{{ path: ["createdAt"], operator: LessThanEqual, valueDate: "{}" }}"#,
                before.to_rfc3339()
            ));
        }

        // Push tag exclusion into the Weaviate `where` so polluted memories
        // are filtered BEFORE nearText applies its top-N cut, not after. On a
        // store where polluted records dominate the highest-similarity matches
        // (which they do for queries like "what happened today" — recap
        // memories ABOUT activity score very close), post-filtering after a
        // small over-fetch can drop every legitimate result. Weaviate's
        // `NotEqual` on a `text[]` property returns objects whose array does
        // NOT contain the value, so each excluded tag becomes its own
        // condition. The post-filter in `recall` stays as a defence-in-depth.
        for tag in &filter.excluded_tags {
            conditions.push(format!(
                r#"{{ path: ["tags"], operator: NotEqual, valueText: "{}" }}"#,
                graphql_escape(tag)
            ));
        }

        // Positive tag filter: SQLite/InMemory treat `filter.tags` as "memory
        // has ANY of these tags" (OR semantics — see `sqlite.rs::350` and
        // `in_memory.rs::209`). Match that here so the pinned-memory injection
        // path (`serve.rs::4793`, `filter.tags = ["pinned"]`) actually scopes
        // to pinned rows instead of returning the user's most-recent N.
        //
        // This clause is a candidate pre-filter only: on collections whose
        // `tags` property is word-tokenized (pre-`field` rebuild), Equal
        // matches on shared TOKENS, so `date:2026-07-05` also admits
        // `date:2026-05-07`. `recall`/`list` re-check each decoded memory
        // with `passes_required_tags` for exact-string semantics.
        if let Some(tags) = filter.tags.as_ref().filter(|t| !t.is_empty()) {
            let operands: Vec<String> = tags
                .iter()
                .map(|t| {
                    format!(
                        r#"{{ path: ["tags"], operator: Equal, valueText: "{}" }}"#,
                        graphql_escape(t)
                    )
                })
                .collect();
            conditions.push(if operands.len() == 1 {
                operands.into_iter().next().unwrap()
            } else {
                format!("{{ operator: Or, operands: [{}] }}", operands.join(", "))
            });
        }

        if conditions.len() == 1 {
            Some(conditions.remove(0))
        } else {
            Some(format!(
                "{{ operator: And, operands: [{}] }}",
                conditions.join(", ")
            ))
        }
    }
}

#[async_trait]
impl MemoryAdmin for WeaviateStore {
    fn collection(&self) -> &str {
        &self.collection
    }

    /// Ensure the Weaviate collection exists.
    ///
    /// When the class already exists, the shape is validated against
    /// `EXPECTED_PROPERTIES`. Hard mismatches (wrong `dataType`, wrong
    /// `tokenization`) return `Err` so the hub fails loud at start-up —
    /// recover via `workforce memory rebuild-schema`.
    async fn ensure_collection(&self) -> Result<()> {
        let collection = self.collection.as_str();
        let url = format!("{}/v1/schema/{}", self.base_url, collection);
        let resp = self.client.get(&url).send().await;

        match resp {
            Ok(r) if r.status().is_success() => {
                debug!("Weaviate collection '{}' already exists", collection);
                let body: serde_json::Value = r.json().await?;
                self.validate_schema_shape(&body)?;
                self.migrate_schema().await?;
                Ok(())
            }
            _ => {
                info!("Creating Weaviate collection '{}'", collection);
                let vectorizer = if self.embedder.is_some() {
                    "none"
                } else {
                    "text2vec-transformers"
                };
                let schema = serde_json::json!({
                    "class": collection,
                    "description": "AI Workforce agent memories",
                    "vectorizer": vectorizer,
                    "invertedIndexConfig": {
                        "stopwords": {
                            "preset": "en",
                            "additions": [],
                            "removals": []
                        },
                        "indexTimestamps": true,
                        "indexNullState": true
                    },
                    // Pin single-shard / single-replica so multi-shard nearText
                    // responses on 1.36.x can't emit the same object twice.
                    "replicationConfig": { "factor": 1 },
                    "shardingConfig": { "desiredCount": 1 },
                    "properties": [
                        {
                            "name": "content",
                            "dataType": ["text"],
                            "description": "Memory content text"
                        },
                        {
                            "name": "memoryId",
                            "dataType": ["text"],
                            "description": "Unique memory ID",
                            "tokenization": "field"
                        },
                        {
                            "name": "layer",
                            "dataType": ["text"],
                            "description": "Memory layer (working, episodic, semantic, team, organisation)",
                            "tokenization": "field"
                        },
                        {
                            "name": "owner",
                            "dataType": ["text"],
                            "description": "Owner agent ID",
                            "tokenization": "field"
                        },
                        {
                            "name": "visibility",
                            "dataType": ["text"],
                            "description": "Visibility (private, team:{id}, global)",
                            "tokenization": "field"
                        },
                        {
                            "name": "importance",
                            "dataType": ["number"],
                            "description": "Importance score 0-1"
                        },
                        {
                            "name": "tags",
                            "dataType": ["text[]"],
                            "description": "Metadata tags",
                            "tokenization": "field"
                        },
                        {
                            "name": "deleted",
                            "dataType": ["boolean"],
                            "description": "Soft delete flag"
                        },
                        {
                            "name": "accessCount",
                            "dataType": ["int"],
                            "description": "Number of times recalled"
                        },
                        {
                            "name": "createdAt",
                            "dataType": ["date"],
                            "description": "Timestamp when the memory was created"
                        },
                        {
                            "name": "lastAccessed",
                            "dataType": ["date"],
                            "description": "Timestamp when the memory was last recalled"
                        },
                        {
                            "name": "source",
                            "dataType": ["text"],
                            "description": "How the memory was produced (human, technical, curated, compaction, agent, unknown)",
                            "tokenization": "field"
                        },
                        {
                            "name": "identityId",
                            "dataType": ["text"],
                            "description": "Canonical identity UUID of the creator",
                            "tokenization": "field"
                        },
                        {
                            "name": "sourceSessionId",
                            "dataType": ["text"],
                            "description": "Session that produced this memory (for drill-down into the transcript)",
                            "tokenization": "field"
                        },
                        {
                            "name": "sourceMessageIds",
                            "dataType": ["text[]"],
                            "description": "Transcript message IDs within sourceSessionId covering this memory's turn",
                            "tokenization": "field"
                        },
                        {
                            "name": "recurrenceCount",
                            "dataType": ["int"],
                            "description": "How many times this memory has been re-extracted as a near-duplicate"
                        },
                        {
                            "name": "lastRecurrenceAt",
                            "dataType": ["date"],
                            "description": "When this memory most recently recurred"
                        },
                        {
                            "name": "eventTime",
                            "dataType": ["date"],
                            "description": "When the depicted event actually happened (latest source-message timestamp; falls back to createdAt)"
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
                    .map_err(|e| eyre::eyre!("Failed to create Weaviate collection: {}", e))?;

                if !resp.status().is_success() {
                    let body = resp.text().await.unwrap_or_default();
                    return Err(eyre::eyre!(
                        "Failed to create Weaviate collection: {}",
                        body
                    ));
                }

                info!("Created Weaviate collection '{}'", collection);
                Ok(())
            }
        }
    }

    /// Like [`Self::ensure_collection`] but auto-recovers from a stale
    /// pre-existing schema (the symptom of a redeploy after the property
    /// shape changed): export → write backup → delete → recreate → import.
    /// Mirrors the CLI's `workforce memory rebuild-schema --yes` flow.
    ///
    /// Connection errors propagate so the caller's retry loop still applies
    /// — only [`SchemaMismatchError`] triggers the rebuild.
    async fn ensure_collection_with_recovery(
        &self,
        backup_dir: Option<&Path>,
    ) -> Result<RecoveryOutcome> {
        let collection = self.collection.as_str();
        match self.ensure_collection().await {
            Ok(()) => Ok(RecoveryOutcome::AlreadyValid),
            Err(e) => match e.downcast_ref::<SchemaMismatchError>() {
                Some(mismatch) => {
                    let mismatches = mismatch.mismatches.clone();
                    warn!(
                        class = collection,
                        count = mismatches.len(),
                        "Weaviate schema drift detected — rebuilding class \
                         (export → backup → delete → recreate → import)"
                    );
                    let exported = self.export(true).await?;
                    let backup_path = match backup_dir {
                        Some(dir) => Some(write_backup(dir, &exported)?),
                        None => None,
                    };
                    self.delete_collection().await?;
                    self.ensure_collection().await?;
                    let imported = self.import(&exported).await?;
                    if imported != exported.len() {
                        warn!(
                            exported = exported.len(),
                            imported,
                            backup = ?backup_path,
                            "Weaviate rebuild lost objects on re-ingest; backup preserved"
                        );
                    } else {
                        info!(
                            exported = exported.len(),
                            imported,
                            backup = ?backup_path,
                            "Weaviate rebuild complete"
                        );
                    }
                    Ok(RecoveryOutcome::Rebuilt {
                        exported: exported.len(),
                        imported,
                        backup_path,
                        mismatches,
                    })
                }
                None => Err(e),
            },
        }
    }

    /// Read-only shape check for `workforce memory doctor`. Returns Err with
    /// a detailed message on any mismatch — no schema modifications.
    async fn doctor(&self) -> Result<()> {
        let collection = self.collection.as_str();
        let url = format!("{}/v1/schema/{}", self.base_url, collection);
        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| eyre::eyre!("Weaviate schema fetch failed: {}", e))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(eyre::eyre!(
                "Weaviate class '{}' not reachable (status {}): {}",
                collection,
                status,
                body
            ));
        }
        let body: serde_json::Value = resp.json().await?;
        self.validate_schema_shape(&body)
    }

    async fn import(&self, objects: &[serde_json::Value]) -> Result<usize> {
        let url = format!("{}/v1/batch/objects", self.base_url);
        let prepared: Vec<serde_json::Value> = objects
            .iter()
            .filter_map(|o| prepare_import_object(o, &self.collection))
            .collect();
        let filtered = objects.len() - prepared.len();

        // Vector-carrying objects insert without touching the vectorizer, so
        // a full export page per request is safe. Vector-less objects make
        // Weaviate embed each one inside the request — keep those batches
        // small enough to finish within the client's 30s timeout.
        let all_have_vectors = prepared.iter().all(|o| o.get("vector").is_some());
        let batch_size = if all_have_vectors {
            IMPORT_BATCH_SIZE
        } else {
            IMPORT_BATCH_SIZE_REVECTORIZE
        };

        let mut count = 0usize;
        // What could not be stored, by id, with why.
        let mut failed: Vec<(String, String)> = Vec::new();
        let id_of = |object: &serde_json::Value| object["id"].as_str().unwrap_or("?").to_string();
        for chunk in prepared.chunks(batch_size) {
            match self.import_batch(&url, chunk).await {
                Ok(refused) => {
                    count += chunk.len() - refused.len();
                    failed.extend(refused);
                }
                // One object at a time, so that the one Weaviate will not
                // take does not take the others with it.
                Err(e) => {
                    warn!(error = %e, batch = chunk.len(), "a batch was not re-ingested; sending its objects one by one");
                    for one in chunk {
                        match self.import_batch(&url, std::slice::from_ref(one)).await {
                            Ok(refused) if refused.is_empty() => count += 1,
                            Ok(refused) => failed.extend(refused),
                            Err(e) => failed.push((id_of(one), format!("{e:#}"))),
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
                "skipped non-vectorizable objects during import"
            );
        }
        crate::admin::imported(count, failed)
    }

    /// PATCH the `tags` property of an object addressed by Weaviate UUID.
    ///
    /// Used by the audit/cleanup CLI which already has the Weaviate UUID
    /// from a raw export — saves a round-trip vs. the `update_tags` trait
    /// method which has to look the UUID up by `memoryId`.
    async fn patch_tags(&self, weaviate_id: &str, tags: &[String]) -> Result<()> {
        let collection = self.collection.as_str();
        let url = format!("{}/v1/objects/{collection}/{weaviate_id}", self.base_url);
        let resp = self
            .client
            .patch(&url)
            .json(&serde_json::json!({
                "properties": { "tags": tags }
            }))
            .send()
            .await?;
        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(eyre::eyre!(
                "patch_tags_by_uuid failed for {weaviate_id}: {body}"
            ));
        }
        Ok(())
    }

    /// Delete the collection entirely. Destructive — callers should export
    /// first. Returns Ok on 2xx and 404 (already gone).
    async fn delete_collection(&self) -> Result<()> {
        let collection = self.collection.as_str();
        let url = format!("{}/v1/schema/{}", self.base_url, collection);
        let resp = self.client.delete(&url).send().await?;
        let status = resp.status();
        if !status.is_success() && status != reqwest::StatusCode::NOT_FOUND {
            let body = resp.text().await.unwrap_or_default();
            return Err(eyre::eyre!(
                "Weaviate delete class failed ({status}): {body}"
            ));
        }
        info!(class = collection, "Deleted Weaviate class");
        Ok(())
    }
}

#[async_trait]
impl SyncSource for WeaviateStore {
    /// One page of the collection's raw `{ id, class, properties, vector }`
    /// objects, through the REST `after` cursor, which walks them in id
    /// order.
    async fn export_page(
        &self,
        after: Option<&str>,
        include_vector: bool,
    ) -> Result<Vec<serde_json::Value>> {
        let mut url = format!(
            "{}/v1/objects?class={}&limit={EXPORT_PAGE}",
            self.base_url, self.collection
        );
        if include_vector {
            url.push_str("&include=vector");
        }
        if let Some(cursor) = after {
            url.push_str(&format!("&after={cursor}"));
        }
        let resp = self.client.get(&url).send().await?;
        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(eyre::eyre!("Weaviate list failed: {}", body));
        }
        let body: serde_json::Value = resp.json().await?;
        Ok(body["objects"].as_array().cloned().unwrap_or_default())
    }

    fn embedding_model(&self) -> Option<&str> {
        self.embedder.as_ref().map(|embedder| embedder.model_id())
    }

    /// The objects' ids are found by `memoryId`, a page at a time, so that
    /// none is left out past one query's limit; each object is then read
    /// as the export reads it, so that it carries every property it has:
    /// one read short of a property would have that property taken off
    /// the copy.
    async fn objects_of_memory(
        &self,
        memory_id: &str,
        with_vectors: bool,
    ) -> Result<Vec<serde_json::Value>> {
        let mut ids = Vec::new();
        let mut offset = 0;
        loop {
            let page = self
                .get_rows(format!(
                    r#"{{ Get {{ {collection}(limit: {MEMORY_OBJECTS_PAGE}, offset: {offset}, where: {{ path: ["memoryId"], operator: Equal, valueText: "{memory}" }}) {{ memoryId _additional {{ id }} }} }} }}"#,
                    collection = self.collection,
                    memory = graphql_escape(memory_id),
                ))
                .await?;
            offset += page.len();
            ids.extend(
                page.iter()
                    .filter(|row| row["memoryId"] == memory_id)
                    .filter_map(|row| Some(row["_additional"]["id"].as_str()?.to_string())),
            );
            if page.len() < MEMORY_OBJECTS_PAGE {
                break;
            }
        }
        self.objects_by_id(&ids, with_vectors).await
    }

    /// Each read as the export reads it, a few at a time; one gone since
    /// it was asked for is left out.
    async fn objects_by_id(
        &self,
        ids: &[String],
        with_vectors: bool,
    ) -> Result<Vec<serde_json::Value>> {
        let read = |id: String| async move {
            let mut url = format!("{}/v1/objects/{}/{id}", self.base_url, self.collection);
            if with_vectors {
                url.push_str("?include=vector");
            }
            let resp = self.client.get(&url).send().await?;
            match resp.status().as_u16() {
                200 => Ok(Some(resp.json::<serde_json::Value>().await?)),
                404 => Ok(None),
                status => Err(eyre::eyre!("Weaviate object read returned {status}")),
            }
        };
        let read: Vec<Result<Option<serde_json::Value>>> =
            futures::stream::iter(ids.iter().cloned())
                .map(read)
                .buffered(OBJECT_READS)
                .collect()
                .await;
        read.into_iter().filter_map(Result::transpose).collect()
    }
}

#[async_trait]
impl SyncTarget for WeaviateStore {
    async fn upsert(&self, objects: &[serde_json::Value]) -> Result<usize> {
        self.import(objects).await
    }

    /// Merge `properties` into an object; with no vectorizer, its vector stays.
    async fn patch(&self, id: &str, properties: serde_json::Value) -> Result<()> {
        let url = format!("{}/v1/objects/{}/{}", self.base_url, self.collection, id);
        let resp = self
            .client
            .patch(&url)
            .json(&serde_json::json!({ "class": self.collection, "properties": properties }))
            .send()
            .await?;
        if resp.status().is_success() || resp.status().as_u16() == 404 {
            Ok(())
        } else {
            Err(eyre::eyre!("Weaviate patch returned {}", resp.status()))
        }
    }

    async fn delete(&self, id: &str) -> Result<()> {
        let url = format!("{}/v1/objects/{}/{}", self.base_url, self.collection, id);
        let resp = self.client.delete(&url).send().await?;
        if !resp.status().is_success() && resp.status().as_u16() != 404 {
            return Err(eyre::eyre!("Weaviate delete returned {}", resp.status()));
        }
        Ok(())
    }

    async fn vectors(&self, texts: &[String]) -> Result<Option<Vec<Vec<f32>>>> {
        match &self.embedder {
            Some(embedder) => Ok(Some(
                embedder
                    .embed_many(texts, crate::embedding::EmbedKind::Document)
                    .await?,
            )),
            None => Ok(None),
        }
    }

    /// Whether memories have switched to this collection: a marker written
    /// once a full sync finished, after which the old collection is no longer
    /// read.
    async fn switched(&self) -> Result<bool> {
        let url = format!(
            "{}/v1/objects/{SWITCH_COLLECTION}/{}",
            self.base_url,
            switch_marker_id(&self.collection)
        );
        let resp = self.client.get(&url).send().await?;
        match resp.status().as_u16() {
            200 => Ok(true),
            404 => Ok(false),
            status => Err(eyre::eyre!("Weaviate switch marker read returned {status}")),
        }
    }

    /// Record the switch (see [`Self::switched`]); idempotent.
    async fn mark_switched(&self) -> Result<()> {
        let schema = serde_json::json!({
            "class": SWITCH_COLLECTION,
            "description": "Memory collections the hub has switched to",
            "vectorizer": "none",
            "properties": [
                { "name": "collection", "dataType": ["text"], "tokenization": "field" },
                { "name": "switchedAt", "dataType": ["date"] }
            ]
        });
        let resp = self
            .client
            .post(format!("{}/v1/schema", self.base_url))
            .json(&schema)
            .send()
            .await?;
        if !resp.status().is_success() && resp.status().as_u16() != 422 {
            return Err(eyre::eyre!(
                "Weaviate switch class create returned {}",
                resp.status()
            ));
        }
        let resp = self
            .client
            .post(format!("{}/v1/objects", self.base_url))
            .json(&serde_json::json!({
                "class": SWITCH_COLLECTION,
                "id": switch_marker_id(&self.collection),
                "properties": {
                    "collection": self.collection,
                    "switchedAt": chrono::Utc::now().to_rfc3339(),
                },
            }))
            .send()
            .await?;
        if resp.status().is_success() || resp.status().as_u16() == 422 {
            Ok(())
        } else {
            Err(eyre::eyre!(
                "Weaviate switch marker write returned {}",
                resp.status()
            ))
        }
    }

    /// Nothing promotes memories to Weaviate from a store it shadowed, so
    /// a replica of one into Weaviate is refused rather than taken as safe.
    async fn promoted(&self) -> Result<bool> {
        Err(eyre::eyre!(
            "a Weaviate collection is never kept as a replica of another store, so no replica sync is run into {}",
            self.collection
        ))
    }
}

impl Switchable for WeaviateStore {
    /// The first store given is kept; a store switches from one other.
    fn keep_previous(&self, previous: std::sync::Arc<dyn MemoryStore>) {
        let kept = self.previous.set(previous).is_ok();
        debug_assert!(kept, "a store switches from one other");
    }
}

impl WeaviateStore {
    /// Send `batch` to the batch endpoint. `Err` if Weaviate did not take
    /// the request; otherwise the `(id, why)` of each object in it that
    /// Weaviate answered was not stored.
    async fn import_batch(
        &self,
        url: &str,
        batch: &[serde_json::Value],
    ) -> Result<Vec<(String, String)>> {
        let body = serde_json::json!({ "objects": batch });
        let resp = self.client.post(url).json(&body).send().await?;
        if !resp.status().is_success() {
            let status = resp.status();
            let said: String = resp.text().await.unwrap_or_default();
            let said: String = said.chars().take(300).collect();
            return Err(eyre::eyre!("Weaviate answered {status}: {said}"));
        }
        // The batch endpoint returns 200 with a result for each object.
        let body: serde_json::Value = resp.json().await?;
        let results = body.as_array().cloned().unwrap_or_default();
        if results.len() != batch.len() {
            return Err(eyre::eyre!(
                "Weaviate answered for {} of the {} objects it was sent",
                results.len(),
                batch.len()
            ));
        }
        Ok(results
            .iter()
            .zip(batch)
            .filter(|(result, _)| {
                let status = result["result"]["status"].as_str().unwrap_or("");
                !status.eq_ignore_ascii_case("SUCCESS")
            })
            .map(|(result, sent)| {
                let id = sent["id"].as_str().unwrap_or("?").to_string();
                (id, result["result"].to_string())
            })
            .collect())
    }
}

/// Prepare one exported raw object for re-ingest, or `None` to drop it.
///
/// Preserves the original object `id` (batch upserts by id, so a retried
/// import is idempotent instead of duplicating) and the exported `vector`
/// so Weaviate skips re-embedding — safe because content is unchanged and
/// the vectorizer is deterministic; before this, a schema-migration rebuild
/// re-vectorized every object one POST at a time (minutes for a few
/// thousand memories), far too slow to run against a live install. Content
/// that could never vectorize (empty / no alphanumerics) is dropped, as the
/// per-object path always did. Empty-string `identityId` normalises to null
/// so per-user scoping's IsNull semantics hold after a round-trip.
fn prepare_import_object(obj: &serde_json::Value, collection: &str) -> Option<serde_json::Value> {
    let props = crate::admin::normalised(obj)?["properties"].take();
    let mut out = serde_json::json!({ "class": collection, "properties": props });
    if let Some(id) = obj.get("id").and_then(|v| v.as_str()) {
        out["id"] = serde_json::json!(id);
    }
    if let Some(vector) = obj.get("vector").filter(|v| v.is_array()) {
        out["vector"] = vector.clone();
    }
    Some(out)
}

/// Write a JSON backup of exported Weaviate objects to
/// `{dir}/memory-backup-{ts}.json`. Mkdir the parent so first-deploy works.
/// Failure is fatal: rebuilding without a backup risks data loss. Every user's
/// memories are in it, so the directory is the hub's alone.
fn write_backup(dir: &Path, objects: &[serde_json::Value]) -> Result<PathBuf> {
    workforce_common::private_fs::ensure_private_dir(dir)
        .map_err(|e| eyre::eyre!("Failed to create backup dir {}: {}", dir.display(), e))?;
    let path = dir.join(format!(
        "memory-backup-{}.json",
        chrono::Utc::now().format("%Y%m%d-%H%M%S")
    ));
    let body = serde_json::json!({ "objects": objects });
    workforce_common::private_fs::private_file_options()
        .write(true)
        .create_new(true)
        .open(&path)
        .and_then(|mut file| {
            std::io::Write::write_all(&mut file, serde_json::to_string_pretty(&body)?.as_bytes())
        })
        .map_err(|e| eyre::eyre!("Failed to write backup {}: {}", path.display(), e))?;
    info!(path = %path.display(), count = objects.len(), "Wrote Weaviate backup");
    Ok(path)
}

const MEMORY_PROPERTY_FIELDS: &str = "content memoryId layer owner visibility importance tags \
     deleted accessCount createdAt lastAccessed source identityId sourceSessionId \
     sourceMessageIds recurrenceCount lastRecurrenceAt eventTime";

/// Whether a row read back is `owner`'s and about exactly `identity`.
fn is_about(item: &serde_json::Value, owner: &AgentId, identity: &str) -> bool {
    item["owner"].as_str() == Some(owner.as_str()) && item["identityId"].as_str() == Some(identity)
}

/// Escape a dynamic string for embedding inside a double-quoted GraphQL
/// string literal. GraphQL forbids raw control characters in strings, and
/// they do occur in our data — the edge's external identity ids join issuer
/// and subject with a `\u{1F}` unit separator, which unescaped makes Weaviate
/// reject the whole query ("Invalid character within String"). Backslash
/// first, then quote, then `\uXXXX` for controls.
fn graphql_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04X}", c as u32));
            }
            c => out.push(c),
        }
    }
    out
}

// Weaviate `get` excludes soft-deleted rows; SQLite/InMemory `get` do not.
// Matches the `deleted=false` predicate the previous `recall("")` path
// implicitly applied via `build_where_filter`.
fn build_get_by_id_query(id: &MemoryId, collection: &str) -> String {
    let escaped = graphql_escape(&id.0);
    let raw = format!(
        r#"{{ Get {{ {collection}(
            where: {{ operator: And, operands: [
                {{ path: ["memoryId"], operator: Equal, valueText: "{escaped}" }},
                {{ path: ["deleted"], operator: Equal, valueBoolean: false }}
            ] }}
            limit: 1
        ) {{ {MEMORY_PROPERTY_FIELDS} }} }} }}"#,
    );
    raw.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Server-side fetch bound for a query that will be post-filtered with
/// `passes_required_tags`. On legacy word-tokenized collections the
/// where-clause admits token cross-matches (see `build_where_filter`), and
/// those are dropped only AFTER Weaviate applied `limit` — a page crowded
/// with cross-matched cousins could starve genuine rows. Overfetch 3×
/// (capped) when tags are requested; callers still get at most `limit`.
fn fetch_limit_for(filter: &MemoryFilter, limit: usize) -> usize {
    if filter.tags.as_ref().is_some_and(|t| !t.is_empty()) {
        limit.saturating_mul(3).min(500).max(limit)
    } else {
        limit
    }
}

fn build_list_query(filter: &MemoryFilter, limit: usize, collection: &str) -> String {
    let where_clause = WeaviateStore::build_where_filter(filter)
        .map(|w| format!("where: {w} "))
        .unwrap_or_default();
    let raw = format!(
        r#"{{ Get {{ {collection}({where_clause}limit: {limit}) {{ {MEMORY_PROPERTY_FIELDS} }} }} }}"#,
    );
    raw.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[async_trait]
impl MemoryStore for WeaviateStore {
    async fn remember(&self, memory: Memory) -> Result<MemoryId> {
        let collection = self.collection.as_str();
        let content = VectorizableText::try_new(&memory.content).map_err(|e| {
            warn!(
                target: "vectorizable",
                operation = "memory_remember",
                reason = e.as_metric_label(),
                memory_id = %memory.id,
                "rejected non-vectorizable content"
            );
            eyre::eyre!("refusing to store memory with non-vectorizable content: {e}")
        })?;
        let url = format!("{}/v1/objects", self.base_url);
        let mut object = serde_json::json!({
            "class": collection,
            "properties": shape::properties(&memory, &content),
        });
        if let Some(embedder) = &self.embedder {
            // Only the content is embedded: ids, owner and tags carry no
            // meaning and would crowd it out of the vector.
            match embedder.embed(&memory.content).await {
                Ok(vector) => object["vector"] = serde_json::json!(vector),
                Err(e) => warn!(
                    target: "embeddings",
                    error = %e,
                    memory_id = %memory.id,
                    "embedding failed; memory stored without a vector, found by keywords until backfilled"
                ),
            }
        }

        let resp = self
            .client
            .post(&url)
            .json(&object)
            .send()
            .await
            .map_err(|e| eyre::eyre!("Weaviate request failed: {}", e))?;

        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(eyre::eyre!("Weaviate store failed: {}", body));
        }

        debug!(memory_id = %memory.id, "Stored memory in Weaviate");
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

    async fn get(&self, id: &MemoryId) -> Result<Option<Memory>> {
        let collection = self.collection.as_str();
        // System-internal id lookup — bypasses per-user scoping (`recall` and
        // `list` enforce it via `check_and_audit_scope`; `get` does not).
        // Callers verify ownership against the returned `identity_id`.
        let url = format!("{}/v1/graphql", self.base_url);
        let resp = self
            .client
            .post(&url)
            .json(&serde_json::json!({ "query": build_get_by_id_query(id, &self.collection) }))
            .send()
            .await?;

        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(eyre::eyre!("Weaviate get failed: {}", body));
        }

        let body: serde_json::Value = resp.json().await?;
        Self::check_graphql_response(&body)?;

        Ok(body["data"]["Get"][collection]
            .as_array()
            .and_then(|arr| arr.first())
            .and_then(decode_memory))
    }

    async fn forget(&self, id: &MemoryId) -> Result<()> {
        let collection = self.collection.as_str();
        // Find the Weaviate object by memoryId and update deleted flag
        let graphql = format!(
            r#"{{ Get {{ {collection}(where: {{ path: ["memoryId"], operator: Equal, valueText: "{}" }}) {{ _additional {{ id }} }} }} }}"#,
            graphql_escape(&id.0)
        );

        let url = format!("{}/v1/graphql", self.base_url);
        let resp = self
            .client
            .post(&url)
            .json(&serde_json::json!({ "query": graphql }))
            .send()
            .await?;

        let body: serde_json::Value = resp.json().await?;
        Self::check_graphql_response(&body)?;

        let items = body["data"]["Get"][collection]
            .as_array()
            .cloned()
            .unwrap_or_default();

        for item in items {
            if let Some(weaviate_id) = item["_additional"]["id"].as_str() {
                let update_url = format!(
                    "{}/v1/objects/{}/{}",
                    self.base_url, collection, weaviate_id
                );
                self.client
                    .patch(&update_url)
                    .json(&serde_json::json!({
                        "properties": { "deleted": true }
                    }))
                    .send()
                    .await?;
            }
        }

        debug!(memory_id = %id, "Soft-deleted memory in Weaviate");
        if let Some(previous) = self.previous.get() {
            if let Err(e) = previous.forget(id).await {
                warn!(target: "embeddings", memory_id = %id, error = %e, "forgetting in the previous collection failed");
            }
        }
        Ok(())
    }

    async fn share(&self, id: &MemoryId, visibility: Visibility) -> Result<()> {
        let collection = self.collection.as_str();
        let graphql = format!(
            r#"{{ Get {{ {collection}(where: {{ path: ["memoryId"], operator: Equal, valueText: "{}" }}) {{ _additional {{ id }} }} }} }}"#,
            graphql_escape(&id.0)
        );

        let url = format!("{}/v1/graphql", self.base_url);
        let resp = self
            .client
            .post(&url)
            .json(&serde_json::json!({ "query": graphql }))
            .send()
            .await?;

        let body: serde_json::Value = resp.json().await?;
        Self::check_graphql_response(&body)?;

        let items = body["data"]["Get"][collection]
            .as_array()
            .cloned()
            .unwrap_or_default();

        let vis_str = visibility_to_string(&visibility);
        for item in items {
            if let Some(weaviate_id) = item["_additional"]["id"].as_str() {
                let update_url = format!(
                    "{}/v1/objects/{}/{}",
                    self.base_url, collection, weaviate_id
                );
                self.client
                    .patch(&update_url)
                    .json(&serde_json::json!({
                        "properties": { "visibility": vis_str }
                    }))
                    .send()
                    .await?;
            }
        }

        Ok(())
    }

    async fn count(&self, filter: MemoryFilter) -> Result<usize> {
        let collection = self.collection.as_str();
        if !crate::store::check_and_audit_scope(&filter, crate::access_audit::AuditOperation::Count)
        {
            return Ok(0);
        }
        let where_filter = Self::build_where_filter(&filter);
        let graphql = format!(
            r#"{{ Aggregate {{ {collection}({where_clause}) {{ meta {{ count }} }} }} }}"#,
            where_clause = where_filter
                .map(|w| format!("where: {}", w))
                .unwrap_or_default(),
        );
        // Normalise whitespace (same as recall)
        let graphql = graphql.split_whitespace().collect::<Vec<_>>().join(" ");

        let url = format!("{}/v1/graphql", self.base_url);
        let resp = self
            .client
            .post(&url)
            .json(&serde_json::json!({ "query": graphql }))
            .send()
            .await?;

        let body: serde_json::Value = resp.json().await?;
        Self::check_graphql_response(&body)?;

        if body["data"]["Aggregate"][collection].is_null() {
            warn!(
                response = %body,
                "Weaviate Aggregate returned null — collection may not exist or query may be invalid"
            );
        }

        let count = body["data"]["Aggregate"][collection][0]["meta"]["count"]
            .as_u64()
            .unwrap_or(0) as usize;

        Ok(count)
    }

    async fn list(&self, mut filter: MemoryFilter) -> Result<Vec<Memory>> {
        let collection = self.collection.as_str();
        if !crate::store::check_and_audit_scope(
            &filter,
            crate::access_audit::AuditOperation::Recall,
        ) {
            return Ok(Vec::new());
        }
        apply_pollution_default(&mut filter);
        let limit = filter.limit.unwrap_or(10);
        let graphql = build_list_query(&filter, fetch_limit_for(&filter, limit), &self.collection);

        let url = format!("{}/v1/graphql", self.base_url);
        let resp = self
            .client
            .post(&url)
            .json(&serde_json::json!({ "query": graphql }))
            .send()
            .await?;

        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(eyre::eyre!("Weaviate list failed: {}", body));
        }

        let body: serde_json::Value = resp.json().await?;
        Self::check_graphql_response(&body)?;

        let items = body["data"]["Get"][collection]
            .as_array()
            .cloned()
            .unwrap_or_default();
        Ok(post_filtered(
            &filter,
            crate::access_audit::AuditOperation::Recall,
            items.iter().filter_map(decode_memory).collect(),
            limit,
        ))
    }

    async fn update_tags(&self, id: &MemoryId, tags: Vec<String>) -> Result<()> {
        let collection = self.collection.as_str();
        // Find the Weaviate object UUID by our memoryId property
        let graphql = format!(
            r#"{{ Get {{ {collection}(where: {{ path: ["memoryId"], operator: Equal, valueText: "{}" }}, limit: 1) {{ _additional {{ id }} }} }} }}"#,
            graphql_escape(&id.0)
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

        let weaviate_id = body["data"]["Get"][collection][0]["_additional"]["id"]
            .as_str()
            .ok_or_else(|| eyre::eyre!("Memory '{}' not found in Weaviate", id.0))?;

        // PATCH using the Weaviate object UUID
        let patch_url = format!("{}/v1/objects/{collection}/{}", self.base_url, weaviate_id);
        let resp = self
            .client
            .patch(&patch_url)
            .json(&serde_json::json!({
                "properties": { "tags": tags }
            }))
            .send()
            .await?;
        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(eyre::eyre!("Weaviate update_tags failed: {}", body));
        }
        Ok(())
    }

    async fn identity_memories(&self, owner: &AgentId, identity: &str) -> Result<Vec<MemoryId>> {
        Ok(self
            .identity_objects(owner, identity)
            .await?
            .into_iter()
            .map(|(_, memory_id)| memory_id)
            .collect())
    }

    /// Weaviate's cursor cannot be combined with a filter, and a filtered read
    /// is capped, so the whole collection is walked by id and `owner`'s rows
    /// picked out here: slower, but never cut short however much it holds.
    /// The rollback copy mostly repeats this one, so it raises a count to its
    /// own rather than adding to it.
    async fn identity_counts(
        &self,
        owner: &AgentId,
    ) -> Result<std::collections::BTreeMap<String, usize>> {
        let mut counts = std::collections::BTreeMap::<String, usize>::new();
        let mut after = String::new();
        loop {
            let page = self
                .get_rows(format!(
                    r#"{{ Get {{ {collection}(limit: {SCAN_PAGE}{after}) {{ owner identityId _additional {{ id }} }} }} }}"#,
                    collection = self.collection,
                ))
                .await?;
            page.iter()
                .filter(|item| item["owner"].as_str() == Some(owner.as_str()))
                .filter_map(|item| item["identityId"].as_str())
                .filter(|identity| !identity.is_empty())
                .for_each(|identity| *counts.entry(identity.to_string()).or_default() += 1);
            if page.len() < SCAN_PAGE {
                break;
            }
            let last = page
                .last()
                .and_then(|item| item["_additional"]["id"].as_str())
                .ok_or_else(|| eyre::eyre!("Weaviate page without an object id"))?;
            after = format!(r#", after: "{}""#, graphql_escape(last));
        }
        if let Some(previous) = self.previous.get() {
            for (identity, held) in previous.identity_counts(owner).await? {
                let count = counts.entry(identity).or_default();
                *count = (*count).max(held);
            }
        }
        Ok(counts)
    }

    async fn reassign_identity(&self, owner: &AgentId, from: &str, to: &str) -> Result<usize> {
        let moved = self
            .drain_identity(owner, from, |object| {
                self.client
                    .patch(self.object_url(object))
                    .json(&serde_json::json!({ "properties": { "identityId": to } }))
            })
            .await?;
        if let Some(previous) = self.previous.get() {
            previous.reassign_identity(owner, from, to).await?;
        }
        Ok(moved)
    }

    async fn purge_identity(&self, owner: &AgentId, identity: &str) -> Result<usize> {
        let purged = self
            .drain_identity(owner, identity, |object| {
                self.client.delete(self.object_url(object))
            })
            .await?;
        // Unlike `forget`, a failure here fails the purge: erasure that left
        // a rollback copy behind would not be erasure.
        if let Some(previous) = self.previous.get() {
            previous.purge_identity(owner, identity).await?;
        }
        Ok(purged)
    }

    async fn purge_memories(&self, ids: &[MemoryId]) -> Result<usize> {
        let collection = self.collection.as_str();
        let url = format!("{}/v1/graphql", self.base_url);
        let mut purged = 0;
        for id in ids {
            let graphql = format!(
                r#"{{ Get {{ {collection}(where: {{ path: ["memoryId"], operator: Equal, valueText: "{}" }}) {{ _additional {{ id }} }} }} }}"#,
                graphql_escape(&id.0)
            );
            let body: serde_json::Value = self
                .client
                .post(&url)
                .json(&serde_json::json!({ "query": graphql }))
                .send()
                .await?
                .json()
                .await?;
            Self::check_graphql_response(&body)?;
            let objects: Vec<String> = body["data"]["Get"][collection]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|item| item["_additional"]["id"].as_str().map(str::to_string))
                .collect();
            for object in &objects {
                let resp = self
                    .client
                    .delete(format!(
                        "{}/v1/objects/{collection}/{object}",
                        self.base_url
                    ))
                    .send()
                    .await?;
                if !resp.status().is_success() && resp.status() != reqwest::StatusCode::NOT_FOUND {
                    let body = resp.text().await.unwrap_or_default();
                    return Err(eyre::eyre!("Weaviate purge failed: {body}"));
                }
            }
            purged += usize::from(!objects.is_empty());
        }
        if let Some(previous) = self.previous.get() {
            previous.purge_memories(ids).await?;
        }
        Ok(purged)
    }

    async fn adjust_importance(&self, id: &MemoryId, delta: f32) -> Result<f32> {
        let collection = self.collection.as_str();
        // Weaviate has no atomic increment primitive on PATCH. We read, compute
        // the clamped target, and write — matching the lost-update tradeoff
        // already accepted by `record_recurrence` above. Concurrent positive +
        // negative reinforcement on the same lesson can therefore lose one
        // delta in a tight race; the residual drift is small and recoverable
        // through subsequent reinforcement events.
        use crate::store::IMPORTANCE_FLOOR;

        let lookup_graphql = format!(
            r#"{{ Get {{ {collection}(where: {{ path: ["memoryId"], operator: Equal, valueText: "{}" }}, limit: 1) {{ _additional {{ id }} importance }} }} }}"#,
            graphql_escape(&id.0)
        );
        let lookup_graphql = lookup_graphql
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");

        let url = format!("{}/v1/graphql", self.base_url);
        let resp = self
            .client
            .post(&url)
            .json(&serde_json::json!({ "query": lookup_graphql }))
            .send()
            .await?;
        let body: serde_json::Value = resp.json().await?;
        Self::check_graphql_response(&body)?;

        let item = &body["data"]["Get"][collection][0];
        let weaviate_id = item["_additional"]["id"]
            .as_str()
            .ok_or_else(|| eyre::eyre!("memory {} not found", id.0))?;
        let current = item["importance"].as_f64().unwrap_or(0.5) as f32;
        let new = (current + delta).clamp(IMPORTANCE_FLOOR, 1.0);

        let patch_url = format!("{}/v1/objects/{collection}/{}", self.base_url, weaviate_id);
        let resp = self
            .client
            .patch(&patch_url)
            .json(&serde_json::json!({ "properties": { "importance": new } }))
            .send()
            .await?;
        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(eyre::eyre!("Weaviate adjust_importance failed: {}", body));
        }
        Ok(new)
    }

    async fn record_recurrence(
        &self,
        id: &MemoryId,
        additional_message_ids: Vec<String>,
    ) -> Result<u32> {
        let collection = self.collection.as_str();
        // Look up Weaviate UUID + current state in one round-trip. We need the
        // current `recurrenceCount` and `sourceMessageIds` because Weaviate
        // PATCH does a property-level overwrite — there's no atomic increment
        // primitive. Concurrent recurrence writes therefore race; the cost of
        // a lost increment is bounded (off-by-one on a counter) and far
        // smaller than the cost of holding a global lock for every passive
        // extraction, so we accept it.
        let lookup_graphql = format!(
            r#"{{ Get {{ {collection}(where: {{ path: ["memoryId"], operator: Equal, valueText: "{}" }}, limit: 1) {{ _additional {{ id }} recurrenceCount sourceMessageIds }} }} }}"#,
            graphql_escape(&id.0)
        );
        let lookup_graphql = lookup_graphql
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");

        let url = format!("{}/v1/graphql", self.base_url);
        let resp = self
            .client
            .post(&url)
            .json(&serde_json::json!({ "query": lookup_graphql }))
            .send()
            .await?;
        let body: serde_json::Value = resp.json().await?;
        Self::check_graphql_response(&body)?;

        let item = &body["data"]["Get"][collection][0];
        let weaviate_id = item["_additional"]["id"]
            .as_str()
            .ok_or_else(|| eyre::eyre!("Memory '{}' not found in Weaviate", id.0))?;
        let current_count = item["recurrenceCount"].as_u64().unwrap_or(0) as u32;
        let mut existing_ids: Vec<String> = item["sourceMessageIds"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        for new_id in additional_message_ids {
            if !new_id.is_empty() && !existing_ids.contains(&new_id) {
                existing_ids.push(new_id);
            }
        }

        let new_count = current_count.saturating_add(1);
        let now = chrono::Utc::now().to_rfc3339();

        let patch_url = format!("{}/v1/objects/{collection}/{}", self.base_url, weaviate_id);
        let resp = self
            .client
            .patch(&patch_url)
            .json(&serde_json::json!({
                "properties": {
                    "recurrenceCount": new_count,
                    "lastRecurrenceAt": now,
                    "lastAccessed": now,
                    "sourceMessageIds": existing_ids,
                }
            }))
            .send()
            .await?;
        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(eyre::eyre!("Weaviate record_recurrence failed: {}", body));
        }

        debug!(
            memory_id = %id.0,
            recurrence_count = new_count,
            "Recorded memory recurrence"
        );
        Ok(new_count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::dedup_by_memory_id;
    use crate::types::MemoryLayer;

    #[cfg(unix)]
    #[test]
    fn a_memory_backup_is_readable_by_the_hub_alone() {
        use std::os::unix::fs::PermissionsExt;
        let other_bits = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o077;
        let data = tempfile::tempdir().unwrap();
        let dir = data.path().join("memory-backups");
        std::fs::create_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o2775)).unwrap();

        let backup =
            write_backup(&dir, &[serde_json::json!({"content": "a user's memory"})]).unwrap();

        assert_eq!(other_bits(&dir), 0);
        assert_eq!(other_bits(&backup), 0);
    }

    fn mock_result(id: &str) -> MemorySearchResult {
        MemorySearchResult {
            memory: Memory::new(
                format!("content-{id}"),
                MemoryLayer::Semantic,
                AgentId::new("agent"),
            )
            .with_tags(Vec::new()),
            score: 0.9,
        }
        .with_memory_id(id)
    }

    trait WithMemoryId {
        fn with_memory_id(self, id: &str) -> Self;
    }
    impl WithMemoryId for MemorySearchResult {
        fn with_memory_id(mut self, id: &str) -> Self {
            self.memory.id = MemoryId(id.to_string());
            self
        }
    }

    #[test]
    fn dedup_keeps_first_occurrence_and_drops_later_duplicates() {
        let results = vec![
            mock_result("a"),
            mock_result("b"),
            mock_result("a"), // dup
            mock_result("c"),
            mock_result("b"), // dup
        ];
        let deduped = dedup_by_memory_id(results);
        let ids: Vec<_> = deduped.iter().map(|r| r.memory.id.0.as_str()).collect();
        assert_eq!(ids, vec!["a", "b", "c"]);
    }

    #[test]
    fn dedup_preserves_single_copies() {
        let results = vec![mock_result("x"), mock_result("y"), mock_result("z")];
        let deduped = dedup_by_memory_id(results);
        assert_eq!(deduped.len(), 3);
    }

    #[test]
    fn where_filter_identity_only_matches_own_id_no_null_fallback() {
        let filter = MemoryFilter {
            identity_id: Some("user-123".to_string()),
            ..Default::default()
        };
        let w = WeaviateStore::build_where_filter(&filter).unwrap();
        assert!(
            w.contains(r#"valueText: "user-123""#),
            "missing identity equality branch: {w}"
        );
        // DW-519: the IS NULL fallback that previously leaked unowned rows
        // across users is gone.
        assert!(
            !w.contains("IsNull"),
            "IsNull fallback must not appear in identity scope: {w}"
        );
        assert!(
            !w.contains("visibility"),
            "team-less filter should not include visibility branch: {w}"
        );
    }

    #[test]
    fn where_filter_identity_with_team_unions_own_and_team_visibility() {
        let filter = MemoryFilter {
            identity_id: Some("user-abc".to_string()),
            team_id: Some("eng".to_string()),
            ..Default::default()
        };
        let w = WeaviateStore::build_where_filter(&filter).unwrap();
        assert!(w.contains(r#"valueText: "user-abc""#));
        assert!(w.contains(r#"valueText: "team:eng""#));
        assert!(
            !w.contains("IsNull"),
            "DW-519: team filter must not match unowned rows: {w}"
        );
    }

    #[test]
    fn where_filter_without_identity_has_no_identity_branch() {
        let filter = MemoryFilter::default();
        let w = WeaviateStore::build_where_filter(&filter).unwrap();
        assert!(!w.contains("identityId"), "{w}");
        assert!(w.contains(r#"path: ["deleted"]"#));
    }

    #[test]
    fn where_filter_positive_tags_single_tag_emits_equal_predicate() {
        let filter = MemoryFilter {
            identity_id: Some("user-1".to_string()),
            tags: Some(vec!["pinned".to_string()]),
            ..Default::default()
        };
        let w = WeaviateStore::build_where_filter(&filter).unwrap();
        assert!(
            w.contains(r#"path: ["tags"], operator: Equal, valueText: "pinned""#),
            "missing positive tag predicate: {w}"
        );
        assert!(
            w.contains(r#"valueText: "user-1""#),
            "identity scope must remain: {w}"
        );
    }

    #[test]
    fn where_filter_positive_tags_multiple_tags_unioned_with_or() {
        let filter = MemoryFilter {
            identity_id: Some("user-1".to_string()),
            tags: Some(vec!["pinned".to_string(), "lesson".to_string()]),
            ..Default::default()
        };
        let w = WeaviateStore::build_where_filter(&filter).unwrap();
        assert!(w.contains("operator: Or"), "expected OR over tags: {w}");
        assert!(w.contains(r#"valueText: "pinned""#), "{w}");
        assert!(w.contains(r#"valueText: "lesson""#), "{w}");
    }

    #[test]
    fn where_filter_positive_tags_empty_vec_emits_no_predicate() {
        let filter = MemoryFilter {
            identity_id: Some("user-1".to_string()),
            tags: Some(Vec::new()),
            ..Default::default()
        };
        let w = WeaviateStore::build_where_filter(&filter).unwrap();
        assert!(
            !w.contains(r#"path: ["tags"], operator: Equal"#),
            "empty tag vec must not emit a predicate: {w}"
        );
    }

    fn schema_json_from_expected() -> serde_json::Value {
        let properties: Vec<serde_json::Value> = EXPECTED_PROPERTIES
            .iter()
            .map(|(name, dtype, tok)| {
                let mut p = serde_json::json!({
                    "name": name,
                    "dataType": [dtype],
                });
                if let Some(t) = tok {
                    p["tokenization"] = serde_json::Value::String((*t).to_string());
                }
                p
            })
            .collect();
        serde_json::json!({
            "class": COLLECTION_NAME,
            "properties": properties,
            "replicationConfig": { "factor": 1 },
            "shardingConfig": { "desiredCount": 1 },
        })
    }

    #[test]
    fn validate_schema_shape_passes_for_correct_schema() {
        let store = WeaviateStore::new("http://localhost:0", None);
        let schema = schema_json_from_expected();
        store.validate_schema_shape(&schema).unwrap();
    }

    #[test]
    fn validate_schema_shape_returns_downcastable_mismatch_on_wrong_dtype() {
        let store = WeaviateStore::new("http://localhost:0", None);
        let mut schema = schema_json_from_expected();
        // Match the production failure mode: stale `memoryId` typed as uuid
        // instead of text.
        for prop in schema["properties"].as_array_mut().unwrap() {
            if prop["name"] == "memoryId" {
                prop["dataType"] = serde_json::json!(["uuid"]);
            }
        }
        let err = store.validate_schema_shape(&schema).unwrap_err();
        let mismatch = err
            .downcast_ref::<SchemaMismatchError>()
            .expect("error should downcast to SchemaMismatchError");
        assert_eq!(mismatch.class, COLLECTION_NAME);
        assert!(
            mismatch
                .mismatches
                .iter()
                .any(|m| m.contains("memoryId") && m.contains("uuid") && m.contains("text")),
            "mismatches: {:?}",
            mismatch.mismatches
        );
    }

    #[test]
    fn validate_schema_shape_returns_downcastable_mismatch_on_wrong_tokenization() {
        let store = WeaviateStore::new("http://localhost:0", None);
        let mut schema = schema_json_from_expected();
        for prop in schema["properties"].as_array_mut().unwrap() {
            if prop["name"] == "source" {
                prop["tokenization"] = serde_json::Value::String("word".to_string());
            }
        }
        let err = store.validate_schema_shape(&schema).unwrap_err();
        let mismatch = err
            .downcast_ref::<SchemaMismatchError>()
            .expect("error should downcast to SchemaMismatchError");
        assert!(
            mismatch
                .mismatches
                .iter()
                .any(|m| m.contains("source") && m.contains("tokenization")),
            "mismatches: {:?}",
            mismatch.mismatches
        );
    }

    #[test]
    fn fetch_limit_overfetches_only_for_tag_filters() {
        let no_tags = MemoryFilter::default();
        assert_eq!(fetch_limit_for(&no_tags, 50), 50);

        let tagged = MemoryFilter {
            tags: Some(vec!["date:2026-07-05".to_string()]),
            ..Default::default()
        };
        assert_eq!(fetch_limit_for(&tagged, 50), 150);
        // Capped, but never below the caller's own bound.
        assert_eq!(fetch_limit_for(&tagged, 400), 500);
        assert_eq!(fetch_limit_for(&tagged, 600), 600);

        let empty_tags = MemoryFilter {
            tags: Some(Vec::new()),
            ..Default::default()
        };
        assert_eq!(fetch_limit_for(&empty_tags, 50), 50);
    }

    /// Legacy collections word-tokenize `tags`, so `date:2026-07-05`
    /// cross-matches `date:2026-05-07` (identical token sets) and daily-notes
    /// prompts pull months-old memories. That shape is deliberately NOT
    /// treated as drift (a boot-time rebuild under start-first deploys would
    /// gut the live collection) — exactness comes from the
    /// `passes_required_tags` post-filter instead.
    #[test]
    fn validate_schema_shape_tolerates_word_tokenized_tags() {
        let store = WeaviateStore::new("http://localhost:0", None);
        let mut schema = schema_json_from_expected();
        for prop in schema["properties"].as_array_mut().unwrap() {
            if prop["name"] == "tags" {
                prop["tokenization"] = serde_json::Value::String("word".to_string());
            }
        }
        store.validate_schema_shape(&schema).unwrap();
    }

    use crate::types::Visibility;

    fn store_for_url(url: &str) -> WeaviateStore {
        WeaviateStore::new(url.to_string(), None)
    }

    fn memory_with_content(content: &str) -> Memory {
        Memory::new(
            content.to_string(),
            MemoryLayer::Semantic,
            AgentId::new("agent"),
        )
        .with_visibility(Visibility::Private)
    }

    #[tokio::test]
    async fn remember_rejects_empty_content_without_network_call() {
        let store = store_for_url("http://127.0.0.1:1");
        for content in ["", "   ", "\t\n", "!?!?", "🚀"] {
            let err = store
                .remember(memory_with_content(content))
                .await
                .expect_err(&format!("empty `{content}` must error before any HTTP"));
            assert!(
                err.to_string().contains("non-vectorizable"),
                "error string: {err}",
            );
        }
    }

    #[tokio::test]
    async fn recall_returns_empty_for_non_vectorizable_query_without_network_call() {
        let store = store_for_url("http://127.0.0.1:1");
        for query in ["", "   ", "!?", "🚀"] {
            let out = store
                .recall(query, MemoryFilter::default())
                .await
                .expect("recall must not error on blank/junk queries");
            assert!(out.is_empty(), "expected empty for {query:?}, got {out:?}");
        }
    }

    #[tokio::test]
    async fn import_raw_objects_filters_non_vectorizable_content() {
        let store = store_for_url("http://127.0.0.1:1");
        let inputs = vec![
            serde_json::json!({"properties": {"content": ""}}),
            serde_json::json!({"properties": {"content": "   "}}),
            serde_json::json!({"properties": {"content": "🚀"}}),
        ];
        let imported = store
            .import(&inputs)
            .await
            .expect("import filters bad records, returns Ok(0)");
        assert_eq!(imported, 0);
    }

    /// A collection exists when its schema reads, does not when Weaviate
    /// says it has none, and anything else is an error, never a "no".
    #[tokio::test]
    async fn a_collection_exists_when_its_schema_reads() {
        let app = axum::Router::new().route(
            "/v1/schema/{class}",
            axum::routing::get(
                |axum::extract::Path(class): axum::extract::Path<String>| async move {
                    match class.as_str() {
                        "Here" => axum::http::StatusCode::OK,
                        "Gone" => axum::http::StatusCode::NOT_FOUND,
                        _ => axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                    }
                },
            ),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let store = |class: &str| WeaviateStore::new(&url, None).in_collection(class);
        assert!(store("Here").collection_exists().await.unwrap());
        assert!(!store("Gone").collection_exists().await.unwrap());
        assert!(store("Broken").collection_exists().await.is_err());
    }

    /// An import that cannot store one object stores the others, each on
    /// its own once Weaviate refused their batch, and fails, saying which
    /// it could not store and why; one Weaviate cannot be reached for
    /// stores none and names them all.
    #[tokio::test]
    async fn an_import_stores_what_it_can_and_fails_naming_what_it_could_not() {
        use axum::response::IntoResponse;
        let stored = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let taken = std::sync::Arc::clone(&stored);
        let app = axum::Router::new().route(
            "/v1/batch/objects",
            axum::routing::post(move |axum::Json(sent): axum::Json<serde_json::Value>| {
                let taken = std::sync::Arc::clone(&taken);
                async move {
                    let objects = sent["objects"].as_array().cloned().unwrap_or_default();
                    // A batch with the bad one in it is refused whole.
                    let bad = |o: &serde_json::Value| o["properties"]["content"] == "bad";
                    if objects.len() > 1 && objects.iter().any(bad) {
                        let refused = axum::http::StatusCode::UNPROCESSABLE_ENTITY;
                        return (refused, "one of these will not do").into_response();
                    }
                    let results: Vec<serde_json::Value> = objects
                        .iter()
                        .map(|o| match bad(o) {
                            true => serde_json::json!({ "result": {
                                "status": "FAILED",
                                "errors": { "error": [{ "message": "it will not do" }] }
                            }}),
                            false => {
                                let id = o["id"].as_str().unwrap().to_string();
                                taken.lock().unwrap().push(id);
                                serde_json::json!({ "result": { "status": "SUCCESS" } })
                            }
                        })
                        .collect();
                    axum::Json(results).into_response()
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let object = |n: u128, content: &str| {
            serde_json::json!({
                "id": uuid::Uuid::from_u128(n).to_string(),
                "properties": { "content": content },
                "vector": [0.5, 0.5],
            })
        };
        let objects = [object(1, "one"), object(2, "bad"), object(3, "three")];
        let failed = store_for_url(&url).import(&objects).await.unwrap_err();
        let short = failed
            .downcast_ref::<crate::admin::ImportShort>()
            .expect("which, and why");
        assert_eq!(short.stored, 2);
        assert_eq!(short.failed.len(), 1, "{short}");
        assert_eq!(short.failed[0].0, objects[1]["id"].as_str().unwrap());
        assert!(short.failed[0].1.contains("it will not do"), "{short}");
        let mut stored = stored.lock().unwrap().clone();
        stored.sort();
        assert_eq!(
            stored,
            [
                objects[0]["id"].as_str().unwrap(),
                objects[2]["id"].as_str().unwrap()
            ]
        );

        let unreachable = store_for_url("http://127.0.0.1:1");
        let failed = unreachable.import(&objects).await.unwrap_err();
        let short = failed.downcast_ref::<crate::admin::ImportShort>().unwrap();
        assert_eq!((short.stored, short.failed.len()), (0, 3));
    }

    /// Weaviate answers a batch with a result for each object. An answer
    /// with fewer says nothing of the rest, which are not taken for
    /// stored: each is sent again on its own, and counted only when
    /// Weaviate says it has it.
    #[tokio::test]
    async fn an_answer_with_fewer_results_than_objects_stores_none_on_its_word() {
        use axum::response::IntoResponse;
        use std::sync::atomic::{AtomicBool, Ordering};
        let stored = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        // Whether an object sent on its own is answered for.
        let alone = std::sync::Arc::new(AtomicBool::new(true));
        let (taken, answered) = (
            std::sync::Arc::clone(&stored),
            std::sync::Arc::clone(&alone),
        );
        let app = axum::Router::new().route(
            "/v1/batch/objects",
            axum::routing::post(move |axum::Json(sent): axum::Json<serde_json::Value>| {
                let taken = std::sync::Arc::clone(&taken);
                let alone = answered.load(Ordering::SeqCst);
                async move {
                    let objects = sent["objects"].as_array().cloned().unwrap_or_default();
                    // Of several, the last is neither stored nor answered
                    // for.
                    let kept = match objects.len() {
                        1 if alone => 1,
                        several => several - 1,
                    };
                    let results: Vec<serde_json::Value> = objects[..kept]
                        .iter()
                        .map(|o| {
                            let id = o["id"].as_str().unwrap().to_string();
                            taken.lock().unwrap().push(id);
                            serde_json::json!({ "result": { "status": "SUCCESS" } })
                        })
                        .collect();
                    axum::Json(results).into_response()
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let objects: Vec<serde_json::Value> = (1..=3u128)
            .map(|n| {
                serde_json::json!({
                    "id": uuid::Uuid::from_u128(n).to_string(),
                    "properties": { "content": format!("memory {n}") },
                    "vector": [0.5, 0.5],
                })
            })
            .collect();
        let ids = || {
            objects
                .iter()
                .map(|o| o["id"].as_str().unwrap().to_string())
        };
        let store = store_for_url(&url);
        assert_eq!(store.import(&objects).await.unwrap(), 3);
        let mut held = stored.lock().unwrap().clone();
        held.sort();
        held.dedup();
        assert_eq!(held, ids().collect::<Vec<_>>(), "each one stored");

        // Not answered for on its own either: not stored, and said so.
        alone.store(false, Ordering::SeqCst);
        stored.lock().unwrap().clear();
        let failed = store.import(&objects[..1]).await.unwrap_err();
        let short = failed
            .downcast_ref::<crate::admin::ImportShort>()
            .expect("which, and why");
        assert_eq!((short.stored, short.failed.len()), (0, 1), "{short}");
        assert_eq!(short.failed[0].0, ids().next().unwrap());
        assert!(short.failed[0].1.contains("0 of the 1"), "{short}");
        assert!(stored.lock().unwrap().is_empty());
    }

    #[test]
    fn prepare_import_object_preserves_id_and_vector() {
        let exported = serde_json::json!({
            "id": "68811dd2-eb2d-4989-8244-25e3dcfdc690",
            "class": COLLECTION_NAME,
            "properties": { "content": "a memory", "identityId": "user-1" },
            "vector": [0.1, -0.2, 0.3],
        });
        let out = prepare_import_object(&exported, COLLECTION_NAME).expect("valid object");
        assert_eq!(out["class"], COLLECTION_NAME);
        assert_eq!(out["id"], "68811dd2-eb2d-4989-8244-25e3dcfdc690");
        assert_eq!(out["vector"], serde_json::json!([0.1, -0.2, 0.3]));
        assert_eq!(out["properties"]["content"], "a memory");
        assert_eq!(out["properties"]["identityId"], "user-1");
    }

    #[test]
    fn prepare_import_object_tolerates_missing_id_and_vector() {
        let exported = serde_json::json!({
            "properties": { "content": "vectorless legacy backup row" },
        });
        let out = prepare_import_object(&exported, COLLECTION_NAME).expect("valid object");
        assert!(out.get("id").is_none(), "no id key should be invented");
        assert!(
            out.get("vector").is_none(),
            "no vector key should be invented"
        );
    }

    #[test]
    fn prepare_import_object_drops_garbage_and_nulls_empty_identity() {
        for content in ["", "   ", "🚀"] {
            let bad = serde_json::json!({"properties": {"content": content}});
            assert!(
                prepare_import_object(&bad, COLLECTION_NAME).is_none(),
                "{content:?}"
            );
        }
        assert!(prepare_import_object(&serde_json::json!({}), COLLECTION_NAME).is_none());

        let empty_identity = serde_json::json!({
            "properties": { "content": "ok", "identityId": "" },
        });
        let out = prepare_import_object(&empty_identity, COLLECTION_NAME).unwrap();
        assert!(out["properties"]["identityId"].is_null());
    }

    /// End-to-end rebuild drill against a REAL scratch Weaviate. Ignored in
    /// CI — run manually before trusting a live schema migration:
    ///
    /// ```sh
    /// docker run -d --name weaviate-e2e -p 18080:8080 \
    ///   -e AUTHENTICATION_ANONYMOUS_ACCESS_ENABLED=true \
    ///   -e ENABLE_MODULES=text2vec-transformers \
    ///   -e TRANSFORMERS_INFERENCE_API=http://127.0.0.1:9 \
    ///   -e DEFAULT_VECTORIZER_MODULE=text2vec-transformers \
    ///   -e PERSISTENCE_DATA_PATH=/var/lib/weaviate \
    ///   cr.weaviate.io/semitechnologies/weaviate:1.28.4
    /// WEAVIATE_E2E_URL=http://localhost:18080 \
    ///   cargo test -p workforce-memory rebuild_e2e -- --ignored --nocapture
    /// ```
    ///
    /// Seeds a legacy-shaped class (word-tokenized `tags` + word-tokenized
    /// `source`, the latter being real schema drift), inserts objects with
    /// explicit vectors, runs `ensure_collection_with_recovery`, then asserts
    /// the rebuild preserved every object AND its vector byte-for-byte
    /// (proving no re-embedding — the inference API above is unreachable, so
    /// any vectorizer call would fail loudly) and that `tags` came back
    /// `field`-tokenized with DB-level exact date-tag matching.
    #[tokio::test]
    #[ignore = "needs a live scratch Weaviate; see doc comment"]
    async fn rebuild_e2e_preserves_vectors_and_fixes_tag_tokenization() {
        let Ok(url) = std::env::var("WEAVIATE_E2E_URL") else {
            panic!("set WEAVIATE_E2E_URL to the scratch Weaviate base URL");
        };
        let http = reqwest::Client::new();
        let store = WeaviateStore::new(url.clone(), None);

        // Clean slate, then create the LEGACY class shape: same properties as
        // production-before-the-fix — `source` and `tags` word-tokenized.
        // Word-tokenized `source` is validated drift, so recovery rebuilds.
        let _ = store.delete_collection().await;
        let mut legacy = serde_json::json!({
            "class": COLLECTION_NAME,
            "vectorizer": "text2vec-transformers",
            "replicationConfig": { "factor": 1 },
            "shardingConfig": { "desiredCount": 1 },
            "properties": EXPECTED_PROPERTIES.iter().map(|(name, dtype, _)| {
                serde_json::json!({ "name": name, "dataType": [dtype] })
            }).collect::<Vec<_>>(),
        });
        for prop in legacy["properties"].as_array_mut().unwrap() {
            // Everything except source/tags keeps its expected tokenization
            // so the ONLY drift is the one under test.
            let name = prop["name"].as_str().unwrap();
            if let Some((_, _, Some(tok))) = EXPECTED_PROPERTIES.iter().find(|(n, _, _)| *n == name)
            {
                if name != "source" {
                    prop["tokenization"] = serde_json::json!(tok);
                }
            }
        }
        let resp = http
            .post(format!("{url}/v1/schema"))
            .json(&legacy)
            .send()
            .await
            .expect("create legacy class");
        assert!(resp.status().is_success(), "{}", resp.text().await.unwrap());

        // Seed objects with explicit 4-dim vectors: half tagged May 7th, half
        // July 5th — the transposed pair whose tokens collide.
        let seed_count = 250usize;
        let mut seeded: Vec<serde_json::Value> = Vec::new();
        for i in 0..seed_count {
            let date = if i % 2 == 0 {
                "2026-05-07"
            } else {
                "2026-07-05"
            };
            seeded.push(serde_json::json!({
                "class": COLLECTION_NAME,
                "id": uuid::Uuid::new_v4().to_string(),
                "properties": {
                    "content": format!("seed memory {i}"),
                    "memoryId": format!("mem-{i}"),
                    "layer": "episodic",
                    "owner": "allan",
                    "visibility": "private",
                    "importance": 0.5,
                    "tags": [format!("date:{date}"), "source:passive"],
                    "deleted": false,
                    "accessCount": 0,
                    "createdAt": "2026-05-10T00:00:00Z",
                    "lastAccessed": "2026-05-10T00:00:00Z",
                    "source": "passive",
                    "identityId": "user-1",
                    "recurrenceCount": 0,
                    "eventTime": "2026-05-07T20:00:00Z",
                },
                "vector": [i as f32, (i % 7) as f32, 0.5, -1.0],
            }));
        }
        for chunk in seeded.chunks(100) {
            let resp = http
                .post(format!("{url}/v1/batch/objects"))
                .json(&serde_json::json!({ "objects": chunk }))
                .send()
                .await
                .expect("seed batch");
            assert!(resp.status().is_success());
        }

        // Sanity: legacy shape cross-matches transposed date tags at DB level.
        let count_for_tag = |tag: &str| {
            let http = http.clone();
            let url = url.clone();
            let tag = tag.to_string();
            async move {
                let q = format!(
                    r#"{{ Get {{ {COLLECTION_NAME}(where: {{ path: ["tags"], operator: Equal, valueText: "{tag}" }}, limit: 1000) {{ memoryId }} }} }}"#
                );
                let body: serde_json::Value = http
                    .post(format!("{url}/v1/graphql"))
                    .json(&serde_json::json!({ "query": q }))
                    .send()
                    .await
                    .unwrap()
                    .json()
                    .await
                    .unwrap();
                body["data"]["Get"][COLLECTION_NAME]
                    .as_array()
                    .map(|a| a.len())
                    .unwrap_or(0)
            }
        };
        assert_eq!(
            count_for_tag("date:2026-07-05").await,
            seed_count,
            "legacy word tokenization should cross-match BOTH date groups"
        );

        // The drill: recovery must export → recreate → re-import without a
        // reachable vectorizer, in seconds.
        let started = std::time::Instant::now();
        let dir = tempfile::tempdir().unwrap();
        let outcome = store
            .ensure_collection_with_recovery(Some(dir.path()))
            .await
            .expect("recovery should rebuild");
        let elapsed = started.elapsed();
        match outcome {
            RecoveryOutcome::Rebuilt {
                exported, imported, ..
            } => {
                assert_eq!(exported, seed_count);
                assert_eq!(imported, seed_count, "no object may be lost");
            }
            other => panic!("expected Rebuilt, got {other:?}"),
        }
        println!("rebuild of {seed_count} objects took {elapsed:?}");
        assert!(
            elapsed < std::time::Duration::from_secs(60),
            "rebuild too slow for a live migration: {elapsed:?}"
        );

        // Tags are now field-tokenized and exact at the DB level.
        let schema: serde_json::Value = http
            .get(format!("{url}/v1/schema/{COLLECTION_NAME}"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let tags_tok = schema["properties"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["name"] == "tags")
            .and_then(|p| p["tokenization"].as_str())
            .unwrap_or("<unset>");
        assert_eq!(tags_tok, "field");
        assert_eq!(
            count_for_tag("date:2026-07-05").await,
            seed_count / 2,
            "rebuilt collection must match date tags exactly"
        );

        // Vectors survived byte-for-byte (fetch one object back by memoryId).
        let q = format!(
            r#"{{ Get {{ {COLLECTION_NAME}(where: {{ path: ["memoryId"], operator: Equal, valueText: "mem-3" }}, limit: 1) {{ memoryId _additional {{ vector }} }} }} }}"#
        );
        let body: serde_json::Value = http
            .post(format!("{url}/v1/graphql"))
            .json(&serde_json::json!({ "query": q }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let vector = body["data"]["Get"][COLLECTION_NAME][0]["_additional"]["vector"]
            .as_array()
            .expect("object must have a vector after rebuild")
            .iter()
            .map(|v| v.as_f64().unwrap() as f32)
            .collect::<Vec<f32>>();
        assert_eq!(vector, vec![3.0, 3.0, 0.5, -1.0]);

        let _ = store.delete_collection().await;
    }

    #[test]
    fn build_get_by_id_query_uses_where_not_near_text() {
        let q = build_get_by_id_query(&MemoryId("abc-123".to_string()), COLLECTION_NAME);
        assert!(q.contains("where:"), "missing where clause: {q}");
        assert!(q.contains(r#"path: ["memoryId"]"#), "no memoryId path: {q}");
        assert!(q.contains(r#"valueText: "abc-123""#), "id not bound: {q}");
        assert!(q.contains(r#"path: ["deleted"]"#), "no deleted path: {q}");
        assert!(q.contains("valueBoolean: false"), "deleted not false: {q}");
        assert!(q.contains("limit: 1"), "missing limit:1 — {q}");
        assert!(!q.contains("nearText"), "must not vector-search: {q}");
        assert!(!q.contains("concepts"), "must not vector-search: {q}");
    }

    #[test]
    fn build_get_by_id_query_escapes_double_quotes() {
        let q = build_get_by_id_query(&MemoryId(r#"a"b"#.to_string()), COLLECTION_NAME);
        assert!(q.contains(r#"valueText: "a\"b""#), "unescaped quote: {q}");
    }

    #[test]
    fn build_get_by_id_query_requests_full_memory_property_set() {
        let q = build_get_by_id_query(&MemoryId("x".to_string()), COLLECTION_NAME);
        for field in [
            "content",
            "memoryId",
            "layer",
            "owner",
            "visibility",
            "importance",
            "tags",
            "createdAt",
            "lastAccessed",
            "source",
            "identityId",
            "sourceSessionId",
            "sourceMessageIds",
            "recurrenceCount",
            "lastRecurrenceAt",
            "eventTime",
        ] {
            assert!(q.contains(field), "missing property `{field}` in {q}");
        }
    }

    #[test]
    fn decode_memory_round_trips_full_payload() {
        let payload = serde_json::json!({
            "content": "hello",
            "memoryId": "m-1",
            "layer": "semantic",
            "owner": "agent-alpha",
            "visibility": "team:eng",
            "importance": 0.87,
            "tags": ["pinned", "lesson"],
            "accessCount": 4,
            "createdAt": "2026-05-20T10:30:00Z",
            "lastAccessed": "2026-05-21T11:00:00Z",
            "source": "human",
            "identityId": "user-7",
            "sourceSessionId": "sess-9",
            "sourceMessageIds": ["msg-1", "msg-2"],
            "recurrenceCount": 2,
            "lastRecurrenceAt": "2026-05-22T09:00:00Z",
            "eventTime": "2026-05-19T08:00:00Z",
        });
        let mem = decode_memory(&payload).expect("decode must succeed");
        assert_eq!(mem.id.0, "m-1");
        assert_eq!(mem.content, "hello");
        assert!(matches!(mem.layer, MemoryLayer::Semantic));
        assert_eq!(mem.owner.as_str(), "agent-alpha");
        assert_eq!(mem.visibility, Visibility::Team("eng".to_string()));
        assert!((mem.importance - 0.87).abs() < 1e-5);
        assert_eq!(mem.tags, vec!["pinned", "lesson"]);
        assert_eq!(mem.access_count, 4);
        assert_eq!(mem.identity_id.as_deref(), Some("user-7"));
        assert_eq!(mem.source_session_id.as_deref(), Some("sess-9"));
        assert_eq!(mem.source_message_ids, vec!["msg-1", "msg-2"]);
        assert_eq!(mem.recurrence_count, 2);
        assert!(mem.last_recurrence_at.is_some());
        assert_eq!(
            mem.event_time.to_rfc3339(),
            "2026-05-19T08:00:00+00:00",
            "event_time must be honoured when present"
        );
    }

    #[test]
    fn decode_memory_returns_none_when_content_or_memory_id_missing() {
        let base = serde_json::json!({
            "content": "x",
            "memoryId": "m-1",
        });

        let mut without_content = base.clone();
        without_content.as_object_mut().unwrap().remove("content");
        assert!(decode_memory(&without_content).is_none());

        let mut without_id = base;
        without_id.as_object_mut().unwrap().remove("memoryId");
        assert!(decode_memory(&without_id).is_none());
    }

    #[test]
    fn decode_memory_event_time_falls_back_to_created_at_for_legacy_rows() {
        let payload = serde_json::json!({
            "content": "legacy",
            "memoryId": "m-legacy",
            "createdAt": "2026-01-01T00:00:00Z",
        });
        let mem = decode_memory(&payload).expect("decode must succeed");
        assert_eq!(mem.event_time, mem.created_at);
    }

    #[tokio::test]
    async fn get_actually_attempts_http_round_trip() {
        // Regression for the pre-fix Weaviate `get` that proxied through
        // `recall("")` and returned `Ok(None)` without ever leaving the
        // process once `VectorizableText` started rejecting empty queries.
        let store = store_for_url("http://127.0.0.1:1");
        let _err = store
            .get(&MemoryId("any-id".to_string()))
            .await
            .expect_err("must attempt HTTP, not short-circuit to Ok(None)");
    }

    #[test]
    fn build_list_query_uses_where_not_near_text() {
        let filter = MemoryFilter {
            identity_id: Some("user-1".to_string()),
            tags: Some(vec!["pinned".to_string()]),
            ..Default::default()
        };
        let q = build_list_query(&filter, 25, COLLECTION_NAME);
        assert!(q.contains("limit: 25"), "missing limit: {q}");
        assert!(q.contains("where:"), "missing where: {q}");
        assert!(
            q.contains(r#"valueText: "user-1""#),
            "identity unbound: {q}"
        );
        assert!(!q.contains("nearText"), "must not vector-search: {q}");
        assert!(!q.contains("concepts"), "must not vector-search: {q}");
        assert!(!q.contains("_additional"), "no certainty needed: {q}");
        for field in [
            "content",
            "memoryId",
            "layer",
            "owner",
            "visibility",
            "importance",
            "tags",
            "createdAt",
            "lastAccessed",
            "source",
            "identityId",
            "sourceSessionId",
            "sourceMessageIds",
            "recurrenceCount",
            "lastRecurrenceAt",
            "eventTime",
        ] {
            assert!(q.contains(field), "missing property `{field}` in {q}");
        }
    }

    #[tokio::test]
    async fn list_attempts_http_round_trip_for_scoped_filter() {
        // Regression: pre-fix `list` proxied through `recall("")` and silently
        // returned an empty Vec, breaking the pinned-memory prompt injection
        // path at `serve.rs:4793` for any Weaviate-backed deployment.
        let store = store_for_url("http://127.0.0.1:1");
        let filter = MemoryFilter {
            identity_id: Some("user-1".to_string()),
            tags: Some(vec!["pinned".to_string()]),
            ..Default::default()
        };
        let _err = store
            .list(filter)
            .await
            .expect_err("must attempt HTTP, not short-circuit to Ok(vec![])");
    }

    #[tokio::test]
    async fn list_returns_empty_without_network_call_when_identity_unscoped() {
        let store = store_for_url("http://127.0.0.1:1");
        let out = store
            .list(MemoryFilter::default())
            .await
            .expect("must short-circuit at the scope gate, not error");
        assert!(out.is_empty());
    }

    #[test]
    fn graphql_escape_handles_backslash_quote_and_controls() {
        let bs = '\u{5c}'; // literal backslash
        assert_eq!(graphql_escape("plain"), "plain");
        assert_eq!(graphql_escape("say \"hi\""), format!("say {bs}\"hi{bs}\""));
        // Backslash escaped first, so a pre-escaped-looking input stays literal.
        assert_eq!(graphql_escape(&format!("a{bs}b")), format!("a{bs}{bs}b"));
        // Control characters become uXXXX escapes - GraphQL forbids them raw.
        assert_eq!(graphql_escape("a\u{1F}b"), format!("a{bs}u001Fb"));
        assert_eq!(graphql_escape("a\nb"), format!("a{bs}u000Ab"));
    }

    #[test]
    fn where_filter_escapes_control_chars_in_identity_id() {
        // Regression: the edge's external identity ids join issuer and subject
        // with a unit separator (U+001F); unescaped, it made Weaviate reject
        // every scoped recall with "Invalid character within String" - memory
        // silently broken for all edge customers.
        let filter = MemoryFilter {
            identity_id: Some("ext:https://accounts.google.com\u{1F}12345".to_string()),
            ..Default::default()
        };
        let clause = WeaviateStore::build_where_filter(&filter).expect("filter built");
        assert!(
            !clause.contains('\u{1F}'),
            "raw control char must not appear"
        );
        let bs = '\u{5c}';
        assert!(
            clause.contains(&format!("accounts.google.com{bs}u001F12345")),
            "got: {clause}"
        );
    }
}

#[cfg(test)]
mod embedded_tests {
    //! The hub-vectored collection against a fake Weaviate that records what
    //! it is sent.
    use super::*;
    use crate::embedding::{EmbedKind, EmbeddingBackend, Reranker};
    use crate::types::MemoryLayer;
    use axum::{extract::State, routing::post, Json, Router};
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct Fake {
        sent: Mutex<Vec<(String, serde_json::Value)>>,
        get_rows: Mutex<Vec<serde_json::Value>>,
        objects: Mutex<Vec<serde_json::Value>>,
        /// Ids whose REST read answers 500, as a Weaviate under strain does.
        failing_reads: Mutex<Vec<String>>,
        /// When set, a `Get` reads `objects` the way Weaviate does, capped at
        /// this many results per query, instead of answering `get_rows`.
        live: Option<Live>,
    }

    struct Live {
        max_results: usize,
        /// An `identityId` the index also matches for one that only starts
        /// with it, as a word-tokenized index might.
        loose: bool,
    }

    impl Fake {
        fn live(max_results: usize, loose: bool) -> Self {
            Self {
                live: Some(Live { max_results, loose }),
                ..Self::default()
            }
        }

        /// One object of `owner`'s about `identity`, as Weaviate stores it.
        fn hold(&self, id: &str, owner: &str, identity: &str) {
            self.objects.lock().unwrap().push(serde_json::json!({
                "class": "MemV2",
                "id": id,
                "properties": { "memoryId": id, "owner": owner, "identityId": identity },
            }));
        }

        fn ids(&self) -> Vec<String> {
            self.objects
                .lock()
                .unwrap()
                .iter()
                .map(|o| o["id"].as_str().unwrap().to_string())
                .collect()
        }

        fn get(&self, live: &Live, query: &str) -> serde_json::Value {
            let number = |key: &str| {
                query.split_once(&format!("{key}: ")).map(|(_, rest)| {
                    rest.chars()
                        .take_while(char::is_ascii_digit)
                        .collect::<String>()
                        .parse::<usize>()
                        .unwrap()
                })
            };
            let (limit, offset) = (number("limit").unwrap(), number("offset").unwrap_or(0));
            if offset + limit > live.max_results {
                return serde_json::json!({ "errors": [{ "message": "query maximum results exceeded" }] });
            }
            let after = query
                .split_once(r#"after: ""#)
                .map(|(_, rest)| rest.split('"').next().unwrap().to_string());
            let operand = |path: &str, value: &str| {
                format!(r#"path: ["{path}"], operator: Equal, valueText: "{value}""#)
            };
            let matches = |p: &serde_json::Value| {
                let field = |name: &str| p[name].as_str().unwrap_or_default().to_string();
                let identity = field("identityId");
                let identity_matches = (0..=identity.len())
                    .filter(|&end| identity.is_char_boundary(end))
                    .filter(|&end| live.loose || end == identity.len())
                    .any(|end| {
                        query.contains(&operand("identityId", &graphql_escape(&identity[..end])))
                    });
                !query.contains("where:")
                    || (query.contains(&operand("owner", &graphql_escape(&field("owner"))))
                        && identity_matches)
                    || query.contains(&operand("memoryId", &graphql_escape(&field("memoryId"))))
            };
            let objects = self.objects.lock().unwrap();
            let start = after.map_or(0, |after| {
                objects
                    .iter()
                    .position(|o| o["id"] == after.as_str())
                    .unwrap()
                    + 1
            });
            let rows: Vec<_> = objects[start..]
                .iter()
                .filter(|o| matches(&o["properties"]))
                .skip(offset)
                .take(limit)
                .map(|o| {
                    let mut row = o["properties"].clone();
                    row["_additional"] = serde_json::json!({ "id": o["id"] });
                    row
                })
                .collect();
            let class = if query.contains("MemV2") {
                "MemV2"
            } else {
                "WorkforceMemory"
            };
            serde_json::json!({ "data": { "Get": { class: rows } } })
        }
    }

    async fn record(
        State(f): State<Arc<Fake>>,
        path: axum::http::Uri,
        Json(body): Json<serde_json::Value>,
    ) -> Json<serde_json::Value> {
        f.sent
            .lock()
            .unwrap()
            .push((path.path().to_string(), body.clone()));
        match path.path() {
            "/v1/graphql" if f.live.is_some() => Json(f.get(
                f.live.as_ref().unwrap(),
                body["query"].as_str().unwrap_or_default(),
            )),
            "/v1/graphql" => {
                let rows = f.get_rows.lock().unwrap().clone();
                let class = if body["query"].as_str().unwrap_or("").contains("MemV2") {
                    "MemV2"
                } else {
                    "WorkforceMemory"
                };
                Json(serde_json::json!({ "data": { "Get": { class: rows } } }))
            }
            "/v1/batch/objects" => {
                let n = body["objects"].as_array().map_or(0, Vec::len);
                f.objects
                    .lock()
                    .unwrap()
                    .extend(body["objects"].as_array().cloned().unwrap_or_default());
                Json(serde_json::json!((0..n)
                    .map(|_| serde_json::json!({"result": {"status": "SUCCESS"}}))
                    .collect::<Vec<_>>()))
            }
            _ => Json(serde_json::json!({})),
        }
    }

    async fn list(
        State(f): State<Arc<Fake>>,
        q: axum::extract::Query<std::collections::HashMap<String, String>>,
    ) -> Json<serde_json::Value> {
        // As Weaviate's cursor: in id order, after the id given, a page of
        // `limit` at a time.
        let class = q.get("class").cloned().unwrap_or_default();
        let after = q.get("after").cloned().unwrap_or_default();
        let limit = q
            .get("limit")
            .and_then(|limit| limit.parse().ok())
            .unwrap_or(25);
        let mut objs: Vec<serde_json::Value> = f
            .objects
            .lock()
            .unwrap()
            .iter()
            .filter(|o| o["class"] == class && o["id"].as_str() > Some(after.as_str()))
            .cloned()
            .collect();
        objs.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));
        objs.truncate(limit);
        Json(serde_json::json!({ "objects": objs }))
    }

    async fn patch_object(
        State(f): State<Arc<Fake>>,
        axum::extract::Path((class, id)): axum::extract::Path<(String, String)>,
        Json(body): Json<serde_json::Value>,
    ) -> Json<serde_json::Value> {
        for o in f.objects.lock().unwrap().iter_mut() {
            if o["class"] == class.as_str() && o["id"] == id.as_str() {
                for (k, v) in body["properties"].as_object().unwrap() {
                    o["properties"][k] = v.clone();
                }
            }
        }
        Json(serde_json::json!({}))
    }

    async fn delete_object(
        State(f): State<Arc<Fake>>,
        axum::extract::Path((class, id)): axum::extract::Path<(String, String)>,
    ) -> axum::http::StatusCode {
        f.objects
            .lock()
            .unwrap()
            .retain(|o| !(o["class"] == class.as_str() && o["id"] == id.as_str()));
        axum::http::StatusCode::NO_CONTENT
    }

    async fn get_object(
        State(f): State<Arc<Fake>>,
        axum::extract::Path((class, id)): axum::extract::Path<(String, String)>,
        q: axum::extract::Query<std::collections::HashMap<String, String>>,
    ) -> (axum::http::StatusCode, Json<serde_json::Value>) {
        if f.failing_reads.lock().unwrap().contains(&id) {
            return (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({})),
            );
        }
        let found = f
            .objects
            .lock()
            .unwrap()
            .iter()
            .find(|o| o["class"] == class.as_str() && o["id"] == id.as_str())
            .cloned();
        match found {
            Some(mut object) => {
                if q.get("include").map(String::as_str) != Some("vector") {
                    object.as_object_mut().map(|o| o.remove("vector"));
                }
                (axum::http::StatusCode::OK, Json(object))
            }
            None => (
                axum::http::StatusCode::NOT_FOUND,
                Json(serde_json::json!({})),
            ),
        }
    }

    async fn serve(fake: Arc<Fake>) -> String {
        let app = Router::new()
            .route("/v1/graphql", post(record))
            .route("/v1/objects", post(record).get(list))
            .route(
                "/v1/objects/{class}/{id}",
                axum::routing::patch(patch_object)
                    .delete(delete_object)
                    .get(get_object),
            )
            .route("/v1/batch/objects", post(record))
            .with_state(fake);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}")
    }

    /// Vectors from text: [length, count of 'a'], enough to tell texts apart.
    struct LenEmbedder {
        fail: bool,
    }

    #[async_trait::async_trait]
    impl EmbeddingBackend for LenEmbedder {
        async fn embed(&self, text: &str) -> Result<Vec<f32>> {
            if self.fail {
                return Err(eyre::eyre!("down"));
            }
            Ok(vec![text.len() as f32, text.matches('a').count() as f32])
        }
        async fn embed_many(&self, texts: &[String], _kind: EmbedKind) -> Result<Vec<Vec<f32>>> {
            let mut out = Vec::new();
            for t in texts {
                out.push(self.embed(t).await?);
            }
            Ok(out)
        }
    }

    struct Reverse;

    #[async_trait::async_trait]
    impl Reranker for Reverse {
        async fn rerank(&self, _q: &str, texts: &[String]) -> Result<Vec<(usize, f32)>> {
            Ok((0..texts.len())
                .rev()
                .map(|i| (i, 0.9 - i as f32 * 0.0))
                .enumerate()
                .map(|(rank, (i, _))| (i, 0.9 - rank as f32 * 0.1))
                .collect())
        }
    }

    fn store(url: &str, fail: bool, rerank: bool) -> WeaviateStore {
        WeaviateStore::new(url, None).with_embeddings(
            "MemV2",
            Arc::new(LenEmbedder { fail }),
            rerank.then(|| Arc::new(Reverse) as Arc<dyn Reranker>),
            0.75,
            30,
        )
    }

    fn row(content: &str, vector: Vec<f32>) -> serde_json::Value {
        serde_json::json!({
            "content": content, "memoryId": content, "layer": "semantic", "owner": "agent", "visibility": "private",
            "importance": 0.5, "tags": [], "deleted": false, "accessCount": 0,
            "createdAt": "2026-01-01T00:00:00Z", "lastAccessed": "2026-01-01T00:00:00Z", "source": "human",
            "identityId": null, "eventTime": "2026-01-01T00:00:00Z",
            "_additional": { "vector": vector }
        })
    }

    /// A system-scoped filter: recall refuses an unscoped one before querying.
    fn unscoped() -> MemoryFilter {
        MemoryFilter {
            limit: Some(5),
            system_unscoped: true,
            ..Default::default()
        }
    }

    fn graphql_queries(fake: &Fake) -> Vec<String> {
        fake.sent
            .lock()
            .unwrap()
            .iter()
            .filter(|(path, _)| path == "/v1/graphql")
            .map(|(_, body)| body["query"].as_str().unwrap().to_string())
            .collect()
    }

    /// Every object holding a memory is read, past one query's limit, and
    /// read whole, as the export reads it: a property a GraphQL field list
    /// left out would be taken off the copy, and put back by the next
    /// reconcile, for ever.
    #[tokio::test]
    async fn a_memorys_objects_are_read_past_one_querys_limit_and_whole() {
        let fake = Arc::new(Fake::live(10_000, false));
        let object = |n: usize, memory: &str| {
            serde_json::json!({
                "class": "MemV2",
                "id": format!("00000000-0000-0000-0000-{n:012}"),
                "properties": {
                    "memoryId": memory, "content": format!("note {n}"), "owner": "agent",
                    "aPropertyNoQueryNames": n, "tags": ["pinned"],
                },
                "vector": [n as f32, 1.0],
            })
        };
        let held: Vec<serde_json::Value> = (0..250).map(|n| object(n, "m1")).collect();
        fake.objects.lock().unwrap().extend(held.clone());
        fake.objects.lock().unwrap().push(object(250, "m2"));
        let store = store(&serve(Arc::clone(&fake)).await, false, false);

        let mut read = store.objects_of_memory("m1", true).await.unwrap();
        read.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));
        assert_eq!(read, held);

        let bare = store.objects_of_memory("m1", false).await.unwrap();
        assert_eq!(bare.len(), 250);
        assert!(bare.iter().all(|o| o.get("vector").is_none()));
        assert!(store
            .objects_of_memory("m3", true)
            .await
            .unwrap()
            .is_empty());
    }

    /// An object Weaviate fails to read is not an object Weaviate no longer
    /// holds: syncing its memory fails, and the replica keeps every copy
    /// of it, the stale one too, until a read answers.
    #[tokio::test]
    async fn a_memory_whose_object_weaviate_fails_to_read_is_synced_by_none_of_it() {
        let fake = Arc::new(Fake::live(10_000, false));
        let id = |n: u64| format!("00000000-0000-0000-0000-{n:012}");
        let object = |n: u64| {
            serde_json::json!({
                "class": "MemV2",
                "id": id(n),
                "properties": { "memoryId": "m1", "content": format!("note {n}"), "owner": "agent" },
                "vector": [n as f32, 1.0],
            })
        };
        fake.objects.lock().unwrap().extend([object(1), object(2)]);
        fake.failing_reads.lock().unwrap().push(id(2));
        let weaviate = store(&serve(Arc::clone(&fake)).await, false, false);
        let replica =
            crate::admin::testing::Held::holding(None, vec![object(1), object(2), object(3)]);

        let failed = crate::admin::sync_memory(&replica, &weaviate, "m1", &crate::admin::NoTurns)
            .await
            .unwrap_err();
        assert!(failed.to_string().contains("500"), "{failed}");
        assert_eq!(
            replica.objects.lock().unwrap().clone(),
            vec![object(1), object(2), object(3)],
            "nothing dropped on a failed read"
        );

        fake.failing_reads.lock().unwrap().clear();
        crate::admin::sync_memory(&replica, &weaviate, "m1", &crate::admin::NoTurns)
            .await
            .unwrap();
        let mut held: Vec<String> = replica
            .objects
            .lock()
            .unwrap()
            .iter()
            .map(|o| o["id"].as_str().unwrap().to_string())
            .collect();
        held.sort();
        assert_eq!(
            held,
            vec![id(1), id(2)],
            "once read, only the stale copy goes"
        );
    }

    /// Nothing promotes memories to Weaviate, so nothing is kept there as
    /// a replica: a replica sync into Weaviate is refused before it writes.
    #[tokio::test]
    async fn a_replica_into_weaviate_is_refused() {
        let fake = Arc::new(Fake::default());
        let weaviate = store(&serve(Arc::clone(&fake)).await, false, false);
        let source = crate::admin::testing::Held::holding(None, vec![]);
        let refused = crate::admin::sync(&weaviate, &source, SyncMode::Replica)
            .await
            .unwrap_err();
        assert!(
            refused.to_string().contains("never kept as a replica"),
            "{refused}"
        );
        let refused = crate::admin::sync_memory(&weaviate, &source, "m1", &crate::admin::NoTurns)
            .await
            .unwrap_err();
        assert!(
            refused.to_string().contains("never kept as a replica"),
            "{refused}"
        );
        assert!(
            fake.sent.lock().unwrap().is_empty(),
            "nothing written: {:?}",
            fake.sent.lock().unwrap()
        );
    }

    #[tokio::test]
    async fn a_person_is_reassigned_and_purged_by_owner_and_identity_deleted_ones_included() {
        let fake = Arc::new(Fake::live(10_000, false));
        fake.hold("o1", "agent", "alice");
        fake.objects.lock().unwrap()[0]["properties"]["deleted"] = true.into();
        fake.hold("o2", "other-agent", "alice");
        let url = serve(Arc::clone(&fake)).await;
        let store = store(&url, false, false);
        let owner = workforce_core::types::AgentId::new("agent");

        assert_eq!(
            store.identity_memories(&owner, "alice").await.unwrap(),
            vec![MemoryId("o1".into())]
        );
        assert_eq!(
            store
                .reassign_identity(&owner, "alice", "bob")
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            fake.objects.lock().unwrap()[0]["properties"]["identityId"],
            "bob"
        );
        let query = &graphql_queries(&fake)[0];
        assert!(
            query.contains(r#"path: ["owner"], operator: Equal, valueText: "agent""#),
            "{query}"
        );
        assert!(
            query.contains(r#"path: ["identityId"], operator: Equal, valueText: "alice""#),
            "{query}"
        );
        assert!(
            !query.contains("deleted"),
            "forgotten memories are erased too: {query}"
        );

        assert_eq!(store.purge_identity(&owner, "bob").await.unwrap(), 1);
        assert_eq!(fake.ids(), ["o2"], "another agent's memory survives");
    }

    #[tokio::test]
    async fn a_purge_deletes_only_the_exact_identity_whatever_the_index_matched() {
        let fake = Arc::new(Fake::live(10_000, true));
        let customer = "ext:iss\u{1f}sub-1";
        fake.hold("o1", "agent", customer);
        fake.hold("o2", "agent", "ext:iss\u{1f}sub-10");
        fake.hold("o3", "agent", customer);
        let url = serve(Arc::clone(&fake)).await;
        let store = store(&url, false, false);
        let owner = workforce_core::types::AgentId::new("agent");

        assert_eq!(
            store
                .identity_memories(&owner, customer)
                .await
                .unwrap()
                .len(),
            2
        );
        assert_eq!(store.purge_identity(&owner, customer).await.unwrap(), 2);
        assert_eq!(fake.ids(), ["o2"], "the neighbour survives");
    }

    #[tokio::test]
    async fn an_identity_past_the_result_cap_is_purged_whole() {
        let fake = Arc::new(Fake::live(1_000, false));
        (0..1_200).for_each(|i| fake.hold(&format!("c{i:04}"), "agent", "ext:iss\u{1f}sub-1"));
        fake.hold("n", "agent", "ext:iss\u{1f}sub-10");
        let url = serve(Arc::clone(&fake)).await;
        let store = store(&url, false, false);
        let owner = workforce_core::types::AgentId::new("agent");

        assert!(
            store
                .identity_memories(&owner, "ext:iss\u{1f}sub-1")
                .await
                .is_err(),
            "a capped read fails rather than coming back short"
        );
        assert_eq!(
            store
                .purge_identity(&owner, "ext:iss\u{1f}sub-1")
                .await
                .unwrap(),
            1_200
        );
        assert_eq!(fake.ids(), ["n"]);
    }

    #[tokio::test]
    async fn an_owners_identities_are_listed_once_each_past_the_result_cap() {
        let fake = Arc::new(Fake::live(1_000, false));
        (0..1_200).for_each(|i| fake.hold(&format!("k{i:04}"), "agent", "edge:knowledge"));
        for (i, identity) in ["alice", "bob", "alice", ""].into_iter().enumerate() {
            fake.hold(&format!("o{i}"), "agent", identity);
        }
        fake.hold("x", "other-agent", "carol");
        fake.objects.lock().unwrap()[1_200]["properties"]["deleted"] = true.into();
        let url = serve(Arc::clone(&fake)).await;
        let owner = workforce_core::types::AgentId::new("agent");

        assert_eq!(
            store(&url, false, false).identities(&owner).await.unwrap(),
            ["alice", "bob", "edge:knowledge"]
        );
        assert!(graphql_queries(&fake)
            .iter()
            .all(|query| !query.contains("where") && !query.contains("offset")));
    }

    #[tokio::test]
    async fn a_memory_is_stored_with_the_vector_of_its_content_only() {
        let fake = Arc::new(Fake::default());
        let url = serve(Arc::clone(&fake)).await;
        let memory = Memory::new(
            "banana",
            MemoryLayer::Semantic,
            workforce_core::types::AgentId::new("agent"),
        );
        store(&url, false, false).remember(memory).await.unwrap();
        let sent = fake.sent.lock().unwrap();
        let (path, body) = &sent[0];
        assert_eq!(path, "/v1/objects");
        assert_eq!(body["class"], "MemV2");
        assert_eq!(
            body["vector"],
            serde_json::json!([6.0, 3.0]),
            "the vector of `banana`, nothing else"
        );
    }

    #[tokio::test]
    async fn recall_is_hybrid_on_content_with_absolute_scores() {
        let fake = Arc::new(Fake::default());
        *fake.get_rows.lock().unwrap() =
            vec![row("far", vec![-6.0, -3.0]), row("near", vec![6.0, 3.0])];
        let url = serve(Arc::clone(&fake)).await;
        let results = store(&url, false, false)
            .recall("banana", unscoped())
            .await
            .unwrap();
        let sent = fake.sent.lock().unwrap();
        let query = sent[0].1["query"].as_str().unwrap();
        assert!(
            query.contains("hybrid:")
                && query.contains("vector: [6.0,3.0]")
                && query.contains(r#"properties: ["content"]"#),
            "{query}"
        );
        assert!(query.contains("alpha: 0.75"), "{query}");
        assert_eq!(
            results
                .iter()
                .map(|r| r.memory.content.as_str())
                .collect::<Vec<_>>(),
            ["near", "far"]
        );
        assert!(
            (results[0].score - 1.0).abs() < 1e-6 && results[1].score.abs() < 1e-6,
            "certainty, not fusion rank"
        );
    }

    /// A vector given is the one asked with, and the store's own embedder
    /// is not asked (its embedding service down here: had it been asked,
    /// the recall would fall back to keywords); none given, by keywords
    /// alone, though the embedder is up (DW-676: one embedding serves the
    /// user's recall and the shadow's).
    #[tokio::test]
    async fn a_recall_given_its_vector_uses_it_and_embeds_nothing() {
        let fake = Arc::new(Fake::default());
        *fake.get_rows.lock().unwrap() = vec![row("near", vec![6.0, 3.0])];
        let url = serve(Arc::clone(&fake)).await;
        let down = store(&url, true, false);
        assert!(down.query_model().is_some());
        down.recall_with_vector("banana", Some(vec![6.0, 3.0]), unscoped())
            .await
            .unwrap();
        store(&url, false, false)
            .recall_with_vector("banana", None, unscoped())
            .await
            .unwrap();
        let sent = fake.sent.lock().unwrap();
        let given = sent[0].1["query"].as_str().unwrap();
        assert!(
            given.contains("hybrid:") && given.contains("vector: [6.0,3.0]"),
            "{given}"
        );
        let none = sent[1].1["query"].as_str().unwrap();
        assert!(
            none.contains("bm25:") && !none.contains("hybrid:"),
            "{none}"
        );
    }

    #[tokio::test]
    async fn without_the_embedding_service_recall_falls_back_to_keywords() {
        let fake = Arc::new(Fake::default());
        *fake.get_rows.lock().unwrap() = vec![row("kept", vec![1.0, 1.0])];
        let url = serve(Arc::clone(&fake)).await;
        let results = store(&url, true, false)
            .recall("banana", unscoped())
            .await
            .unwrap();
        let query = fake.sent.lock().unwrap()[0].1["query"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(
            query.contains("bm25:") && !query.contains("hybrid:"),
            "{query}"
        );
        assert_eq!(results.len(), 1);
        assert_eq!(
            results[0].score, 0.0,
            "no absolute relevance, so nothing clears a threshold"
        );
    }

    #[tokio::test]
    async fn a_reranker_reorders_and_its_relevance_is_the_score() {
        let fake = Arc::new(Fake::default());
        *fake.get_rows.lock().unwrap() = vec![row("a", vec![6.0, 3.0]), row("b", vec![1.0, 0.0])];
        let url = serve(Arc::clone(&fake)).await;
        let results = store(&url, false, true)
            .recall("banana", unscoped())
            .await
            .unwrap();
        let query = fake.sent.lock().unwrap()[0].1["query"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(
            query.contains("limit: 30"),
            "candidates fetched for reranking: {query}"
        );
        assert_eq!(
            results
                .iter()
                .map(|r| r.memory.content.as_str())
                .collect::<Vec<_>>(),
            ["b", "a"]
        );
        assert!((results[0].score - 0.9).abs() < 1e-6);
    }

    #[tokio::test]
    async fn min_certainty_applies_to_the_computed_certainty() {
        let fake = Arc::new(Fake::default());
        *fake.get_rows.lock().unwrap() =
            vec![row("near", vec![6.0, 3.0]), row("far", vec![-6.0, -3.0])];
        let url = serve(Arc::clone(&fake)).await;
        let filter = MemoryFilter {
            min_certainty: Some(0.8),
            ..unscoped()
        };
        let results = store(&url, false, false)
            .recall("banana", filter)
            .await
            .unwrap();
        assert_eq!(
            results
                .iter()
                .map(|r| r.memory.content.as_str())
                .collect::<Vec<_>>(),
            ["near"]
        );
    }

    const ID1: &str = "00000000-0000-0000-0000-000000000001";
    const ID2: &str = "00000000-0000-0000-0000-000000000002";
    const ID3: &str = "00000000-0000-0000-0000-000000000003";

    fn obj(class: &str, id: &str, props: serde_json::Value) -> serde_json::Value {
        serde_json::json!({ "class": class, "id": id, "properties": props })
    }

    /// As [`obj`], with a vector: as a collection the hub embeds for holds
    /// a memory whose embedding did not fail.
    fn held(class: &str, id: &str, props: serde_json::Value) -> serde_json::Value {
        let mut held = obj(class, id, props);
        held["vector"] = serde_json::json!([1.0, 0.0]);
        held
    }

    fn props(content: &str, deleted: bool, access: u32) -> serde_json::Value {
        serde_json::json!({ "content": content, "memoryId": content, "deleted": deleted, "accessCount": access })
    }

    fn v2(fake: &Fake, id: &str) -> Option<serde_json::Value> {
        fake.objects
            .lock()
            .unwrap()
            .iter()
            .rev()
            .find(|o| o["class"] == "MemV2" && o["id"] == id)
            .cloned()
    }

    /// Weaviate-shaped memories, over more than one export page, mirrored
    /// into a real vellev node (see `crate::vellev::node_tests`) and back:
    /// the second pass each way finds nothing to change.
    #[tokio::test]
    async fn a_mirror_between_weaviate_and_vellev_finds_them_alike_after_one_pass() {
        use crate::vellev::node_tests::{Node, Stores};
        let Some(node) = Node::fresh().await else {
            return;
        };
        let n = 150;
        let fake = Arc::new(Fake::default());
        fake.objects
            .lock()
            .unwrap()
            .extend((0..n).map(|i| {
                let id = format!("00000000-0000-0000-0000-{i:012}");
                obj(
                    "WorkforceMemory",
                    &id,
                    serde_json::json!({
                        "content": format!("note {i}"), "memoryId": id, "layer": "episodic",
                        "owner": "agent", "visibility": "private",
                        "importance": if i % 2 == 0 { serde_json::json!(1) } else { serde_json::json!(0.699999988079071) },
                        "tags": if i % 3 == 0 { serde_json::json!([]) } else { serde_json::json!(["pinned"]) },
                        "sourceMessageIds": [], "deleted": false, "accessCount": i,
                        "createdAt": "2026-07-05T09:30:00.1200005Z", "lastAccessed": "2026-07-05T09:30:00Z",
                        "eventTime": "2026-07-04T23:00:00.1Z", "source": "human", "identityId": "alice",
                        "recurrenceCount": 0,
                    }),
                )
            }));
        let weaviate = WeaviateStore::new(&serve(Arc::clone(&fake)).await, None);
        let vellev = node.store("WorkforceMemory").await;

        let first = crate::admin::sync(&vellev, &weaviate, SyncMode::Mirror)
            .await
            .unwrap();
        assert_eq!(first.copied, n, "{first:?}");
        let again = crate::admin::sync(&vellev, &weaviate, SyncMode::Mirror)
            .await
            .unwrap();
        assert_eq!(
            again,
            SyncReport {
                unchanged: n,
                ..Default::default()
            }
        );
        let back = crate::admin::sync(&weaviate, &vellev, SyncMode::Mirror)
            .await
            .unwrap();
        assert_eq!(
            back,
            SyncReport {
                unchanged: n,
                ..Default::default()
            }
        );
    }

    /// Against a node: a vellev imported from Weaviate carries the switch
    /// marker, and a replica drops what Weaviate purged all the same; a
    /// memory synced alone keeps only the objects Weaviate holds it in,
    /// under their ids; and once vellev is promoted neither writes to it.
    /// The switch marker is never taken for a promotion.
    #[tokio::test]
    async fn a_replica_into_vellev_drops_what_weaviate_purged_and_refuses_once_promoted() {
        use crate::admin::{sync, sync_memory, NoTurns, Promoted, SyncSource, SyncTarget};
        use crate::vellev::node_tests::{Node, Stores};
        let Some(node) = Node::fresh().await else {
            return;
        };
        let id = |n: u64| format!("00000000-0000-0000-0000-{n:012}");
        let object = |n: u64, memory: &str| {
            obj(
                "WorkforceMemory",
                &id(n),
                serde_json::json!({
                    "content": format!("note {n}"), "memoryId": memory, "layer": "episodic",
                    "owner": "agent", "visibility": "private", "importance": 0.5, "tags": [],
                    "sourceMessageIds": [], "deleted": false, "accessCount": n,
                    "createdAt": "2026-07-05T09:30:00Z", "lastAccessed": "2026-07-05T09:30:00Z",
                    "source": "human", "identityId": "alice", "recurrenceCount": 0,
                }),
            )
        };
        let fake = Arc::new(Fake::live(10_000, false));
        fake.objects.lock().unwrap().extend([
            object(0, "m0"),
            object(1, "m1"),
            object(2, "m2"),
            object(3, "m3"),
            object(5, "m1"),
        ]);
        let weaviate = WeaviateStore::new(&serve(Arc::clone(&fake)).await, None);
        let vellev = node.store("WorkforceMemory").await;
        assert_eq!(
            sync(&vellev, &weaviate, SyncMode::Replica)
                .await
                .unwrap()
                .copied,
            5
        );
        // As an import brings it.
        vellev.mark_switched().await.unwrap();
        assert!(
            !vellev.promoted().await.unwrap(),
            "a switch is not a promotion"
        );

        fake.objects
            .lock()
            .unwrap()
            .retain(|o| o["id"] != id(2).as_str() && o["id"] != id(5).as_str());
        let report = sync(&vellev, &weaviate, SyncMode::Replica).await.unwrap();
        assert_eq!((report.dropped, report.unchanged), (2, 3), "{report:?}");
        assert!(vellev
            .objects_of_memory("m2", false)
            .await
            .unwrap()
            .is_empty());
        let ids = |objects: Vec<serde_json::Value>| -> Vec<String> {
            let mut ids: Vec<String> = objects
                .iter()
                .map(|o| o["id"].as_str().unwrap().to_string())
                .collect();
            ids.sort();
            ids
        };
        assert_eq!(
            ids(vellev.objects_of_memory("m1", false).await.unwrap()),
            [id(1)]
        );

        // A stale copy of m3, under an id Weaviate does not hold it by.
        vellev.upsert(&[object(6, "m3")]).await.unwrap();
        let report = sync_memory(&vellev, &weaviate, "m3", &NoTurns)
            .await
            .unwrap();
        assert_eq!((report.dropped, report.unchanged), (1, 1), "{report:?}");
        let held = vellev.objects_of_memory("m3", true).await.unwrap();
        assert_eq!(ids(held.clone()), [id(3)]);
        assert!(
            held[0]["vector"].is_array(),
            "read with its vector: {held:?}"
        );
        assert_eq!(held[0]["properties"]["accessCount"], 3);
        let by_id = vellev.objects_by_id(&[id(3), id(99)], true).await.unwrap();
        assert_eq!(ids(by_id.clone()), [id(3)], "one held, one not");
        assert!(by_id[0]["vector"].is_array(), "{by_id:?}");
        assert_eq!(by_id[0]["properties"], held[0]["properties"]);

        workforce_vellev::mark_promoted(&node.tenant(), "WorkforceMemory")
            .await
            .unwrap();
        assert!(vellev.promoted().await.unwrap());
        fake.objects.lock().unwrap().clear();
        let refused = sync(&vellev, &weaviate, SyncMode::Replica)
            .await
            .unwrap_err();
        assert!(refused.downcast_ref::<Promoted>().is_some(), "{refused}");
        let refused = sync_memory(&vellev, &weaviate, "m0", &NoTurns)
            .await
            .unwrap_err();
        assert!(refused.downcast_ref::<Promoted>().is_some(), "{refused}");
        assert_eq!(
            vellev.export(false).await.unwrap().len(),
            3,
            "nothing dropped"
        );
    }

    #[tokio::test]
    async fn a_mirror_sync_copies_updates_and_drops_keeping_ids_and_vectors() {
        let fake = Arc::new(Fake::default());
        fake.objects.lock().unwrap().extend([
            obj("WorkforceMemory", ID1, props("one", false, 5)),
            obj("WorkforceMemory", ID2, props("two", false, 0)),
            held("MemV2", ID1, props("one", false, 1)),
            held("MemV2", ID3, props("gone", false, 0)),
        ]);
        let url = serve(Arc::clone(&fake)).await;
        let v1 = WeaviateStore::new(&url, None);
        let report = crate::admin::sync(&store(&url, false, false), &v1, SyncMode::Mirror)
            .await
            .unwrap();
        assert_eq!(
            report,
            SyncReport {
                copied: 1,
                updated: 1,
                dropped: 1,
                unchanged: 0
            }
        );
        let two = v2(&fake, ID2).expect("copied with its id");
        assert_eq!(two["vector"], serde_json::json!([3.0, 0.0]));
        assert_eq!(
            v2(&fake, ID1).unwrap()["properties"]["accessCount"],
            5,
            "patched, not re-embedded"
        );
        assert!(
            v2(&fake, ID3).is_none(),
            "dropped: the source no longer has it"
        );
    }

    #[tokio::test]
    async fn a_mirror_after_another_replica_switched_keeps_what_it_wrote() {
        let fake = Arc::new(Fake::default());
        fake.objects.lock().unwrap().extend([
            obj("WorkforceMemory", ID1, props("one", false, 0)),
            obj("MemV2", ID1, props("one", false, 7)),
            obj("MemV2", ID2, props("written after the switch", false, 0)),
            obj(
                SWITCH_COLLECTION,
                &switch_marker_id("MemV2").to_string(),
                serde_json::json!({}),
            ),
        ]);
        let url = serve(Arc::clone(&fake)).await;
        let v1 = WeaviateStore::new(&url, None);
        let report = crate::admin::sync(&store(&url, false, false), &v1, SyncMode::Mirror)
            .await
            .unwrap();
        assert_eq!((report.updated, report.dropped), (0, 0));
        assert!(v2(&fake, ID2).is_some());
        assert_eq!(v2(&fake, ID1).unwrap()["properties"]["accessCount"], 7);
    }

    #[tokio::test]
    async fn a_catch_up_sync_adds_new_memories_and_forgetting_only() {
        let fake = Arc::new(Fake::default());
        fake.objects.lock().unwrap().extend([
            obj("WorkforceMemory", ID1, props("one", true, 0)),
            obj("WorkforceMemory", ID2, props("two", false, 0)),
            obj("WorkforceMemory", ID3, props("three", false, 9)),
            held("MemV2", ID1, props("one", false, 0)),
            held("MemV2", ID3, props("three", true, 2)),
        ]);
        let url = serve(Arc::clone(&fake)).await;
        let v1 = WeaviateStore::new(&url, None);
        let report = crate::admin::sync(&store(&url, false, false), &v1, SyncMode::CatchUp)
            .await
            .unwrap();
        assert_eq!((report.copied, report.updated), (1, 1));
        assert_eq!(v2(&fake, ID1).unwrap()["properties"]["deleted"], true);
        let three = v2(&fake, ID3).unwrap();
        assert_eq!(
            (
                three["properties"]["deleted"].clone(),
                three["properties"]["accessCount"].clone()
            ),
            (serde_json::json!(true), serde_json::json!(2)),
            "a memory forgotten here stays forgotten"
        );
    }

    #[test]
    fn the_switch_marker_id_is_a_stable_uuid_per_collection() {
        let a = switch_marker_id("WorkforceMemoryTitan2");
        assert_eq!(a, switch_marker_id("WorkforceMemoryTitan2"));
        assert_ne!(a, switch_marker_id("WorkforceMemoryOther"));
    }
}
