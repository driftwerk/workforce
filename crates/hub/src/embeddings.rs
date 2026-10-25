//! The hub's embedding backends, built from `[embeddings]`: a
//! text-embeddings-inference server, an OpenAI-compatible endpoint, or Amazon
//! Bedrock (Titan Text Embeddings v2). With one configured, memories move to
//! a collection holding the hub's vectors (see `WeaviateStore::with_embeddings`
//! and `VellevStore::with_embeddings`).
//!
//! The OpenAI-compatible endpoint's key is read from the global vault on
//! every request (`embeddings/openai/api_key`), then `EMBEDDINGS_API_KEY`.
//! The embedder serves every person's memories into one collection, so the
//! key is the hub's own: no per-user or per-agent tier applies.

use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use eyre::{eyre, Result};
use parking_lot::{Mutex, RwLock};
use tokio::sync::Semaphore;
use workforce_agent::llm::bedrock::{resolve_region, sign_v4};
use workforce_agent::llm::bedrock_credentials::{CredentialResolver, CHAIN_SENTINEL};
use workforce_common::secret::ApiKeySource;
use workforce_common::settings::{EmbeddingsConfig, EmbeddingsProvider, WorkforceConfig};
use workforce_memory::admin::{self, Switchable, SyncMode};
use workforce_memory::embedding::{
    EmbedKind, EmbeddingBackend, OpenAiEmbeddingBackend, Reranker, TeiEmbeddingBackend, TeiReranker,
};
use workforce_memory::store::MemoryStore;
use workforce_memory::types::{Memory, MemoryFilter, MemoryId, MemorySearchResult, Visibility};
use workforce_memory::vellev::{self, VellevStore};
use workforce_memory::weaviate::WeaviateStore;

use crate::credentials::EMBEDDINGS_OPENAI_API_KEY;
use crate::vault::Vault;

/// Bedrock calls in flight at once while embedding many texts.
const BEDROCK_CONCURRENCY: usize = 10;
/// Titan v2 takes up to 8,192 tokens; characters beyond this are dropped.
const TITAN_MAX_CHARS: usize = 30_000;
/// Query embeddings kept, so a turn's memory and lesson recall of the same
/// text embed it once.
const QUERY_CACHE: usize = 256;

/// An embedder and the reranker that goes with it, if any.
pub type Backends = (Arc<dyn EmbeddingBackend>, Option<Arc<dyn Reranker>>);

/// The embedder and reranker `[embeddings]` names; `None` when unset.
/// `global_vault` holds the OpenAI-compatible endpoint's key.
pub fn build(
    config: &EmbeddingsConfig,
    global_vault: Option<Arc<Vault>>,
) -> Result<Option<Backends>> {
    let url = || {
        config
            .url
            .as_deref()
            .ok_or_else(|| eyre!("[embeddings] provider {:?} needs a url", config.provider))
    };
    let model = config.model_name().to_string();
    let embedder: Arc<dyn EmbeddingBackend> = match config.provider {
        EmbeddingsProvider::None => return Ok(None),
        EmbeddingsProvider::Tei => Arc::new(TeiEmbeddingBackend::new(
            url()?,
            &model,
            &config.query_prefix,
            &config.document_prefix,
        )),
        EmbeddingsProvider::Openai => Arc::new(OpenAiEmbeddingBackend::with_key_source(
            url()?,
            &model,
            openai_key(global_vault, EMBEDDINGS_OPENAI_API_KEY.from_env()),
        )),
        EmbeddingsProvider::Bedrock => Arc::new(BedrockEmbeddings::new(
            &model,
            config.dimensions.unwrap_or(1024),
            &resolve_region(config.region.as_deref()),
        )),
    };
    let reranker = config
        .rerank_url
        .as_deref()
        .map(|u| Arc::new(TeiReranker::new(u)) as Arc<dyn Reranker>);
    Ok(Some((Arc::new(QueryCached::new(embedder)), reranker)))
}

/// The OpenAI-compatible endpoint's key: the global vault's, read per
/// request, else `fallback`.
fn openai_key(global_vault: Option<Arc<Vault>>, fallback: Option<String>) -> ApiKeySource {
    crate::credentials::source(&EMBEDDINGS_OPENAI_API_KEY, None, global_vault, fallback)
}

/// [`build`] for the install, with the global vault opened only for the
/// provider that reads its key there; an open returns the instance the hub
/// already holds, so a rotation written through it is seen here.
pub async fn from_config(config: &WorkforceConfig) -> Result<Option<Backends>> {
    let global_vault = match config.embeddings.provider {
        EmbeddingsProvider::Openai => crate::vault::open_global(config).await?,
        _ => None,
    };
    build(&config.embeddings, global_vault)
}

