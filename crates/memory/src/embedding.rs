//! Pluggable embedding backends for the memory store.
//!
//! `SqliteMemoryStore` uses an optional [`EmbeddingBackend`] to enrich recall
//! with vector similarity on top of BM25 keyword search. The bundled
//! [`OpenAiEmbeddingBackend`] talks to any OpenAI-compatible `/v1/embeddings`
//! endpoint; alternative backends (e.g. an in-process fastembed-rs) can
//! implement the trait directly without going through this crate.

use async_trait::async_trait;
use eyre::{Result, WrapErr};
use serde::Deserialize;
use workforce_common::secret::{current_key, fixed_key, scrub, ApiKeySource};

use crate::types::MemorySearchResult;

/// Whether text is a stored document or a search query. Instruction-tuned
/// models embed the two differently: a query carries a task instruction, a
/// document none.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmbedKind {
    Document,
    Query,
}

/// Embeds text into a fixed-dimension vector. Dimension must be consistent
/// within a single store — memories and queries are compared by cosine
/// similarity, so mixing backends with different dimensions silently breaks
/// retrieval.
#[async_trait]
pub trait EmbeddingBackend: Send + Sync {
    /// One document's vector.
    async fn embed(&self, text: &str) -> Result<Vec<f32>>;

    /// Many texts at once, as documents or queries. The default embeds one at
    /// a time as documents; backends with a batch API or query instructions
    /// override it.
    async fn embed_many(&self, texts: &[String], kind: EmbedKind) -> Result<Vec<Vec<f32>>> {
        let _ = kind;
        let mut out = Vec::with_capacity(texts.len());
        for text in texts {
            out.push(self.embed(text).await?);
        }
        Ok(out)
    }

    /// A query's vector.
    async fn embed_query(&self, text: &str) -> Result<Vec<f32>> {
        self.embed_many(&[text.to_string()], EmbedKind::Query)
            .await?
            .into_iter()
            .next()
            .ok_or_else(|| eyre::eyre!("empty embedding response"))
    }

    /// What produced the vectors, so a store can tell when a different model
    /// would make its stored vectors incomparable.
    fn model_id(&self) -> &str {
        "unknown"
    }
}

/// Reorders candidates by how well each answers the query.
#[async_trait]
pub trait Reranker: Send + Sync {
    /// `(index into texts, relevance)` for each text, most relevant first.
    async fn rerank(&self, query: &str, texts: &[String]) -> Result<Vec<(usize, f32)>>;
}

/// A text-embeddings-inference server's `/embed`. Queries and documents each
/// carry the prefix the model was trained with: e.g. bge's "Represent this
/// sentence for searching relevant passages: ", nomic's "search_query: " /
/// "search_document: ", Qwen3-Embedding's "Instruct: <task>\nQuery: ".
pub struct TeiEmbeddingBackend {
    client: reqwest::Client,
    url: String,
    model: String,
    query_prefix: String,
    document_prefix: String,
}

impl TeiEmbeddingBackend {
    pub fn new(url: &str, model: &str, query_prefix: &str, document_prefix: &str) -> Self {
        Self {
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(60))
                .build()
                .unwrap_or_default(),
            url: url.trim_end_matches('/').to_string(),
            model: model.to_string(),
            query_prefix: query_prefix.to_string(),
            document_prefix: document_prefix.to_string(),
        }
    }

    fn prepare(&self, text: &str, kind: EmbedKind) -> String {
        match kind {
            EmbedKind::Query => format!("{}{text}", self.query_prefix),
            EmbedKind::Document => format!("{}{text}", self.document_prefix),
        }
    }
}

/// Texts per request to an embedding server.
const TEI_BATCH: usize = 32;

#[async_trait]
impl EmbeddingBackend for TeiEmbeddingBackend {
    async fn embed(&self, text: &str) -> Result<Vec<f32>> {
        self.embed_many(&[text.to_string()], EmbedKind::Document)
            .await?
            .into_iter()
            .next()
            .ok_or_else(|| eyre::eyre!("empty embedding response"))
    }

