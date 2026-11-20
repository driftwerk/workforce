//! Writing objects with vectors made outside the node.
//!
//! A vellev node embeds what is written to a collection it embeds for, one
//! run of its model at a time. Filling a large collection that way takes
//! as long as the node's model does. The node also takes a vector the
//! client sends with an object and stores it as it is, so the same model
//! served elsewhere, several requests at once, can make the vectors
//! instead: only if it is the same model, given the same text, pooled and
//! normalised alike, or what is stored will not be near what the node
//! makes of a query. [`equivalence`] measures that before anything is
//! trusted to it.

use std::sync::Arc;

use async_trait::async_trait;
use eyre::{eyre, Result};
use futures::stream::{self, StreamExt, TryStreamExt};
use serde_json::{json, Map, Value};
use uuid::Uuid;
use vellev_client::api::{Change, Content, Page};
use vellev_core::{Collection, CollectionName, DataType, Embedding, ObjectId, Property};

use crate::{classified, collection, failure, Asked, Cap, Tenants};

/// Makes the vectors of documents, outside the node.
#[async_trait]
pub trait DocumentEmbedder: Send + Sync {
    /// What it is, for messages: the endpoint and the model.
    fn name(&self) -> String;
    /// A vector for each text, in order.
    async fn documents(&self, texts: &[String]) -> Result<Vec<Vec<f32>>>;
}

/// How objects are written with vectors made outside the node.
#[derive(Clone)]
pub struct Seeding {
    pub embedder: Arc<dyn DocumentEmbedder>,
    /// Objects per request, to the embedder and to the node.
    pub batch: usize,
    /// Requests to the embedder at once: over everything written with
    /// this seeding and its clones, whichever collection, page or repo
    /// each is for.
    pub concurrency: usize,
    /// One for each request the embedder may be working on.
    slots: Arc<tokio::sync::Semaphore>,
}

/// How many pages are written at a time for the embedder to be kept
/// busy: while one is being written to the node, read again and recorded,
/// the requests of the next are with the embedder, and those of the ones
/// after wait their turn.
const PAGES_AT_A_TIME: usize = 4;

impl Seeding {
    pub fn new(embedder: Arc<dyn DocumentEmbedder>, batch: usize, concurrency: usize) -> Self {
        let (batch, concurrency) = (batch.max(1), concurrency.max(1));
        Self {
            embedder,
            batch,
            concurrency,
            slots: Arc::new(tokio::sync::Semaphore::new(concurrency)),
        }
    }

    /// How many objects make a page: as many as the embedder is sent at
    /// once.
    pub fn page(&self) -> usize {
        self.batch * self.concurrency
    }

    /// How many pages to write at a time, each as long as [`Self::page`],
    /// for the embedder to have its requests however long a page takes to
    /// be written and recorded: what is held in memory at once, with its
    /// vectors, is this many pages.
    pub fn pages_at_a_time(&self) -> usize {
        PAGES_AT_A_TIME
    }
}

impl std::fmt::Debug for Seeding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Seeding")
            .field("embedder", &self.embedder.name())
            .field("batch", &self.batch)
            .field("concurrency", &self.concurrency)
            .finish()
    }
}

/// The text of `properties` that a collection embedding as `embedding`
/// turns into a vector, exactly as the node arrives at it (vellev-core's
/// `Object::embedded_text`): the listed properties' text values, in order,
/// a line each, after the collection's document prefix.
pub fn embedded_text(embedding: &Embedding, properties: &Map<String, Value>) -> String {
    let lines: Vec<&str> = embedding
        .properties
        .iter()
        .filter_map(|name| properties.get(name))
        .flat_map(|value| match value {
            Value::String(text) => vec![text.as_str()],
            Value::Array(texts) => texts.iter().filter_map(Value::as_str).collect(),
            _ => Vec::new(),
        })
        .collect();
    let text = lines.join("\n");
    match &embedding.document_prefix {
        Some(prefix) if !text.trim().is_empty() => format!("{prefix}{text}"),
        _ => text,
    }
}