/// After the switch, replicas still on the old release keep writing to the
/// old collection; new memories and forgetting are carried over this often,
/// and, once that is over, carried back to the old collection this often
/// too, for as long as it is kept, so that it can be switched back to...
const CATCH_UP_EVERY: Duration = Duration::from_secs(120);
/// ...the former for this long after each switch or start.
const CATCH_UP_FOR: Duration = Duration::from_secs(30 * 60);
/// A failed sync is retried after this long.
const SYNC_RETRY: Duration = Duration::from_secs(600);
/// How often a replica that is not copying looks for the switch marker.
const SWITCH_POLL: Duration = Duration::from_secs(30);
/// Time for the leader election to be wired before the first leadership check.
const LEADER_WIRING: Duration = Duration::from_secs(20);

/// The memory store to use: `v1` (Weaviate's own vectorizer) until
/// `[embeddings]` is set and its collection holds every memory, then that
/// collection. The first sync runs in the background while the hub serves
/// from `v1`; the new collection is read only once a full sync finished,
/// and the running hub then switches over. `v1` is kept for rollback:
/// forgetting a memory forgets it there too.
pub async fn memory_store(
    config: &WorkforceConfig,
    v1: Arc<dyn Switchable>,
    recovery_dir: Option<&Path>,
    budget: Duration,
) -> Result<Arc<dyn MemoryStore>> {
    config.check_memory_backend()?;
    crate::promotion::guard_memories(config).await?;
    let Some((embedder, reranker)) = from_config(config).await? else {
        return Ok(v1);
    };
    let e = &config.embeddings;
    // One store (and one background sync) per process and collection, however
    // many parts of the hub open memory.
    static OPENED: tokio::sync::Mutex<Vec<(String, Arc<dyn MemoryStore>)>> =
        tokio::sync::Mutex::const_new(Vec::new());
    let mut opened = OPENED.lock().await;
    let key = if config.memory.is_vellev() {
        let v = &config.vellev;
        format!("{}/{}#{}", v.url, v.tenant, e.collection_name())
    } else {
        format!("{}#{}", config.weaviate.url, e.collection_name())
    };
    if let Some((_, store)) = opened.iter().find(|(k, _)| *k == key) {
        return Ok(Arc::clone(store));
    }
    let store = open_switching(config, v1, embedder, reranker, recovery_dir, budget).await?;
    opened.push((key, Arc::clone(&store)));
    Ok(store)
}

async fn open_switching(
    config: &WorkforceConfig,
    v1: Arc<dyn Switchable>,
    embedder: Arc<dyn EmbeddingBackend>,
    reranker: Option<Arc<dyn Reranker>>,
    recovery_dir: Option<&Path>,
    budget: Duration,
) -> Result<Arc<dyn MemoryStore>> {
    let e = &config.embeddings;
    let v2 = embedded_store(config, Backend::of(config), embedder, reranker)?;
    let label = if config.memory.is_vellev() {
        "vellev"
    } else {
        "Weaviate"
    };
    crate::state::ensure_memory_store_ready(v2.as_ref(), recovery_dir, budget, label).await?;
    if v2.switched().await? {
        keep(config.embeddings.keep_previous, &v2, v1);
        let current: Arc<dyn MemoryStore> = v2;
        return Ok(current);
    }
    let store = Arc::new(Switching(RwLock::new(
        Arc::clone(&v1) as Arc<dyn MemoryStore>
    )));
    let switching = Arc::clone(&store);
    let collection = e.collection_name();
    let keep_previous = e.keep_previous;
    tokio::spawn(async move {
        // Only the fleet's leader copies (several replicas copying would
        // embed every memory several times over); the others switch when
        // they see its marker. The leadership flag is wired shortly after
        // the store opens, so the first look waits for it.
        tokio::time::sleep(LEADER_WIRING).await;
        let mut last_attempt: Option<tokio::time::Instant> = None;
        loop {
            match v2.switched().await {
                Ok(true) => break,
                Ok(false) => {}
                Err(err) => {
                    tracing::warn!(target: "embeddings", error = %err, "reading the memory switch marker failed")
                }
            }
            let due = last_attempt.is_none_or(|at| at.elapsed() >= SYNC_RETRY);
            if due && crate::health_gauges::is_leader() {
                last_attempt = Some(tokio::time::Instant::now());
                tracing::info!(target: "embeddings", %collection, "memory sync starting on the leader");
                match admin::sync(v2.as_ref(), v1.as_ref(), SyncMode::Mirror).await {
                    Ok(_) => match v2.mark_switched().await {
                        Ok(()) => break,
                        Err(err) => {
                            tracing::warn!(target: "embeddings", error = %err, "recording the memory switch failed")
                        }
                    },
                    Err(err) => {
                        tracing::warn!(target: "embeddings", error = %err, %collection, "memory sync failed; still on the previous collection")
                    }
                }
            }
            tokio::time::sleep(SWITCH_POLL).await;
        }
        let current: Arc<dyn MemoryStore> = v2.clone();
        *switching.0.write() = current;
        tracing::info!(target: "embeddings", %collection, "memories switched to the hub's embeddings");
        keep(keep_previous, &v2, v1);
    });
    Ok(store)
}