    async fn embed_many(&self, texts: &[String], kind: EmbedKind) -> Result<Vec<Vec<f32>>> {
        let mut out = Vec::with_capacity(texts.len());
        for chunk in texts.chunks(TEI_BATCH) {
            let inputs: Vec<String> = chunk.iter().map(|t| self.prepare(t, kind)).collect();
            let resp = self
                .client
                .post(format!("{}/embed", self.url))
                .json(&serde_json::json!({ "inputs": inputs, "truncate": true }))
                .send()
                .await
                .wrap_err("embedding server unreachable")?;
            if !resp.status().is_success() {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                return Err(eyre::eyre!("embedding server returned {status}: {body}"));
            }
            let vectors: Vec<Vec<f32>> = resp.json().await.wrap_err("bad embedding response")?;
            if vectors.len() != chunk.len() {
                return Err(eyre::eyre!(
                    "embedding server returned {} vectors for {}",
                    vectors.len(),
                    chunk.len()
                ));
            }
            out.extend(vectors);
        }
        Ok(out)
    }

    fn model_id(&self) -> &str {
        &self.model
    }
}

/// A text-embeddings-inference server's `/rerank`.
pub struct TeiReranker {
    client: reqwest::Client,
    url: String,
}

impl TeiReranker {
    pub fn new(url: &str) -> Self {
        Self {
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .unwrap_or_default(),
            url: url.trim_end_matches('/').to_string(),
        }
    }
}

#[derive(Deserialize)]
struct RerankHit {
    index: usize,
    score: f32,
}

#[async_trait]
impl Reranker for TeiReranker {
    async fn rerank(&self, query: &str, texts: &[String]) -> Result<Vec<(usize, f32)>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let resp = self
            .client
            .post(format!("{}/rerank", self.url))
            .json(&serde_json::json!({ "query": query, "texts": texts, "truncate": true }))
            .send()
            .await
            .wrap_err("reranker unreachable")?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(eyre::eyre!("reranker returned {status}: {body}"));
        }
        let mut hits: Vec<RerankHit> = resp.json().await.wrap_err("bad rerank response")?;
        hits.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        Ok(hits
            .into_iter()
            .filter(|h| h.index < texts.len())
            .map(|h| (h.index, h.score))
            .collect())
    }
}

/// Map a certainty through `knots` (ascending `(from, to)` pairs), linearly
/// between knots and flat beyond the ends; no knots leaves it unchanged.
/// Lets a new model's scores read on the scale existing thresholds were
/// tuned on, without reordering anything.
pub fn map_score(knots: &[(f32, f32)], x: f32) -> f32 {
    let (Some(first), Some(last)) = (knots.first(), knots.last()) else {
        return x;
    };
    if x <= first.0 {
        return first.1;
    }
    if x >= last.0 {
        return last.1;
    }
    for pair in knots.windows(2) {
        let ((x0, y0), (x1, y1)) = (pair[0], pair[1]);
        if x <= x1 {
            if x1 <= x0 {
                return y1;
            }
            return y0 + (y1 - y0) * (x - x0) / (x1 - x0);
        }
    }
    last.1
}

/// Cosine similarity mapped to Weaviate's certainty scale, `(1 + cos) / 2`,
/// so thresholds read the same whether Weaviate or the hub computed them.
pub fn certainty(a: &[f32], b: &[f32]) -> f32 {
    let (mut dot, mut na, mut nb) = (0.0f32, 0.0f32, 0.0f32);
    for (x, y) in a.iter().zip(b) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    (1.0 + dot / (na.sqrt() * nb.sqrt())) / 2.0
}

/// Order hub-vectored recall results. `min_certainty` applies to the
/// certainty computed from the vectors (the store never saw the threshold).
/// A reranker, when given, reorders the candidates and its relevance
/// becomes the score; if it fails, results keep their certainty order.
pub(crate) async fn rank_embedded(
    reranker: Option<&dyn Reranker>,
    query: &str,
    mut results: Vec<MemorySearchResult>,
    min_certainty: Option<f32>,
    vectored: bool,
) -> Vec<MemorySearchResult> {
    if let (Some(min), true) = (min_certainty, vectored) {
        results.retain(|r| r.score >= min);
    }
    if let (Some(reranker), true) = (reranker, results.len() > 1) {
        let texts: Vec<String> = results.iter().map(|r| r.memory.content.clone()).collect();
        match reranker.rerank(query, &texts).await {
            Ok(order) => {
                let mut slots: Vec<Option<MemorySearchResult>> =
                    results.into_iter().map(Some).collect();
                return order
                    .into_iter()
                    .filter_map(|(i, relevance)| {
                        slots.get_mut(i).and_then(Option::take).map(|mut r| {
                            r.score = relevance;
                            r
                        })
                    })
                    .collect();
            }
            Err(e) => {
                tracing::warn!(target: "embeddings", error = %e, "rerank failed; keeping vector order")
            }
        }
    }
    if vectored {
        results.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
    }
    results
}