/// Apply `changes` to `wanted`'s collection in the tenant `at`, through its
/// writes, giving each upsert that has
/// text to embed a vector made by `seeding`'s embedder: `seeding.batch`
/// to a request, each batch one atomic step. However many calls are under
/// way, the embedder has no more than `seeding.concurrency` requests at
/// once; a batch being written to the node holds none of them up. Returns how many changes were applied; `Err` at the first
/// batch that fails, the others under way finishing or not.
pub async fn apply_embedded(
    at: &Tenants,
    wanted: &Collection,
    seeding: &Seeding,
    changes: Vec<Change>,
) -> Result<usize> {
    let batches: Vec<Vec<Change>> = changes
        .chunks(seeding.batch.max(1))
        .map(<[Change]>::to_vec)
        .collect();
    stream::iter(batches)
        .map(|mut batch| async move {
            let embedded: Vec<(usize, String)> = batch
                .iter()
                .enumerate()
                .filter_map(|(at, change)| match change {
                    Change::Upsert { properties, .. } | Change::Insert { properties, .. } => {
                        Some((at, embedded_text(&wanted.embedding, properties)))
                    }
                    _ => None,
                })
                .filter(|(_, text)| !text.trim().is_empty())
                .collect();
            let texts: Vec<String> = embedded.iter().map(|(_, text)| text.clone()).collect();
            let vectors = if texts.is_empty() {
                Vec::new()
            } else {
                let _slot = seeding.slots.acquire().await?;
                seeding.embedder.documents(&texts).await?
            };
            if vectors.len() != texts.len() {
                return Err(eyre!(
                    "{} gave {} vectors for {} texts",
                    seeding.embedder.name(),
                    vectors.len(),
                    texts.len()
                ));
            }
            for ((at, _), made) in embedded.into_iter().zip(vectors) {
                if made.len() != wanted.embedding.dimensions as usize {
                    return Err(eyre!(
                        "{} gave a vector of {} dimensions, and collection '{}' holds {}",
                        seeding.embedder.name(),
                        made.len(),
                        wanted.name,
                        wanted.embedding.dimensions
                    ));
                }
                match &mut batch[at] {
                    Change::Upsert { vector, .. } | Change::Insert { vector, .. } => {
                        *vector = Some(made)
                    }
                    _ => {}
                }
            }
            let size = batch.len();
            at.writes
                .apply(batch)
                .await
                .map_err(|e| classified(e, at))?;
            Ok(size)
        })
        .buffer_unordered(seeding.concurrency.max(1) * 2)
        .try_fold(0, |applied, size| async move { Ok(applied + size) })
        .await
}

/// The fewest texts an embedder is compared with the node over before it
/// is trusted. [`equivalence`] always has as many: it adds
/// [`STANDING_SAMPLE`] to whatever it is given.
pub const MIN_SAMPLE: usize = 16;

/// Texts every comparison includes, whatever else it is given: prose and
/// code, short and long, so that a small corpus is still a fair sample.
const STANDING_SAMPLE: [&str; 16] = [
    "pub fn parse_config(path: &Path) -> Result<Config>",
    "Reads the TOML configuration file and applies environment overrides.",
    "impl Scheduler { pub async fn tick(&mut self) -> Vec<JobRun> }",
    "SELECT id, title FROM artifacts WHERE owner_id = $1 ORDER BY updated_at DESC",
    "How to roll back a release that went wrong",
    "class InvoiceRenderer(BaseRenderer):\n    def render(self, invoice): ...",
    "function debounce(fn, wait) { let t; return (...args) => { clearTimeout(t); t = setTimeout(() => fn(...args), wait); }; }",
    "Deploy runbook\n\ntags: ops, release\n\nStop the workers, migrate, start the workers, watch the queue.",
    "x",
    "TODO",
    "The quarterly review covers churn, expansion revenue and support load across the three regions.",
    "func (s *Server) ListenAndServe(ctx context.Context) error",
    "error[E0382]: use of moved value: `records`",
    "Lunch menu: soup and bread on Fridays.",
    "A summary of the module: it owns the retry policy for outbound webhooks and records each attempt.",
    "struct Point { x: f64, y: f64 } // a point in the plane, in metres from the origin of the site survey, east then north",
];