/// The store memories switch from: the configured backend's collection
/// that embeds for itself, `[vellev]`'s when `[memory] backend = "vellev"`,
/// otherwise `[weaviate]`'s.
pub fn base_store(config: &WorkforceConfig) -> Result<Arc<dyn Switchable>> {
    // Every process that builds a memory store is refused a backend it
    // cannot use here, not when its config was read (DW-676).
    config.check_memory_backend()?;
    Ok(if config.memory.is_vellev() {
        Arc::new(vellev_store(config, vellev::DEFAULT_COLLECTION)?)
    } else {
        Arc::new(WeaviateStore::new(
            &config.weaviate.url,
            config.weaviate.api_key.as_deref(),
        ))
    })
}

fn vellev_store(config: &WorkforceConfig, collection: &str) -> Result<VellevStore> {
    let v = &config.vellev;
    let key = v.api_key.as_deref().ok_or_else(|| {
        eyre!(
            "[memory] backend = \"{}\" needs [vellev] api_key",
            config.memory.backend
        )
    })?;
    VellevStore::new(&v.url, key, &v.tenant, collection)
}

/// The database a memory store is built on: chosen here by the caller,
/// never read from the config by the builder, so that shadow mode can
/// build vellev's store while `[memory] backend` routes everything to
/// Weaviate (DW-676, design r2 section 4.7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Weaviate,
    Vellev,
}

impl Backend {
    /// The backend `[memory] backend` serves memories from: vellev only
    /// for `"vellev"`, so Weaviate for `"shadow"`.
    pub fn of(config: &WorkforceConfig) -> Self {
        match config.memory.is_vellev() {
            true => Self::Vellev,
            false => Self::Weaviate,
        }
    }
}

/// The two stores of memories shadow mode keeps in step: vellev's
/// replica, and the Weaviate collection it copies, which serves. Both are
/// the collection the hub embeds for, built with the same `[embeddings]`
/// (alpha, reranker, candidates, score map) and the same embedder, so they
/// name the same model and vectors are copied, not embedded again.
///
/// Neither goes through [`memory_store`]: its cache is keyed on the
/// backend that serves, and its switch machinery would write the switch
/// marker into the replica and sync it as a mirror. The replica is kept
/// only by `SyncMode::Replica`.
pub async fn shadow_stores(
    config: &WorkforceConfig,
) -> Result<(Arc<dyn Switchable>, Arc<dyn Switchable>)> {
    let (embedder, reranker) = from_config(config).await?.ok_or_else(|| {
        eyre!(
            "[memory] backend = \"shadow\" needs [embeddings]: the replica holds the hub's vectors"
        )
    })?;
    paired(config, embedder, reranker)
}

/// vellev's store of the collection memories are first kept in, the one
/// an import from Weaviate brings: shadow mode refuses a replica's tenant
/// that holds it, since nothing it does reaches it, an erasure included.
pub fn shadow_v1_store(config: &WorkforceConfig) -> Result<VellevStore> {
    vellev_store(config, vellev::DEFAULT_COLLECTION)
}

/// [`shadow_stores`] with the embedder given.
pub(crate) fn paired(
    config: &WorkforceConfig,
    embedder: Arc<dyn EmbeddingBackend>,
    reranker: Option<Arc<dyn Reranker>>,
) -> Result<(Arc<dyn Switchable>, Arc<dyn Switchable>)> {
    let replica = embedded_store(
        config,
        Backend::Vellev,
        Arc::clone(&embedder),
        reranker.clone(),
    )?;
    let serving = embedded_store(config, Backend::Weaviate, embedder, reranker)?;
    Ok((replica, serving))
}

/// The store memories switch to: the collection the hub embeds for, on
/// `backend`.
fn embedded_store(
    config: &WorkforceConfig,
    backend: Backend,
    embedder: Arc<dyn EmbeddingBackend>,
    reranker: Option<Arc<dyn Reranker>>,
) -> Result<Arc<dyn Switchable>> {
    let e = &config.embeddings;
    let score_map = e.score_map.iter().map(|[a, b]| (*a, *b)).collect();
    Ok(if backend == Backend::Vellev {
        Arc::new(
            vellev_store(config, &e.collection_name())?
                .with_embeddings(embedder, reranker, e.alpha, e.rerank_candidates)
                .with_score_map(score_map),
        )
    } else {
        Arc::new(
            WeaviateStore::new(&config.weaviate.url, config.weaviate.api_key.as_deref())
                .with_embeddings(
                    &e.collection_name(),
                    embedder,
                    reranker,
                    e.alpha,
                    e.rerank_candidates,
                )
                .with_score_map(score_map),
        )
    })
}

/// Keep `previous`, the store memories switched from, beside `current`
/// (forgetting and erasure reaching it, a catch-up from and back into
/// it), unless `[embeddings] keep_previous` is off: then nothing of the
/// hub's reads or writes it again (DW-676).
fn keep(keep_previous: bool, current: &Arc<dyn Switchable>, previous: Arc<dyn Switchable>) {
    if !keep_previous {
        tracing::info!(target: "embeddings", collection = previous.collection(), "the collection memories switched from is left alone ([embeddings] keep_previous = false)");
        return;
    }
    let kept: Arc<dyn MemoryStore> = previous.clone();
    current.keep_previous(kept);
    spawn_catch_up(Arc::clone(current), previous);
}