/// Embeds via an OpenAI-compatible `/v1/embeddings` endpoint.
pub struct OpenAiEmbeddingBackend {
    client: reqwest::Client,
    base_url: String,
    model: String,
    api_key: ApiKeySource,
}

impl OpenAiEmbeddingBackend {
    pub fn new(base_url: &str, model: &str, api_key: &str) -> Self {
        Self::with_key_source(base_url, model, fixed_key(api_key))
    }

    /// A request goes without `Authorization` when `api_key` has no key.
    pub fn with_key_source(base_url: &str, model: &str, api_key: ApiKeySource) -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url: base_url.to_string(),
            model: model.to_string(),
            api_key,
        }
    }
}

#[derive(Deserialize)]
struct EmbeddingResponse {
    data: Vec<EmbeddingData>,
}

#[derive(Deserialize)]
struct EmbeddingData {
    embedding: Vec<f32>,
}

#[async_trait]
impl EmbeddingBackend for OpenAiEmbeddingBackend {
    async fn embed(&self, text: &str) -> Result<Vec<f32>> {
        let url = format!("{}/v1/embeddings", self.base_url);
        let body = serde_json::json!({
            "model": self.model,
            "input": text,
        });

        let mut req = self.client.post(&url).json(&body);
        let key = current_key(&self.api_key);
        if let Some(key) = &key {
            req = req.bearer_auth(key);
        }
        let resp = req.send().await.wrap_err("Failed to call embedding API")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body_text = resp.text().await.unwrap_or_default();
            return Err(eyre::eyre!(
                "Embedding API returned {}: {}",
                status,
                scrub(&body_text, key.as_deref())
            ));
        }

        let response: EmbeddingResponse = resp
            .json()
            .await
            .wrap_err("Failed to parse embedding response")?;

        response
            .data
            .into_iter()
            .next()
            .map(|d| d.embedding)
            .ok_or_else(|| eyre::eyre!("Empty embedding response"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn certainty_matches_weaviates_scale() {
        assert!((certainty(&[1.0, 0.0], &[1.0, 0.0]) - 1.0).abs() < 1e-6);
        assert!((certainty(&[1.0, 0.0], &[0.0, 1.0]) - 0.5).abs() < 1e-6);
        assert!(certainty(&[1.0, 0.0], &[-1.0, 0.0]).abs() < 1e-6);
        assert_eq!(certainty(&[0.0, 0.0], &[1.0, 0.0]), 0.0);
    }

    #[test]
    fn queries_and_documents_carry_their_own_prefixes() {
        let tei = TeiEmbeddingBackend::new("http://x/", "m", "search_query: ", "search_document: ");
        assert_eq!(tei.prepare("why?", EmbedKind::Query), "search_query: why?");
        assert_eq!(
            tei.prepare("a note", EmbedKind::Document),
            "search_document: a note"
        );
        let plain = TeiEmbeddingBackend::new("http://x", "m", "", "");
        assert_eq!(plain.prepare("why?", EmbedKind::Query), "why?");
    }

    #[test]
    fn scores_map_linearly_between_knots_and_flat_beyond() {
        let knots = [(0.5, 0.0), (0.7, 0.6), (0.9, 0.9)];
        assert_eq!(map_score(&[], 0.42), 0.42);
        assert!((map_score(&knots, 0.6) - 0.3).abs() < 1e-6);
        assert!((map_score(&knots, 0.8) - 0.75).abs() < 1e-6);
        assert_eq!(map_score(&knots, 0.1), 0.0);
        assert_eq!(map_score(&knots, 0.99), 0.9);
    }
}