/// Drops the collection made to embed with the node, however the embedding
/// ends: explicitly when it runs to its end, and from a task of its own if
/// it is dropped part of the way.
struct Probe {
    records: Option<vellev_client::Records>,
    name: CollectionName,
}

impl Probe {
    /// The probe was never made: its create was refused outright, so
    /// there is nothing to drop.
    fn never_made(&mut self) {
        self.records = None;
    }

    /// Drop the probe from the tenant `at`, where it embeds with `model`.
    async fn dropped(mut self, at: &Tenants, model: &str) -> Result<()> {
        let Some(records) = self.records.take() else {
            return Ok(());
        };
        match records.drop().await {
            Err(e) if !e.is_not_found() => {
                let asked = Asked::Request {
                    name: &self.name,
                    model,
                };
                Err(failure(e, at, asked).wrap_err(format!(
                    "the collection made to embed with the node, '{}', was not dropped",
                    self.name
                )))
            }
            _ => Ok(()),
        }
    }
}

impl Drop for Probe {
    fn drop(&mut self) {
        let (Some(records), Ok(runtime)) =
            (self.records.take(), tokio::runtime::Handle::try_current())
        else {
            return;
        };
        runtime.spawn(async move {
            let _ = records.drop().await;
        });
    }
}

/// The node itself as an embedder: texts are written `per_write` at a time
/// to a collection made for them and read back with their vectors, and the
/// collection is dropped. A question is embedded alone, so one text a write
/// gives the vector the node searches with.
pub struct NodeEmbedder {
    tenants: Tenants,
    embedding: Embedding,
    per_write: usize,
    cap: Arc<Cap>,
}

impl NodeEmbedder {
    /// Embedding with `tenants`' node as `embedding` does, `per_write`
    /// texts to a write (at least one).
    pub fn new(tenants: Tenants, embedding: Embedding, per_write: usize) -> Self {
        Self {
            tenants,
            embedding,
            per_write: per_write.max(1),
            cap: Arc::default(),
        }
    }

    /// Listing the probe within `cap`, the store's, so that a lower limit
    /// the node gave it is not learned again.
    pub fn sharing(self, cap: Arc<Cap>) -> Self {
        Self { cap, ..self }
    }

    fn probe(&self) -> Result<Collection> {
        Ok(collection(
            CollectionName::new(format!("WorkforceEmbedProbe{}", Uuid::new_v4().simple()))?,
            vec![Property {
                filterable: false,
                ..Property::new("text", DataType::Text)
            }],
            Embedding {
                properties: vec!["text".to_string()],
                ..self.embedding.clone()
            },
        ))
    }

    async fn made(
        &self,
        probe: &Collection,
        armed: &mut Probe,
        records: &vellev_client::Records,
        texts: &[String],
    ) -> Result<Vec<Vec<f32>>> {
        let failed = |e| classified(e, &self.tenants);
        if let Err(e) = records.create(probe).await {
            if refused_outright(&e) {
                armed.never_made();
            }
            return Err(failed(e));
        }
        let ids: Vec<ObjectId> = texts.iter().map(|_| ObjectId(Uuid::new_v4())).collect();
        let written: Vec<(&ObjectId, &String)> = ids.iter().zip(texts).collect();
        for batch in written.chunks(self.per_write) {
            let changes = batch
                .iter()
                .map(|(id, text)| {
                    Change::upsert(
                        probe.name.clone(),
                        **id,
                        Content::new(Map::from_iter([("text".to_string(), json!(text))])),
                    )
                })
                .collect();
            self.tenants.writes.apply(changes).await.map_err(failed)?;
        }
        let mut by_node = std::collections::HashMap::new();
        let mut after = None;
        loop {
            let listed = self
                .cap
                .ask(PROBE_PAGE, |limit| {
                    let page = after
                        .into_iter()
                        .fold(Page::new(limit).vectors(true), Page::after);
                    async move { records.list(&page).await }
                })
                .await
                .map_err(failed)?;
            for object in listed.objects {
                let vector = object.vector.ok_or_else(|| {
                    eyre!(
                        "{} listed object {} of the probe '{}' with no vector",
                        self.name(),
                        object.id.0,
                        probe.name
                    )
                })?;
                by_node.insert(object.id, vector);
            }
            after = listed.next;
            if after.is_none() {
                break;
            }
        }
        let vectors: Vec<Vec<f32>> = ids.iter().filter_map(|id| by_node.remove(id)).collect();
        if vectors.len() != texts.len() {
            return Err(eyre!(
                "{} gave back vectors for {} of {} texts",
                self.name(),
                vectors.len(),
                texts.len()
            ));
        }
        Ok(vectors)
    }
}