/// How often a catch-up runs: [`CATCH_UP_EVERY`], or, in tests, what
/// [`CATCH_UP_EVERY_IN_TESTS`] says (milliseconds, when not zero).
fn catch_up_every() -> Duration {
    #[cfg(test)]
    {
        let ms = CATCH_UP_EVERY_IN_TESTS.load(std::sync::atomic::Ordering::SeqCst);
        if ms > 0 {
            return Duration::from_millis(ms);
        }
    }
    CATCH_UP_EVERY
}

/// [`catch_up_every`] for tests that watch a catch-up happen.
#[cfg(test)]
pub(crate) static CATCH_UP_EVERY_IN_TESTS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Carry new memories and forgetting from `previous` into `current` while
/// replicas may still write to `previous`, then from `current` back into
/// `previous` for as long as `previous` is kept, so that switching back
/// loses nothing. Only the leader copies.
fn spawn_catch_up(current: Arc<dyn Switchable>, previous: Arc<dyn Switchable>) {
    tokio::spawn(async move {
        let since = chrono::Utc::now();
        let forward_until = tokio::time::Instant::now() + CATCH_UP_FOR;
        loop {
            tokio::time::sleep(catch_up_every()).await;
            if !crate::health_gauges::is_leader() {
                continue;
            }
            if tokio::time::Instant::now() < forward_until {
                if let Err(err) =
                    admin::sync(current.as_ref(), previous.as_ref(), SyncMode::CatchUp).await
                {
                    tracing::warn!(target: "embeddings", error = %err, "memory catch-up failed");
                }
                continue;
            }
            if let Err(err) = previous.doctor().await {
                tracing::info!(target: "embeddings", collection = previous.collection(), error = %err, "the previous collection is gone; no longer carrying memories back");
                return;
            }
            let back = SyncMode::Back { since };
            if let Err(err) = admin::sync(previous.as_ref(), current.as_ref(), back).await {
                tracing::warn!(target: "embeddings", error = %err, "carrying memories back to the previous collection failed");
            }
        }
    });
}

/// The store in use while a sync runs: the previous collection, replaced by
/// the new one once it holds every memory.
pub(crate) struct Switching(pub(crate) RwLock<Arc<dyn MemoryStore>>);

impl Switching {
    fn current(&self) -> Arc<dyn MemoryStore> {
        Arc::clone(&self.0.read())
    }
}

#[async_trait]
impl MemoryStore for Switching {
    async fn remember(&self, memory: Memory) -> Result<MemoryId> {
        self.current().remember(memory).await
    }
    async fn recall(&self, query: &str, filter: MemoryFilter) -> Result<Vec<MemorySearchResult>> {
        self.current().recall(query, filter).await
    }
    /// The store in use's: until the switch, the collection memories are
    /// moved from, which may take no vector or another model's.
    fn query_model(&self) -> Option<String> {
        self.current().query_model()
    }
    fn recall_settings(&self) -> Option<workforce_memory::store::RecallSettings> {
        self.current().recall_settings()
    }
    async fn recall_with_vector(
        &self,
        query: &str,
        vector: Option<Vec<f32>>,
        filter: MemoryFilter,
    ) -> Result<Vec<MemorySearchResult>> {
        self.current()
            .recall_with_vector(query, vector, filter)
            .await
    }
    async fn get(&self, id: &MemoryId) -> Result<Option<Memory>> {
        self.current().get(id).await
    }
    async fn forget(&self, id: &MemoryId) -> Result<()> {
        self.current().forget(id).await
    }
    async fn share(&self, id: &MemoryId, visibility: Visibility) -> Result<()> {
        self.current().share(id, visibility).await
    }
    async fn count(&self, filter: MemoryFilter) -> Result<usize> {
        self.current().count(filter).await
    }
    async fn list(&self, filter: MemoryFilter) -> Result<Vec<Memory>> {
        self.current().list(filter).await
    }
    async fn update_tags(&self, id: &MemoryId, tags: Vec<String>) -> Result<()> {
        self.current().update_tags(id, tags).await
    }
    async fn adjust_importance(&self, id: &MemoryId, delta: f32) -> Result<f32> {
        self.current().adjust_importance(id, delta).await
    }
    async fn identity_memories(
        &self,
        owner: &workforce_core::types::AgentId,
        identity: &str,
    ) -> Result<Vec<MemoryId>> {
        self.current().identity_memories(owner, identity).await
    }
    async fn identity_counts(
        &self,
        owner: &workforce_core::types::AgentId,
    ) -> Result<std::collections::BTreeMap<String, usize>> {
        self.current().identity_counts(owner).await
    }
    async fn reassign_identity(
        &self,
        owner: &workforce_core::types::AgentId,
        from: &str,
        to: &str,
    ) -> Result<usize> {
        self.current().reassign_identity(owner, from, to).await
    }
    async fn purge_identity(
        &self,
        owner: &workforce_core::types::AgentId,
        identity: &str,
    ) -> Result<usize> {
        self.current().purge_identity(owner, identity).await
    }
    async fn purge_memories(&self, ids: &[MemoryId]) -> Result<usize> {
        self.current().purge_memories(ids).await
    }
    async fn find_near_duplicate(
        &self,
        content: &str,
        filter: MemoryFilter,
        min_similarity: f32,
    ) -> Result<Option<MemorySearchResult>> {
        self.current()
            .find_near_duplicate(content, filter, min_similarity)
            .await
    }
    async fn record_recurrence(
        &self,
        id: &MemoryId,
        additional_message_ids: Vec<String>,
    ) -> Result<u32> {
        self.current()
            .record_recurrence(id, additional_message_ids)
            .await
    }
}

