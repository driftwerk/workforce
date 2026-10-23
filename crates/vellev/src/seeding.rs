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
use vellev_client::api::{Change, Page};
use vellev_client::Tenant;
use vellev_core::{Collection, CollectionName, DataType, Embedding, ObjectId, Property};

use crate::{collection, Tenants, LOCAL_QUERY_PREFIX};

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

/// Apply `changes` to `wanted`'s collection, giving each upsert that has
/// text to embed a vector made by `seeding`'s embedder: `seeding.batch`
/// to a request, each batch one atomic step. However many calls are under
/// way, the embedder has no more than `seeding.concurrency` requests at
/// once; a batch being written to the node holds none of them up. Returns how many changes were applied; `Err` at the first
/// batch that fails, the others under way finishing or not.
pub async fn apply_embedded(
    writes: &Tenant,
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
            writes.apply(batch).await?;
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

/// Drops the collection made to compare embeddings, however the comparison
/// ends: explicitly when it runs to its end, and from a task of its own if
/// it is dropped part of the way.
struct Probe {
    records: Option<vellev_client::Records>,
    name: CollectionName,
}

impl Probe {
    async fn dropped(mut self) {
        if let Some(records) = self.records.take() {
            if let Err(e) = records.drop().await {
                tracing::warn!(collection = %self.name, error = %e, "the collection made to compare embeddings was not dropped");
            }
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

/// Texts per request when an embedder is compared with the node: no more
/// than text-embeddings-inference takes in one by default.
const PROBE_BATCH: usize = 32;

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
    /// The mean cosine between the embedder's vector of a text and the
    /// node's vector of the text after the query prefix: high when the
    /// embedder puts that prefix before documents.
    pub mean_against_prefixed: f32,
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
        let differs = if self.mean_against_prefixed >= least {
            format!(
                "it puts the query prefix ({LOCAL_QUERY_PREFIX:?}) before documents, which the node does not: send documents with no prefix"
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
/// makes of them, embedding as `embedding` does: the texts are written to
/// a collection made for the purpose, which the node embeds, read back
/// with their vectors, and the collection dropped.
pub async fn equivalence(
    tenants: &Tenants,
    embedding: &Embedding,
    embedder: &dyn DocumentEmbedder,
    texts: &[String],
) -> Result<Equivalence> {
    let standing: Vec<String> = STANDING_SAMPLE
        .iter()
        .map(|text| text.to_string())
        .collect();
    let texts: Vec<&String> = texts
        .iter()
        .chain(&standing)
        .filter(|text| !text.trim().is_empty())
        .collect();
    let probe = collection(
        CollectionName::new(format!("WorkforceEmbedProbe{}", Uuid::new_v4().simple()))?,
        vec![Property {
            filterable: false,
            ..Property::new("text", DataType::Text)
        }],
        Embedding {
            properties: vec!["text".to_string()],
            ..embedding.clone()
        },
    );
    // Before it is made: a comparison given up while the node is making
    // the collection still has it dropped.
    let made = Probe {
        records: Some(tenants.reads.collection(&probe.name)),
        name: probe.name.clone(),
    };
    let records = tenants.reads.collection(&probe.name);
    records.create(&probe).await?;
    let measured = async {
        // Each text as it is, and after the query prefix.
        let prefixed = |text: &String| format!("{LOCAL_QUERY_PREFIX}{text}");
        let written: Vec<(ObjectId, String)> = texts
            .iter()
            .flat_map(|text| [(*text).clone(), prefixed(text)])
            .map(|text| (ObjectId(Uuid::new_v4()), text))
            .collect();
        for batch in written.chunks(32) {
            let changes = batch
                .iter()
                .map(|(id, text)| Change::Upsert {
                    collection: probe.name.clone(),
                    id: *id,
                    properties: json!({ "text": text })
                        .as_object()
                        .cloned()
                        .unwrap_or_default(),
                    vector: None,
                })
                .collect();
            tenants.writes.apply(changes).await?;
        }
        let mut by_node = std::collections::HashMap::new();
        let mut after = None;
        loop {
            let page = Page {
                filter: None,
                after,
                limit: 200,
                vectors: true,
            };
            let listed = records.list(&page).await?;
            for object in listed.objects {
                by_node.insert(object.id, object.vector.unwrap_or_default());
            }
            after = listed.next;
            if after.is_none() {
                break;
            }
        }
        let documents: Vec<String> = texts.iter().map(|text| (*text).clone()).collect();
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
        let (mut lowest, mut sum, mut sum_prefixed, mut sum_length) = (1.0f32, 0.0, 0.0, 0.0);
        for (pair, outside) in written.chunks(2).zip(&by_embedder) {
            let node = |at: usize| by_node.get(&pair[at].0).map(Vec::as_slice).unwrap_or(&[]);
            let near = cosine(node(0), outside);
            lowest = lowest.min(near);
            sum += near;
            sum_prefixed += cosine(node(1), outside);
            sum_length += outside.iter().map(|x| x * x).sum::<f32>().sqrt();
        }
        let compared = by_embedder.len();
        let mean = |sum: f32| {
            if compared == 0 {
                0.0
            } else {
                sum / compared as f32
            }
        };
        Ok(Equivalence {
            compared,
            lowest: if compared == 0 { 0.0 } else { lowest },
            mean: mean(sum),
            mean_against_prefixed: mean(sum_prefixed),
            mean_length: mean(sum_length),
        })
    }
    .await;
    made.dropped().await;
    measured
}