#[async_trait]
impl DocumentEmbedder for NodeEmbedder {
    fn name(&self) -> String {
        format!(
            "the vellev node at {} ({})",
            self.tenants.node, self.embedding.model
        )
    }

    async fn documents(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let probe = self.probe()?;
        let records = self.tenants.reads.collection(&probe.name);
        let mut made = Probe {
            records: Some(records.clone()),
            name: probe.name.clone(),
        };
        match (
            self.made(&probe, &mut made, &records, texts).await,
            made.dropped(&self.tenants, &self.embedding.model).await,
        ) {
            (Ok(vectors), Ok(())) => Ok(vectors),
            (Err(e), Ok(())) | (Ok(_), Err(e)) => Err(e),
            (Err(e), Err(dropped)) => {
                Err(e.wrap_err(format!("the embedding failed, and then {dropped:#}")))
            }
        }
    }
}

/// Whether the node turned a request away for certain, leaving nothing
/// done: its own refusal of the request, never a timeout or an answer that
/// leaves the outcome open.
fn refused_outright(error: &vellev_client::Error) -> bool {
    match error {
        vellev_client::Error::Api { status, .. } => status.is_client_error(),
        vellev_client::Error::Limited { .. } => true,
        _ => false,
    }
}

/// Texts per request when an embedder is compared with the node: no more
/// than text-embeddings-inference takes in one by default.
const PROBE_BATCH: usize = 32;

/// Probe objects listed a page at a time, with their vectors.
const PROBE_PAGE: usize = 200;

/// The least cosine between a vector the node makes of a text and one made
/// outside it for the two to be taken as the same model's. Measured
/// between the node's bge-base-en-v1.5 (ONNX, by fastembed) and the same
/// model served by text-embeddings-inference, the least over code and
/// prose was above 0.9999: the two differ only by
/// their arithmetic. A wrong prefix, another pooling or another model each
/// falls far below this.
pub const MIN_COSINE: f32 = 0.999;

/// How near an outside embedder's vectors are to the node's, over a sample.
#[derive(Debug, Clone, PartialEq)]
pub struct Equivalence {
    /// Texts compared.
    pub compared: usize,
    /// The least cosine between the node's vector of a text and the
    /// embedder's.
    pub lowest: f32,
    pub mean: f32,
    /// The query prefix the collection puts before a query, if it has one.
    pub query_prefix: Option<String>,
    /// The mean cosine between the embedder's vector of a text and the
    /// node's vector of the text after [`Self::query_prefix`]: high when
    /// the embedder puts that prefix before documents. `None` with no
    /// prefix, when nothing could be confused.
    pub mean_against_prefixed: Option<f32>,
    /// The mean length of the embedder's vectors: the node's are 1.
    pub mean_length: f32,
}