/// The store a side process (no startup sync) reads: the new collection
/// once a hub has switched to it, otherwise `v1`.
pub async fn switched_memory_store(
    config: &WorkforceConfig,
    v1: Arc<dyn Switchable>,
) -> Result<Arc<dyn Switchable>> {
    config.check_memory_backend()?;
    crate::promotion::guard_memories(config).await?;
    let Some((embedder, reranker)) = from_config(config).await? else {
        return Ok(v1);
    };
    let v2 = embedded_store(config, Backend::of(config), embedder, reranker)?;
    Ok(if v2.switched().await.unwrap_or(false) {
        if config.embeddings.keep_previous {
            let previous: Arc<dyn MemoryStore> = v1;
            v2.keep_previous(previous);
        }
        v2
    } else {
        v1
    })
}

/// Titan Text Embeddings v2 on Bedrock, signed with the hub's own AWS
/// identity (task role, or the static credentials the LLM path uses).
pub struct BedrockEmbeddings {
    client: reqwest::Client,
    resolver: CredentialResolver,
    region: String,
    model: String,
    dimensions: u32,
    permits: Arc<Semaphore>,
}

impl BedrockEmbeddings {
    pub fn new(model: &str, dimensions: u32, region: &str) -> Self {
        Self {
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .unwrap_or_default(),
            resolver: CredentialResolver::from_packed(CHAIN_SENTINEL),
            region: region.to_string(),
            model: model.to_string(),
            dimensions,
            permits: Arc::new(Semaphore::new(BEDROCK_CONCURRENCY)),
        }
    }

    fn url(&self) -> String {
        let encoded: String = self
            .model
            .chars()
            .map(|c| {
                if c == ':' {
                    "%3A".to_string()
                } else {
                    c.to_string()
                }
            })
            .collect();
        format!(
            "https://bedrock-runtime.{}.amazonaws.com/model/{encoded}/invoke",
            self.region
        )
    }

    async fn embed_one(&self, text: &str) -> Result<Vec<f32>> {
        let _permit = self.permits.acquire().await.map_err(|e| eyre!("{e}"))?;
        let input: String = if text.trim().is_empty() {
            " ".to_string()
        } else {
            text.chars().take(TITAN_MAX_CHARS).collect()
        };
        let body = serde_json::to_vec(&serde_json::json!({
            "inputText": input,
            "dimensions": self.dimensions,
            "normalize": true,
        }))?;
        let url = self.url();
        let mut delay = Duration::from_millis(250);
        for attempt in 0..5 {
            let creds = self.resolver.resolve().await?;
            let mut headers = vec![
                ("content-type".to_string(), "application/json".to_string()),
                ("accept".to_string(), "application/json".to_string()),
            ];
            headers.extend(sign_v4(
                "POST",
                &url,
                &headers,
                &body,
                &creds,
                &self.region,
                SystemTime::now(),
            )?);
            let mut req = self.client.post(&url).body(body.clone());
            for (k, v) in &headers {
                req = req.header(k.as_str(), v.as_str());
            }
            match req.send().await {
                Ok(resp) if resp.status().is_success() => {
                    let out: serde_json::Value = resp.json().await?;
                    return out["embedding"]
                        .as_array()
                        .map(|v| {
                            v.iter()
                                .filter_map(|x| x.as_f64().map(|f| f as f32))
                                .collect()
                        })
                        .ok_or_else(|| eyre!("Bedrock embedding response had no embedding"));
                }
                Ok(resp) => {
                    let status = resp.status();
                    let retry = status.as_u16() == 429 || status.is_server_error();
                    let text = resp.text().await.unwrap_or_default();
                    if !retry || attempt == 4 {
                        return Err(eyre!(
                            "Bedrock embedding returned {status}: {}",
                            text.chars().take(300).collect::<String>()
                        ));
                    }
                }
                Err(e) if attempt == 4 => {
                    return Err(eyre!("Bedrock embedding request failed: {e}"))
                }
                Err(_) => {}
            }
            tokio::time::sleep(delay).await;
            delay *= 2;
        }
        Err(eyre!("Bedrock embedding retries exhausted"))
    }
}

#[async_trait]
impl EmbeddingBackend for BedrockEmbeddings {
    async fn embed(&self, text: &str) -> Result<Vec<f32>> {
        self.embed_one(text).await
    }

    /// Titan takes one text per call, so many texts run concurrently,
    /// bounded by the permits.
    async fn embed_many(&self, texts: &[String], _kind: EmbedKind) -> Result<Vec<Vec<f32>>> {
        futures::future::try_join_all(texts.iter().map(|t| self.embed_one(t))).await
    }