impl Equivalence {
    /// `Err`, saying what probably differs, unless every text compared is
    /// at least `least` near.
    pub fn checked(&self, embedder: &str, least: f32) -> Result<()> {
        if self.compared < MIN_SAMPLE {
            return Err(eyre!(
                "{embedder} was compared with the node over {} texts, and at least {MIN_SAMPLE} are needed to trust it. Nothing was written",
                self.compared
            ));
        }
        if self.lowest >= least {
            return Ok(());
        }
        let prefixed = self
            .query_prefix
            .as_ref()
            .zip(self.mean_against_prefixed)
            .filter(|(_, mean)| *mean >= least);
        let differs = if let Some((prefix, _)) = prefixed {
            format!(
                "it puts the query prefix ({prefix:?}) before documents, which the node does not: send documents with no prefix"
            )
        } else if (self.mean_length - 1.0).abs() > 0.01 && self.lowest > 0.99 {
            format!(
                "its vectors are not normalised (mean length {:.3}), and the model's training or pooling may differ slightly: have it normalise",
                self.mean_length
            )
        } else {
            "it adds a prefix of its own, pools otherwise (bge takes the first token's vector, not the mean), cuts texts at another length, or serves another model".to_string()
        };
        Err(eyre!(
            "{embedder} does not embed as the node does: over {} texts the least cosine is {:.5} and the mean {:.5}, and {least} is needed. Probably {differs}. Nothing was written",
            self.compared,
            self.lowest,
            self.mean
        ))
    }
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b).map(|(a, b)| a * b).sum();
    let length = |v: &[f32]| v.iter().map(|x| x * x).sum::<f32>().sqrt();
    let lengths = length(a) * length(b);
    if a.len() != b.len() || lengths == 0.0 {
        return 0.0;
    }
    dot / lengths
}

/// Measure how near `embedder`'s vectors of `texts` are to those the node
/// makes of them, embedding as `embedding` does ([`NodeEmbedder`]). Each
/// is also embedded by the node after the embedding's query prefix, if it
/// has one, to tell an embedder that puts it before documents.
pub async fn equivalence(
    tenants: &Tenants,
    embedding: &Embedding,
    embedder: &dyn DocumentEmbedder,
    texts: &[String],
) -> Result<Equivalence> {
    let documents: Vec<String> = texts
        .iter()
        .map(String::as_str)
        .chain(STANDING_SAMPLE)
        .filter(|text| !text.trim().is_empty())
        .map(str::to_string)
        .collect();
    let prefix = embedding.query_prefix.as_deref();
    let written: Vec<String> = documents
        .iter()
        .cloned()
        .chain(
            prefix
                .into_iter()
                .flat_map(|prefix| documents.iter().map(move |text| format!("{prefix}{text}"))),
        )
        .collect();
    let node = NodeEmbedder::new(tenants.clone(), embedding.clone(), PROBE_BATCH);
    let by_node = node.documents(&written).await?;
    let (plain, prefixed) = by_node.split_at(documents.len());
    let mut by_embedder = Vec::with_capacity(documents.len());
    for batch in documents.chunks(PROBE_BATCH) {
        by_embedder.extend(embedder.documents(batch).await?);
    }
    if by_embedder.len() != documents.len() {
        return Err(eyre!(
            "{} gave {} vectors for {} texts",
            embedder.name(),
            by_embedder.len(),
            documents.len()
        ));
    }
    let compared = by_embedder.len();
    let near: Vec<f32> = plain
        .iter()
        .zip(&by_embedder)
        .map(|(node, outside)| cosine(node, outside))
        .collect();
    let mean = |sum: f32| match compared {
        0 => 0.0,
        compared => sum / compared as f32,
    };
    let against_prefixed: f32 = prefixed
        .iter()
        .zip(&by_embedder)
        .map(|(node, outside)| cosine(node, outside))
        .sum();
    let lengths: f32 = by_embedder
        .iter()
        .map(|outside| outside.iter().map(|x| x * x).sum::<f32>().sqrt())
        .sum();
    Ok(Equivalence {
        compared,
        lowest: near.iter().copied().reduce(f32::min).unwrap_or(0.0),
        mean: mean(near.iter().sum()),
        query_prefix: prefix.map(str::to_string),
        mean_against_prefixed: prefix.map(|_| mean(against_prefixed)),
        mean_length: mean(lengths),
    })
}