    fn model_id(&self) -> &str {
        &self.model
    }
}

/// Remembers recent query embeddings: one turn recalls memories and lessons
/// with the same text.
pub struct QueryCached {
    inner: Arc<dyn EmbeddingBackend>,
    cache: Mutex<(HashMap<String, Vec<f32>>, VecDeque<String>)>,
}

impl QueryCached {
    pub fn new(inner: Arc<dyn EmbeddingBackend>) -> Self {
        Self {
            inner,
            cache: Mutex::new((HashMap::new(), VecDeque::new())),
        }
    }
}

#[async_trait]
impl EmbeddingBackend for QueryCached {
    async fn embed(&self, text: &str) -> Result<Vec<f32>> {
        self.inner.embed(text).await
    }

    async fn embed_many(&self, texts: &[String], kind: EmbedKind) -> Result<Vec<Vec<f32>>> {
        self.inner.embed_many(texts, kind).await
    }

    async fn embed_query(&self, text: &str) -> Result<Vec<f32>> {
        if let Some(v) = self.cache.lock().0.get(text) {
            return Ok(v.clone());
        }
        let v = self.inner.embed_query(text).await?;
        let mut guard = self.cache.lock();
        let (map, order) = &mut *guard;
        if map.insert(text.to_string(), v.clone()).is_none() {
            order.push_back(text.to_string());
            if order.len() > QUERY_CACHE {
                if let Some(old) = order.pop_front() {
                    map.remove(&old);
                }
            }
        }
        Ok(v)
    }

    fn model_id(&self) -> &str {
        self.inner.model_id()
    }
}

#[cfg(test)]
mod tests {
    /// Every public way to build a memory store refuses a `[memory]
    /// backend` it cannot use, in whichever process calls it (the hub, CLI
    /// memory commands, training, mcp_serving), and builds as before for
    /// every value it accepted before.
    #[tokio::test]
    async fn every_memory_store_builder_refuses_shadow_and_unknown_backends() {
        for (backend, says) in [
            ("shadow", "[memory] backend = \"shadow\""),
            ("mongodb", "[memory] backend must be one of"),
        ] {
            let mut config = WorkforceConfig::default();
            config.memory.backend = backend.into();
            let base = base_store(&config)
                .err()
                .expect("base_store refuses")
                .to_string();
            assert!(base.contains(says), "base_store, {backend}: {base}");
            let v1: Arc<dyn Switchable> = Arc::new(WeaviateStore::new("http://localhost:1", None));
            let memory = memory_store(&config, v1.clone(), None, Duration::from_millis(1))
                .await
                .err()
                .expect("memory_store refuses")
                .to_string();
            assert!(memory.contains(says), "memory_store, {backend}: {memory}");
            let switched = switched_memory_store(&config, v1)
                .await
                .err()
                .expect("switched_memory_store refuses")
                .to_string();
            assert!(
                switched.contains(says),
                "switched_memory_store, {backend}: {switched}"
            );
        }
        for backend in ["sqlite", "", "weaviate", "in_memory"] {
            let mut config = WorkforceConfig::default();
            config.memory.backend = backend.into();
            base_store(&config).unwrap_or_else(|e| panic!("{backend:?}: {e}"));
        }
    }

    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use workforce_memory::admin::{MemoryAdmin, SyncSource, SyncTarget};

    struct Counting(AtomicUsize);

    #[async_trait]
    impl EmbeddingBackend for Counting {
        async fn embed(&self, text: &str) -> Result<Vec<f32>> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(vec![text.len() as f32])
        }
    }

    #[tokio::test]
    async fn a_repeated_query_is_embedded_once() {
        let inner = Arc::new(Counting(AtomicUsize::new(0)));
        let cached = QueryCached::new(Arc::clone(&inner) as Arc<dyn EmbeddingBackend>);
        assert_eq!(cached.embed_query("who?").await.unwrap(), vec![4.0]);
        assert_eq!(cached.embed_query("who?").await.unwrap(), vec![4.0]);
        cached.embed_query("what?").await.unwrap();
        assert_eq!(inner.0.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn the_openai_key_is_read_from_the_global_vault_on_every_request() {
        use axum::{http::HeaderMap, routing::post, Json, Router};
        let seen: Arc<Mutex<Vec<Option<String>>>> = Arc::default();
        let record = Arc::clone(&seen);
        let app = Router::new().route(
            "/v1/embeddings",
            post(move |headers: HeaderMap| async move {
                record.lock().push(
                    headers
                        .get("authorization")
                        .map(|v| v.to_str().unwrap().to_string()),
                );
                Json(serde_json::json!({"data": [{"embedding": [1.0]}]}))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let dir = tempfile::tempdir().unwrap();
        let global = Arc::new(Vault::open(&dir.path().join("g.enc"), "test-key").unwrap());
        let embedder = |vault: Option<Arc<Vault>>, fallback: Option<&str>| {
            OpenAiEmbeddingBackend::with_key_source(
                &url,
                "m",
                openai_key(vault, fallback.map(str::to_string)),
            )
        };
        let from_vault = embedder(Some(Arc::clone(&global)), Some("env-key"));
        from_vault.embed("a").await.unwrap();
        global
            .set(EMBEDDINGS_OPENAI_API_KEY.path, "vault-key")
            .unwrap();
        from_vault.embed("a").await.unwrap();
        global
            .set(EMBEDDINGS_OPENAI_API_KEY.path, "rotated")
            .unwrap();
        from_vault.embed("a").await.unwrap();
        global
            .set(
                &format!("users/alice/{}", EMBEDDINGS_OPENAI_API_KEY.path),
                "alice",
            )
            .unwrap();
        workforce_agent::runner::CURRENT_USER_ID
            .scope("alice".into(), from_vault.embed("a"))
            .await
            .unwrap();
        embedder(None, None).embed("a").await.unwrap();
        assert_eq!(
            *seen.lock(),
            vec![
                Some("Bearer env-key".to_string()),
                Some("Bearer vault-key".to_string()),
                Some("Bearer rotated".to_string()),
                Some("Bearer rotated".to_string()),
                None,
            ]
        );
    }

    #[tokio::test]
    async fn an_error_body_that_echoes_the_openai_key_never_carries_it() {
        use axum::{http::HeaderMap, http::StatusCode, routing::post, Router};
        let app = Router::new().route(
            "/v1/embeddings",
            post(|headers: HeaderMap| async move {
                let sent = headers["authorization"].to_str().unwrap().to_string();
                (StatusCode::UNAUTHORIZED, format!("rejected {sent}"))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let embedder = OpenAiEmbeddingBackend::with_key_source(
            &url,
            "m",
            openai_key(None, Some("sk-very-secret".into())),
        );
        let error = embedder.embed("a").await.unwrap_err().to_string();
        assert!(error.contains("rejected Bearer <redacted>"), "{error}");
        assert!(!error.contains("sk-very-secret"), "{error}");
    }

    #[test]
    fn unset_embeddings_build_nothing_and_a_url_is_required_for_servers() {
        assert!(build(&EmbeddingsConfig::default(), None).unwrap().is_none());
        let tei = EmbeddingsConfig {
            provider: EmbeddingsProvider::Tei,
            ..Default::default()
        };
        assert!(build(&tei, None).is_err());
        let bedrock = EmbeddingsConfig {
            provider: EmbeddingsProvider::Bedrock,
            ..Default::default()
        };
        let (embedder, reranker) = build(&bedrock, None).unwrap().unwrap();
        assert_eq!(embedder.model_id(), "amazon.titan-embed-text-v2:0");
        assert!(reranker.is_none());
    }

    #[test]
    fn defaults_hold_without_toml_and_the_collection_is_named_after_the_model() {
        let d = EmbeddingsConfig::default();
        assert_eq!((d.alpha, d.rerank_candidates), (0.75, 30));
        let parsed: EmbeddingsConfig = toml::from_str("provider = \"bedrock\"").unwrap();
        assert_eq!((parsed.alpha, parsed.rerank_candidates), (0.75, 30));
        assert_eq!(
            parsed.collection_name(),
            "WorkforceMemory_amazon_titan_embed_text_v2_0"
        );
        let sized = EmbeddingsConfig {
            dimensions: Some(512),
            ..parsed.clone()
        };
        assert_eq!(
            sized.collection_name(),
            "WorkforceMemory_amazon_titan_embed_text_v2_0_512"
        );
        let named = EmbeddingsConfig {
            collection: Some("Mine".into()),
            ..parsed
        };
        assert_eq!(named.collection_name(), "Mine");
    }

    #[test]
    fn the_base_store_follows_the_memory_backend() {
        let mut config = WorkforceConfig::default();
        let weaviate = base_store(&config).unwrap();
        assert_eq!(
            weaviate.embedding_model(),
            None,
            "Weaviate embeds for itself"
        );
        config.memory.backend = "vellev".into();
        assert!(base_store(&config).is_err(), "a key is required");
        config.vellev.api_key = Some("key".into());
        config.vellev.tenant = "workforce".into();
        let vellev = base_store(&config).unwrap();
        assert_eq!(vellev.collection(), "WorkforceMemory");
        assert_eq!(vellev.embedding_model(), Some("BAAI/bge-base-en-v1.5"));
        config.vellev.tenant = "Not A Tenant".into();
        assert!(base_store(&config).is_err());
    }

    #[test]
    fn the_bedrock_url_encodes_the_model_version() {
        let b = BedrockEmbeddings::new("amazon.titan-embed-text-v2:0", 1024, "eu-west-2");
        assert_eq!(
            b.url(),
            "https://bedrock-runtime.eu-west-2.amazonaws.com/model/amazon.titan-embed-text-v2%3A0/invoke"
        );
    }

    /// The switch end to end against a REAL scratch Weaviate and an embedder
    /// speaking the text-embeddings-inference protocol. Ignored in CI:
    ///
    /// ```sh
    /// docker run -d --name weaviate-e2e -p 18080:8080 \
    ///   -e AUTHENTICATION_ANONYMOUS_ACCESS_ENABLED=true \
    ///   -e ENABLE_MODULES=text2vec-transformers \
    ///   -e TRANSFORMERS_INFERENCE_API=http://127.0.0.1:9 \
    ///   cr.weaviate.io/semitechnologies/weaviate:1.36.8
    /// WEAVIATE_E2E_URL=http://localhost:18080 EMBED_E2E_URL=http://localhost:18081 \
    ///   cargo test -p workforce-hub embeddings::tests::switch_e2e -- --ignored --nocapture
    /// ```
    ///
    /// Memories already in the old collection (stored with vectors, so its
    /// unreachable vectorizer is never called) are copied with their ids; the
    /// running store switches only once the copy is recorded; recall then
    /// ranks by the hub's vectors; forgetting reaches the old collection;
    /// new memories land in the new one.
    #[tokio::test]
    #[ignore = "needs a live scratch Weaviate and embedder; see doc comment"]
    async fn switch_e2e() {
        use workforce_memory::types::{MemoryLayer, MemorySource};
        let url = std::env::var("WEAVIATE_E2E_URL").expect("WEAVIATE_E2E_URL");
        let embed = std::env::var("EMBED_E2E_URL").expect("EMBED_E2E_URL");
        let mut config = WorkforceConfig::default();
        config.weaviate.url = url.clone();
        config.embeddings = EmbeddingsConfig {
            provider: EmbeddingsProvider::Tei,
            url: Some(embed),
            collection: Some("E2eMemV2".into()),
            ..Default::default()
        };
        let http = reqwest::Client::new();
        for class in ["WorkforceMemory", "E2eMemV2", "WorkforceMemorySwitch"] {
            http.delete(format!("{url}/v1/schema/{class}"))
                .send()
                .await
                .unwrap();
        }
        let v1 = WeaviateStore::new(&url, None);
        v1.ensure_collection().await.unwrap();
        let row = |id: &str, content: &str| {
            serde_json::json!({
                "id": id,
                "properties": {
                    "content": content, "memoryId": content, "layer": "semantic", "owner": "agent",
                    "visibility": "private", "importance": 0.5, "tags": [], "deleted": false,
                    "accessCount": 0, "createdAt": "2026-01-01T00:00:00Z",
                    "lastAccessed": "2026-01-01T00:00:00Z", "source": "human",
                    "eventTime": "2026-01-01T00:00:00Z"
                },
                "vector": vec![0.1f32; 384],
            })
        };
        let imported = v1
            .import(&[
                row("00000000-0000-0000-0000-00000000000a", "bananas are yellow"),
                row(
                    "00000000-0000-0000-0000-00000000000b",
                    "the server runs on port 8080",
                ),
                row("00000000-0000-0000-0000-00000000000c", "zzz quiet hours"),
            ])
            .await
            .unwrap();
        assert_eq!(imported, 3);

        let store = memory_store(&config, Arc::new(v1), None, Duration::from_secs(30))
            .await
            .unwrap();
        let probe = WeaviateStore::new(&url, None).with_embeddings(
            "E2eMemV2",
            build(&config.embeddings, None).unwrap().unwrap().0,
            None,
            0.75,
            30,
        );
        let mut waited = 0;
        while !probe.switched().await.unwrap() {
            assert!(waited < 120, "never switched");
            tokio::time::sleep(Duration::from_millis(500)).await;
            waited += 1;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;

        let filter = || MemoryFilter {
            limit: Some(3),
            system_unscoped: true,
            ..Default::default()
        };
        let hits = store.recall("banana yellow", filter()).await.unwrap();
        println!(
            "{:?}",
            hits.iter()
                .map(|h| (h.memory.content.clone(), h.score))
                .collect::<Vec<_>>()
        );
        assert_eq!(hits[0].memory.content, "bananas are yellow");
        assert_eq!(
            hits.len(),
            3,
            "vectors find every memory, not only keyword hits"
        );
        assert!(hits[0].score > 0.9, "certainty from the hub's vectors");

        let id = hits[0].memory.id.clone();
        store.forget(&id).await.unwrap();
        let old = WeaviateStore::new(&url, None).get(&id).await.unwrap();
        assert!(
            old.is_none_or(|m| m.deleted),
            "forgotten in the old collection too"
        );

        let mut fresh = Memory::new(
            "kiwis are green",
            MemoryLayer::Semantic,
            workforce_core::types::AgentId::new("agent"),
        );
        fresh.source = MemorySource::Human;
        store.remember(fresh).await.unwrap();
        let hits = store.recall("kiwi green", filter()).await.unwrap();
        println!(
            "{:?}",
            hits.iter()
                .map(|h| (h.memory.content.clone(), h.score))
                .collect::<Vec<_>>()
        );
        assert_eq!(hits[0].memory.content, "kiwis are green");
        let in_v1 = WeaviateStore::new(&url, None).export(false).await.unwrap();
        assert_eq!(in_v1.len(), 3, "new memories go to the new collection only");
    }
}
