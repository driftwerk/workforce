//! The [`CodeVectorStore`] on vellev: `CodeEntity` and `CodeSummary`
//! collections of one tenant, named as in Weaviate and embedded by the
//! node with the configured code model (bge-base unless `[vellev.code]`
//! names another), so nothing but the database changes for the indexer.
//!
//! A collection is made sure of before its first use, and again before
//! each use while the node does not answer. One of the wrong shape is
//! refused for good: nothing is written to or read from it, and the
//! readiness check reports why. That is what becomes of a collection
//! vellev's importer copied from Weaviate, holding `text2vec-transformers`
//! vectors the node cannot embed queries to match: it is to be dropped,
//! and a full re-index rebuilds it from the repositories.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use eyre::Result;
use serde_json::{json, Map, Value};
use tracing::{debug, info, warn};
use uuid::Uuid;
use vellev_client::api::{Change, Changes, Content, Nearest, OnlyIf, Page};
use vellev_client::Client;
use vellev_core::{Collection, CollectionName, DataType, Embedding, ObjectId, Property};
use vellev_index::filter::Filter;
use workforce_common::settings::{CodeModel, QueryPrefix, VellevConfig};
use workforce_vellev::seeding::{apply_embedded, Seeding};
use workforce_vellev::{
    collection, embedding_of, failure, is_conflict, local_embedding, Asked, Cap, Ensured, Unusable,
    LOCAL_MODEL, LOCAL_QUERY_PREFIX, RECHECK_AFTER,
};

use super::health::{InsertErrorSink, InsertErrorSlot};
use super::{
    count_embedded, entity_text, search_result, search_text, summary_text, vectorizable,
    CodeVectorStore, DeleteOutcome, Differing, HeldRecord, VectorCodeEntity, VectorCodeSummary,
    VectorSearchResult, CODE_ENTITY_COLLECTION, CODE_SUMMARY_COLLECTION, VERIFY_PAGE,
};
use crate::graph::entities::EntityKind;
use crate::graph::VectorKeys;

/// Records per delete, or per patch of their fills, each one atomic step.
const INDEX_BATCH: usize = 50;

/// Records per write the node embeds, one write at a time: the most a
/// store has the node embedding at once when nothing outside the node
/// makes the vectors. A node runs its model on one request's texts at a
/// time, and a question waits for the request under way: at 50 to 200 a
/// write, as a fill or a seeding run with no `--embed-url` sent them, a
/// question asked meanwhile waited seconds (7 s was measured behind 160
/// queued texts). Sixteen keeps that wait to about what one question's
/// own embedding costs several times over, whatever the pace of the fill.
/// The node's own answer, embedding questions ahead of writes, is vellev's
/// to make (vellev PR #48); until a node has it this bound is what keeps
/// questions answered.
pub const NODE_EMBEDS_AT_ONCE: usize = 16;

/// The property a record's fill generation is kept in (see
/// [`VectorCodeEntity::generation`]).
const FILL: &str = "fill";

/// The property a record's hash is kept in (see
/// [`VectorCodeEntity::record_hash`]): filterable, so that a page of
/// records is compared with the store by one count.
const HASH: &str = "textHash";

/// The property that tells one entity of a repo from another, as the graph
/// does.
const ENTITY_KEY: &str = "entityId";

/// The property that tells one summary of a repo from another.
const SUMMARY_KEY: &str = "summaryKey";

/// What, in a tenant, records which code graph its code vectors are of.
const OWNER: &str = "CodeGraph";

/// How long a readiness check may take.
/// For how long a store that found the tenant's code vectors its graph's
/// takes them to be so still, before a write. Before anything is removed
/// it looks again whatever the time.
pub const CLAIM_RECHECK: Duration = Duration::from_secs(60);

const HEALTH_TIMEOUT: Duration = Duration::from_secs(5);

/// What to do about a code collection holding vectors the node cannot embed
/// queries to match: code vectors are rebuilt from the repositories.
const REINDEX: &str = "drop it, then run a full index of each repository (the knowledge tool's `trigger_full` action, or `POST /api/knowledge/index` with `{\"mode\": \"full\"}`), which rebuilds its vectors from the code";

/// The objects of entities about to be written that the collection holds
/// as they are, but for their fill.
pub(crate) type Matched = HashSet<ObjectId>;

/// How the code collections embed for `model`, as `[vellev.code]` names
/// it: today's bge-base and its query prefix when it names none. An unset
/// prefix is bge's only for bge-base, the model it belongs to.
pub fn code_embedding(model: Option<CodeModel<'_>>) -> Embedding {
    let Some(CodeModel {
        model,
        dimensions,
        query_prefix,
    }) = model
    else {
        return local_embedding(&[]);
    };
    let query_prefix = match query_prefix {
        QueryPrefix::Unset if model == LOCAL_MODEL => Some(LOCAL_QUERY_PREFIX),
        QueryPrefix::Unset | QueryPrefix::None => None,
        QueryPrefix::Given(prefix) => Some(prefix),
    };
    embedding_of(model, dimensions, query_prefix, &[])
}

/// What was last found of whose the tenant's code vectors are, and when.
#[derive(Clone)]
enum Owned {
    Ours(Instant),
    /// The node does not serve the tenant.
    Misrouted(Unusable, Instant),
}

/// The [`CodeVectorStore`] on vellev.
pub struct VellevCodeVectorStore {
    base_url: String,
    tenants: workforce_vellev::Tenants,
    /// How the collections embed, but for what.
    local: Embedding,
    entities: Ensured,
    summaries: Ensured,
    /// The most results the node gives for one request.
    cap: Cap,
    /// Asks the node whether it is ready, without the tenant's key.
    health: Client,
    insert_errors: InsertErrorSlot,
    /// When set, vectors are made outside the node and sent with what is
    /// written.
    seeding: Option<Seeding>,
    /// The code graph these vectors are kept for, when it is known.
    graph: std::sync::OnceLock<String>,
    /// When the tenant's code vectors were last found to be this graph's,
    /// or its node found not to serve the tenant.
    owned: std::sync::Mutex<Option<Owned>>,
    /// For how long that is taken as still so, by a write.
    claim_recheck: Duration,
}

impl VellevCodeVectorStore {
    /// The code vectors of `tenant` on the node at `url`, reached with
    /// `api_key`; every request is bounded by the client's timeout.
    pub fn new(url: &str, api_key: &str, tenant: &str) -> Result<Self> {
        let tenants = workforce_vellev::connect(url, api_key, tenant)?;
        let local = local_embedding(&[]);
        let (entities, summaries) = collections(&tenants, &local)?;
        Ok(Self {
            base_url: url.to_string(),
            entities,
            summaries,
            tenants,
            local,
            cap: Cap::default(),
            health: Client::new(url, HEALTH_TIMEOUT)?,
            insert_errors: InsertErrorSlot::default(),
            seeding: None,
            graph: std::sync::OnceLock::new(),
            owned: std::sync::Mutex::new(None),
            claim_recheck: CLAIM_RECHECK,
        })
    }

    /// Keep these vectors for the code graph `graph` (its identity), and
    /// for no other: a tenant's code vectors are one graph's. The first
    /// graph to write claims them; another is refused, since two graphs
    /// may hold repos of one id, and each would take the other's vectors
    /// for stale ones of its own and remove them.
    pub fn for_graph(self, graph: &str) -> Self {
        self.keep_for_graph(graph);
        self
    }

    /// The store `vellev` configures for code vectors: on the node, tenant
    /// and key `[vellev.code]` names, each `[vellev]`'s where it does not,
    /// embedding with the code model it names.
    pub fn configured(vellev: &VellevConfig) -> Result<Self> {
        let node = vellev.code_node();
        node.reachable("[knowledge] vector_backend = \"vellev\"")?;
        let key = node.api_key.map_or("", |key| key.value);
        Self::new(node.url.value, key, node.tenant.value)?
            .embedding(code_embedding(vellev.code_model()?))
    }

    /// Have the collections embed as `local` does, but for what: the
    /// configured code model ([`code_embedding`]). Before first use, as
    /// the other builders.
    pub fn embedding(self, local: Embedding) -> Result<Self> {
        let (entities, summaries) = collections(&self.tenants, &local)?;
        Ok(Self {
            local,
            entities,
            summaries,
            ..self
        })
    }

    /// Look at whose the tenant's code vectors are no less often than
    /// every `recheck` before a write.
    pub fn rechecking(mut self, recheck: Duration) -> Self {
        self.claim_recheck = recheck;
        self
    }

    /// `Err` unless the tenant's code vectors are this store's graph's,
    /// claiming them if they are nobody's yet. Whose they are is read
    /// again if it was last read longer ago than [`CLAIM_RECHECK`]: they
    /// may have been taken over for another graph since
    /// ([`CodeVectorStore::take_over`]), and a process still running for
    /// the graph they were taken from then stops writing, within that
    /// time.
    async fn owned(&self) -> Result<()> {
        self.owned_within(self.claim_recheck).await
    }

    /// As [`Self::owned`], reading whose they are now, whenever it was
    /// last read: before anything is removed. A process whose graph the
    /// vectors were taken from removes nothing of the graph that has them
    /// now, however lately it looked.
    async fn owned_now(&self) -> Result<()> {
        self.owned_within(Duration::ZERO).await
    }

    /// A node found not to serve the tenant is taken not to, without
    /// asking, until [`RECHECK_AFTER`], except before anything is removed.
    async fn owned_within(&self, recheck: Duration) -> Result<()> {
        let Some(graph) = self.graph.get() else {
            return Ok(());
        };
        let owned = || self.owned.lock().unwrap_or_else(|e| e.into_inner());
        let last = owned().clone();
        match last {
            Some(Owned::Ours(at)) if at.elapsed() < recheck => return Ok(()),
            Some(Owned::Misrouted(misrouted, at))
                if !recheck.is_zero() && at.elapsed() < RECHECK_AFTER =>
            {
                return Err(eyre::Report::new(misrouted))
            }
            _ => {}
        }
        let owner = match workforce_vellev::index_claim(&self.tenants, OWNER, graph).await {
            Ok(owner) => owner,
            Err(e) => {
                if let Some(misrouted @ Unusable::Misrouted { .. }) = e.downcast_ref() {
                    *owned() = Some(Owned::Misrouted(misrouted.clone(), Instant::now()));
                }
                return Err(e);
            }
        };
        let ours = owner == *graph;
        *owned() = ours.then(|| Owned::Ours(Instant::now()));
        if !ours {
            return Err(eyre::eyre!(
                "the code vectors of vellev tenant '{}' are kept for another code graph ({owner}), not this one ({graph}): a tenant holds one code graph's vectors. Give each code graph a tenant of its own, or have every hub use one code graph ([codegraph] backend = \"postgres\"); or, if this graph has replaced that one (the code graph database was made again), run `workforce code reindex-vectors --take-over`",
                self.tenants.reads.id()
            ));
        }
        Ok(())
    }

    /// Have the vectors of what is written made by `seeding`'s embedder
    /// and sent with it, rather than made by the node: for filling a large
    /// collection faster than the node's own model can. Only for an
    /// embedder checked to embed as the node does (see
    /// [`workforce_vellev::seeding::equivalence`]).
    pub fn seeded_by(mut self, seeding: Seeding) -> Self {
        self.seeding = Some(seeding);
        self
    }

    /// The tenant, as reads and as writes reach it, and how the code
    /// collections embed: what an equivalence check needs.
    pub fn embedded_as(&self) -> (&workforce_vellev::Tenants, Embedding) {
        (&self.tenants, self.entities.wanted().embedding.clone())
    }

    /// `e`, an indexing of `what` that failed, as the sink hears of it,
    /// keeping what it is.
    fn told(&self, what: &str, e: eyre::Report) -> eyre::Report {
        let failed = format!("vellev batch index of {what} failed");
        warn!(error = %e, what, "vellev batch insert failed");
        self.insert_errors.notify(&format!("{failed}: {e}"));
        e.wrap_err(failed)
    }

    /// Store `changes` into `collection`, a batch at a time; the sink hears
    /// of a batch that failed, or of a collection not to be written to, and
    /// the failure ends the indexing.
    async fn store(&self, what: &str, collection: &Ensured, changes: Vec<Change>) -> Result<usize> {
        self.owned().await?;
        let told = |e| self.told(what, e);
        if let Some(seeding) = &self.seeding {
            collection.records().await.map_err(told)?;
            let stored = apply_embedded(&self.tenants, collection.wanted(), seeding, changes)
                .await
                .map_err(told)?;
            count_embedded(VectorKeys::VELLEV, stored);
            debug!(
                what,
                count = stored,
                "Batch indexed in vellev, with its vectors"
            );
            return Ok(stored);
        }
        let mut stored = 0;
        for batch in changes.chunks(NODE_EMBEDS_AT_ONCE) {
            let size = batch.len();
            let applied = collection
                .run(|| {
                    let batch = batch.to_vec();
                    async move { self.tenants.writes.apply(batch).await }
                })
                .await;
            applied.map_err(told)?;
            count_embedded(VectorKeys::VELLEV, size);
            stored += size;
            debug!(
                what,
                chunk_size = size,
                total = stored,
                "Batch indexed chunk in vellev"
            );
        }
        info!(what, count = stored, "Batch indexed in vellev");
        Ok(stored)
    }

    /// Which of `entities` the collection holds as they are, but for their
    /// fill: compared a repo at a time, [`VERIFY_PAGE`] at once, as a
    /// verify compares them.
    async fn compare(&self, entities: &[&VectorCodeEntity]) -> Result<Matched> {
        let by_repo = entities.iter().fold(
            HashMap::<&str, Vec<&VectorCodeEntity>>::new(),
            |mut by_repo, entity| {
                by_repo.entry(&entity.repo_id).or_default().push(entity);
                by_repo
            },
        );
        let mut matched = Matched::new();
        for (repo_id, held) in by_repo {
            for page in held.chunks(VERIFY_PAGE) {
                let wanted = entity_keys(page.iter().copied());
                let own: Vec<ObjectId> = page.iter().map(|entity| entity_id(entity)).collect();
                let differing = self
                    .differing(&self.entities, repo_id, ENTITY_KEY, &wanted, &own)
                    .await?;
                let differing: HashSet<usize> = differing.places.into_iter().collect();
                matched.extend(
                    page.iter()
                        .enumerate()
                        .filter(|(at, _)| !differing.contains(at))
                        .map(|(_, entity)| entity_id(entity)),
                );
            }
        }
        Ok(matched)
    }

    /// Write `entities`: those `matched` by a patch of their fill alone, the
    /// rest whole, as are those of a patch the node refused. Returns how
    /// many were written, each once.
    pub(crate) async fn write(
        &self,
        entities: &[&VectorCodeEntity],
        matched: &Matched,
    ) -> Result<usize> {
        let (whole, patched) = split(entities, matched);
        let (refilled, refused) = self.refill(patched).await?;
        let changes = whole
            .into_iter()
            .chain(refused)
            .map(|entity| {
                upsert(
                    CODE_ENTITY_COLLECTION,
                    entity_id(entity),
                    Value::Object(entity_properties(entity)),
                )
            })
            .collect::<Result<Vec<_>>>()?;
        if changes.is_empty() {
            return Ok(refilled);
        }
        Ok(refilled + self.store("entities", &self.entities, changes).await?)
    }

    /// Give `entities`, held as they are, their fill by patches that embed
    /// nothing, each only if its object still holds the entity's hash.
    /// Returns how many the node took, and those of each batch it refused,
    /// an object of it having changed or gone meanwhile: nothing of such a
    /// batch was written, and writing it whole is a write as any other.
    async fn refill<'a>(
        &self,
        entities: Vec<&'a VectorCodeEntity>,
    ) -> Result<(usize, Vec<&'a VectorCodeEntity>)> {
        if entities.is_empty() {
            return Ok((0, entities));
        }
        self.owned().await?;
        let name = self.entities.name();
        self.entities
            .records()
            .await
            .map_err(|e| self.told("entities", e))?;
        let mut refilled = 0;
        let mut refused = Vec::new();
        for batch in entities.chunks(INDEX_BATCH) {
            let patches = batch
                .iter()
                .map(|entity| fill_patch(name, entity))
                .collect();
            match self.tenants.writes.apply(patches).await {
                Ok(_) => refilled += batch.len(),
                Err(e) if is_conflict(&e) || e.is_not_found() => refused.extend_from_slice(batch),
                Err(e) => {
                    let asked = Asked::Request {
                        name,
                        model: &self.local.model,
                    };
                    let failed = failure(e, &self.tenants, asked);
                    return Err(self.told("entities", failed));
                }
            }
        }
        debug!(
            refilled,
            refused = refused.len(),
            "Fills of entities held as they are patched in vellev"
        );
        Ok((refilled, refused))
    }

    /// How `wanted`, `(key, hash)` pairs of one repo whose records are
    /// kept at the ids `own`, differs from what `collection` holds: told by
    /// counting where all are held (see [`workforce_vellev::unmatched`]),
    /// [`VERIFY_PAGE`] pairs at a time. Every object held under a wanted
    /// key at another id than the record's own is [`Differing::stale`]:
    /// an earlier version, or a second copy of the same text.
    async fn differing(
        &self,
        collection: &Ensured,
        repo_id: &str,
        key: &str,
        wanted: &[(String, String)],
        own: &[ObjectId],
    ) -> Result<Differing> {
        let scope = of_repo(repo_id);
        let mut differing = Differing::default();
        let pages = wanted.chunks(VERIFY_PAGE).zip(own.chunks(VERIFY_PAGE));
        for (page, (chunk, own)) in pages.enumerate() {
            let found = collection
                .run(|| {
                    workforce_vellev::unmatched(
                        collection.collection(),
                        Some(&scope),
                        (key, HASH),
                        chunk,
                        &self.cap,
                    )
                })
                .await?;
            let missing: HashSet<&String> = found.differing.iter().collect();
            // An earlier version held beside the wanted one differs too.
            let besides: HashSet<&String> = found.stale.iter().map(|(_, key)| key).collect();
            let offset = page * VERIFY_PAGE;
            for (at, (key, _)) in chunk.iter().enumerate() {
                let lacked = missing.contains(key);
                if lacked || besides.contains(key) {
                    differing.places.push(offset + at);
                }
                if lacked {
                    differing.missing.push(offset + at);
                }
            }
            let own: HashMap<&str, ObjectId> = chunk
                .iter()
                .map(|(key, _)| key.as_str())
                .zip(own.iter().copied())
                .collect();
            for (id, held_key) in &found.stale {
                if own.get(held_key.as_str()) != Some(id) {
                    differing.stale.push(id.0.to_string());
                } else if !missing.contains(held_key) {
                    // Another object of the wanted text was taken for the
                    // record: it is the one held besides.
                    let besides = self.held_under(collection, &scope, key, held_key).await?;
                    let besides = besides.into_iter().filter(|other| other != id);
                    differing
                        .stale
                        .extend(besides.map(|other| other.0.to_string()));
                }
            }
            differing.held += usize::try_from(found.held)?;
        }
        differing.stale.sort();
        differing.stale.dedup();
        Ok(differing)
    }

    /// The ids of the objects `collection` holds within `scope` under the
    /// key `held_key`.
    async fn held_under(
        &self,
        collection: &Ensured,
        scope: &Filter,
        key: &str,
        held_key: &str,
    ) -> Result<Vec<ObjectId>> {
        let filter = Filter::And(vec![scope.clone(), eq(key, held_key)]);
        let listed = collection
            .run(|| {
                self.cap.ask(VERIFY_PAGE, |limit| {
                    let page = Page::new(limit).filter(filter.clone());
                    async move { collection.collection().list(&page).await }
                })
            })
            .await?;
        Ok(listed.objects.iter().map(|object| object.id).collect())
    }

    /// The collection of entities, or `summaries`, and the property each
    /// of its records is told apart by.
    fn keyed(&self, summaries: bool) -> (&Ensured, &'static str) {
        match summaries {
            true => (&self.summaries, SUMMARY_KEY),
            false => (&self.entities, ENTITY_KEY),
        }
    }

    /// Delete the objects with `ids` from `collection`, as one step each
    /// [`INDEX_BATCH`].
    async fn delete_ids(&self, collection: &Ensured, ids: Vec<ObjectId>) -> Result<()> {
        self.owned_now().await?;
        for batch in ids.chunks(INDEX_BATCH) {
            collection
                .run(|| {
                    let deletes = batch
                        .iter()
                        .map(|id| Change::delete(collection.name().clone(), *id))
                        .collect();
                    async move { self.tenants.writes.apply(deletes).await }
                })
                .await?;
        }
        Ok(())
    }

    /// Delete everything in `collection` passing `filter`, however many
    /// match (see [`workforce_vellev::delete_where`]): no one commit
    /// deletes more than vellev lets a delete by filter match.
    async fn delete_where(&self, collection: &Ensured, filter: Filter) -> Result<DeleteOutcome> {
        self.owned_now().await?;
        let deleted = collection
            .run(|| {
                workforce_vellev::delete_where(
                    &self.tenants.reads,
                    &self.tenants.writes,
                    collection.name(),
                    &filter,
                )
            })
            .await?;
        Ok(DeleteOutcome {
            successful: usize::try_from(deleted)?,
            failed: 0,
        })
    }
}

/// `ids` as object ids.
fn object_ids(ids: &[String]) -> Result<Vec<ObjectId>> {
    ids.iter().map(|id| Ok(ObjectId(id.parse()?))).collect()
}

/// The two code collections in the tenant, embedding as `local` does.
fn collections(
    tenants: &workforce_vellev::Tenants,
    local: &Embedding,
) -> Result<(Ensured, Ensured)> {
    let ensured = |wanted| Ensured::new(tenants.clone(), wanted, REINDEX).identified();
    Ok((
        ensured(entity_collection(local)?),
        ensured(summary_collection(local)?),
    ))
}

/// How a collection embedding as `local` does embeds `property`.
fn embedding_on(local: &Embedding, property: &str) -> Embedding {
    Embedding {
        properties: vec![property.to_string()],
        ..local.clone()
    }
}

/// `CodeEntity` as search, deletes and a fill's compare read it, embedded
/// by the node on its searchable content, which it does not keep.
fn entity_collection(local: &Embedding) -> Result<Collection> {
    Ok(collection(
        CollectionName::new(CODE_ENTITY_COLLECTION)?,
        [
            embedded("searchableContent"),
            filtered(ENTITY_KEY),
            filtered("repoId"),
            kept("qualifiedName", DataType::Text),
            filtered("kind"),
            filtered("filePath"),
            kept("startLine", DataType::Int),
            kept("endLine", DataType::Int),
            filtered(FILL),
            filtered(HASH),
        ]
        .into(),
        embedding_on(local, "searchableContent"),
    ))
}

/// The properties an entity is written with: [`entity_collection`]'s.
fn entity_properties(entity: &VectorCodeEntity) -> Map<String, Value> {
    [
        ("searchableContent", json!(entity.searchable_content)),
        (ENTITY_KEY, json!(entity.entity_id)),
        ("repoId", json!(entity.repo_id)),
        ("qualifiedName", json!(entity.qualified_name)),
        ("kind", json!(entity.kind)),
        ("filePath", json!(entity.file_path)),
        ("startLine", json!(entity.start_line)),
        ("endLine", json!(entity.end_line)),
        (FILL, json!(entity.generation)),
        (HASH, json!(entity.record_hash())),
    ]
    .into_iter()
    .map(|(name, value)| (name.to_string(), value))
    .collect()
}

/// What an entity is compared with the store by: its key and its hash.
fn entity_keys<'a>(
    entities: impl IntoIterator<Item = &'a VectorCodeEntity>,
) -> Vec<(String, String)> {
    entities
        .into_iter()
        .map(|entity| (entity.entity_id.clone(), entity.record_hash()))
        .collect()
}

/// `entities` as those to write whole and those `matched`, to patch, each
/// part in their order.
fn split<'a>(
    entities: &[&'a VectorCodeEntity],
    matched: &Matched,
) -> (Vec<&'a VectorCodeEntity>, Vec<&'a VectorCodeEntity>) {
    entities
        .iter()
        .copied()
        .partition(|entity| !matched.contains(&entity_id(entity)))
}

/// A patch of `entity`'s fill alone, only if its object holds its hash.
fn fill_patch(collection: &CollectionName, entity: &VectorCodeEntity) -> Change {
    Change::patch(
        collection.clone(),
        entity_id(entity),
        Changes::new(Map::from_iter([(
            FILL.to_string(),
            json!(entity.generation),
        )]))
        .only_if(OnlyIf::new(HASH, json!(entity.record_hash()))),
    )
}

/// `CodeSummary` as Weaviate has it, embedded by the node on its text.
fn summary_collection(local: &Embedding) -> Result<Collection> {
    Ok(collection(
        CollectionName::new(CODE_SUMMARY_COLLECTION)?,
        [
            searched("summaryText"),
            filtered("entityId"),
            filtered("repoId"),
            filtered("granularity"),
            filtered("filePath"),
            Property::new("isStale", DataType::Bool),
            filtered(FILL),
            filtered(HASH),
            filtered(SUMMARY_KEY),
        ]
        .into(),
        embedding_on(local, "summaryText"),
    ))
}

/// Text found by its words.
fn searched(name: &str) -> Property {
    Property {
        filterable: false,
        ..Property::new(name, DataType::Text).searchable()
    }
}

/// Text matched whole.
fn filtered(name: &str) -> Property {
    Property::new(name, DataType::Text)
}

fn embedded(name: &str) -> Property {
    Property::new(name, DataType::Text).unstored()
}

fn kept(name: &str, data_type: DataType) -> Property {
    Property {
        filterable: false,
        ..Property::new(name, data_type)
    }
}

/// The object id of a record, the same however often it is indexed, so
/// that indexing again replaces it. Named by the JSON of its kind and
/// parts, which no two different lists of parts share.
fn record_id(kind: &str, parts: &[&str]) -> ObjectId {
    let name = json!([kind, parts]).to_string();
    ObjectId(Uuid::new_v5(&Uuid::NAMESPACE_URL, name.as_bytes()))
}

/// An entity's id. Its graph id hashes only its file, kind and short name,
/// which `impl A { fn new }` and `impl B { fn new }` share, so the qualified
/// name, signature and first line keep apart what the graph does not, as
/// Weaviate's random ids did.
fn entity_id(entity: &VectorCodeEntity) -> ObjectId {
    let line = entity.start_line.to_string();
    record_id(
        CODE_ENTITY_COLLECTION,
        &[
            &entity.repo_id,
            &entity.entity_id,
            &entity.qualified_name,
            &entity.signature,
            &line,
        ],
    )
}

fn summary_id(summary: &VectorCodeSummary) -> ObjectId {
    record_id(
        CODE_SUMMARY_COLLECTION,
        &[&summary.repo_id, &summary.granularity, &summary.entity_id],
    )
}

fn eq(property: &str, value: &str) -> Filter {
    Filter::Eq {
        property: property.to_string(),
        value: value.into(),
    }
}

fn of_repo(repo_id: &str) -> Filter {
    eq("repoId", repo_id)
}

fn of_file(repo_id: &str, file_path: &str) -> Filter {
    Filter::And(vec![of_repo(repo_id), eq("filePath", file_path)])
}

/// The filter of an entity search, if it has one.
fn search_filter(repo_id: Option<&str>, kind: Option<EntityKind>) -> Option<Filter> {
    let kind = kind.map(|kind| format!("{kind:?}"));
    let conditions: Vec<Filter> = [
        repo_id.map(of_repo),
        kind.as_deref().map(|kind| eq("kind", kind)),
    ]
    .into_iter()
    .flatten()
    .collect();
    (!conditions.is_empty()).then_some(Filter::And(conditions))
}

fn upsert(records: &str, id: ObjectId, properties: Value) -> Result<Change> {
    Ok(Change::upsert(
        CollectionName::new(records)?,
        id,
        Content::new(match properties {
            Value::Object(properties) => properties,
            _ => Map::new(),
        }),
    ))
}

#[async_trait]
impl CodeVectorStore for VellevCodeVectorStore {
    fn backend(&self) -> &'static str {
        crate::graph::VectorKeys::VELLEV
    }

    async fn ensure_collections(&self) -> Result<()> {
        self.entities.records().await?;
        self.summaries.records().await?;
        Ok(())
    }

    /// Ready when the node is and both collections are, looked at afresh. A
    /// collection an operator has to put right ([`Unusable`]: refused shape,
    /// missing tenant, refused key) is an error naming why.
    async fn health_check(&self) -> Result<bool> {
        if self.health.ready().await.is_err() {
            return Ok(false);
        }
        for collection in [&self.entities, &self.summaries] {
            match collection.recheck().await {
                Ok(_) => {}
                Err(e) if e.downcast_ref::<Unusable>().is_some() => return Err(e),
                Err(_) => return Ok(false),
            }
        }
        Ok(true)
    }

    fn base_url(&self) -> &str {
        &self.base_url
    }

    fn tenant(&self) -> Option<String> {
        Some(self.tenants.reads.id().to_string())
    }

    fn set_insert_error_sink(&self, sink: Arc<dyn InsertErrorSink>) {
        self.insert_errors.set(sink);
    }

    async fn index_entities(&self, entities: &[VectorCodeEntity]) -> Result<usize> {
        let entities: Vec<&VectorCodeEntity> =
            vectorizable(entities, entity_text, "code_index_entities", "entities")
                .into_iter()
                .map(|(entity, _)| entity)
                .collect();
        let matched = self
            .compare(&entities)
            .await
            .map_err(|e| self.told("entities", e))?;
        self.write(&entities, &matched).await
    }

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
        let filter = search_filter(repo_id, kind);
        let found = self
            .entities
            .run(|| {
                self.cap.ask(limit, |limit| {
                    let nearest = Nearest::to_text(query.as_str(), limit);
                    let nearest = filter.clone().into_iter().fold(nearest, Nearest::filter);
                    async move { self.entities.collection().nearest(&nearest).await }
                })
            })
            .await?;
        Ok(found
            .results
            .into_iter()
            .filter_map(|found| {
                search_result(
                    &Value::Object(found.object.properties),
                    found.certainty.unwrap_or(0.0),
                )
            })
            .collect())
    }

    async fn delete_repo_entities(&self, repo_id: &str) -> Result<DeleteOutcome> {
        let outcome = self.delete_where(&self.entities, of_repo(repo_id)).await?;
        info!(
            repo_id,
            deleted = outcome.successful,
            "Deleted repo entities from vellev"
        );
        Ok(outcome)
    }

    async fn delete_entities_by_file(
        &self,
        repo_id: &str,
        file_path: &str,
    ) -> Result<DeleteOutcome> {
        self.delete_where(&self.entities, of_file(repo_id, file_path))
            .await
    }

    /// The sink hears of a failed batch of summaries as of entities: both
    /// say the vector backend is failing writes.
    async fn index_summaries(&self, summaries: &[VectorCodeSummary]) -> Result<usize> {
        let changes = vectorizable(summaries, summary_text, "code_index_summaries", "summaries")
            .into_iter()
            .map(|(summary, text)| {
                upsert(
                    CODE_SUMMARY_COLLECTION,
                    summary_id(summary),
                    json!({
                        "summaryText": text,
                        "entityId": summary.entity_id,
                        "repoId": summary.repo_id,
                        "granularity": summary.granularity,
                        "filePath": summary.file_path,
                        "isStale": summary.is_stale,
                        FILL: summary.generation,
                        HASH: summary.record_hash(),
                        SUMMARY_KEY: summary.key(),
                    }),
                )
            })
            .collect::<Result<Vec<_>>>()?;
        self.store("summaries", &self.summaries, changes).await
    }

    async fn delete_repo_summaries(&self, repo_id: &str) -> Result<DeleteOutcome> {
        let outcome = self.delete_where(&self.summaries, of_repo(repo_id)).await?;
        info!(
            repo_id,
            deleted = outcome.successful,
            "Deleted repo summaries from vellev"
        );
        Ok(outcome)
    }

    async fn delete_summaries_by_file(
        &self,
        repo_id: &str,
        file_path: &str,
    ) -> Result<DeleteOutcome> {
        self.delete_where(&self.summaries, of_file(repo_id, file_path))
            .await
    }

    /// A vellev collection starts empty, or is dropped and made again, while
    /// the graph already holds the repo: its vectors are complete only once
    /// a full index's vector pass, or a backfill from the graph, says so.
    async fn take_over(&self) -> Result<()> {
        if self.graph.get().is_none() {
            return Ok(());
        }
        workforce_vellev::index_unmark(&self.tenants, OWNER).await?;
        self.owned_now().await
    }

    fn keep_for_graph(&self, graph: &str) {
        let _ = self.graph.set(graph.to_string());
    }

    fn tracks_completeness(&self) -> bool {
        true
    }

    /// Both collections' makings: either lost, the store is another.
    async fn identity(&self, afresh: bool) -> Result<String> {
        self.owned().await?;
        if afresh {
            self.entities.recheck().await?;
            self.summaries.recheck().await?;
        }
        Ok(format!(
            "{}/{}",
            self.entities.identity().await?,
            self.summaries.identity().await?
        ))
    }

    async fn differing_entities(
        &self,
        repo_id: &str,
        entities: &[VectorCodeEntity],
    ) -> Result<Differing> {
        let own: Vec<ObjectId> = entities.iter().map(entity_id).collect();
        self.differing(
            &self.entities,
            repo_id,
            ENTITY_KEY,
            &entity_keys(entities),
            &own,
        )
        .await
    }

    async fn differing_summaries(
        &self,
        repo_id: &str,
        summaries: &[VectorCodeSummary],
    ) -> Result<Differing> {
        let wanted: Vec<(String, String)> = summaries
            .iter()
            .map(|summary| (summary.key(), summary.record_hash()))
            .collect();
        let own: Vec<ObjectId> = summaries.iter().map(summary_id).collect();
        self.differing(&self.summaries, repo_id, SUMMARY_KEY, &wanted, &own)
            .await
    }

    async fn held_after(
        &self,
        repo_id: &str,
        summaries: bool,
        after: Option<String>,
        limit: usize,
    ) -> Result<(Vec<HeldRecord>, Option<String>)> {
        let (collection, key) = self.keyed(summaries);
        let after = match after {
            Some(after) => Some(ObjectId(after.parse()?)),
            None => None,
        };
        let filter = of_repo(repo_id);
        let listed = collection
            .run(|| {
                self.cap.ask(limit, |limit| {
                    let page = Page::new(limit).filter(filter.clone());
                    let page = after.into_iter().fold(page, Page::after);
                    async move { collection.collection().list(&page).await }
                })
            })
            .await?;
        let held = listed
            .objects
            .iter()
            .map(|object| {
                let property = |name: &str| {
                    let value = object.properties.get(name).and_then(|value| value.as_str());
                    value.unwrap_or_default().to_string()
                };
                HeldRecord {
                    id: object.id.0.to_string(),
                    key: property(key),
                    file_path: property("filePath"),
                }
            })
            .collect();
        Ok((held, listed.next.map(|next| next.0.to_string())))
    }

    async fn remove_held(&self, summaries: bool, ids: &[String]) -> Result<()> {
        let (collection, _) = self.keyed(summaries);
        self.delete_ids(collection, object_ids(ids)?).await
    }

    /// One object of each collection is listed, of any repo but these:
    /// what holds no repo at all is never one.
    async fn held_outside(&self, repos: &[String]) -> Result<Option<String>> {
        let outside = Filter::And(
            std::iter::once(Filter::Not(Box::new(Filter::IsNull {
                property: "repoId".to_string(),
            })))
            .chain(repos.iter().map(|repo| Filter::Ne {
                property: "repoId".to_string(),
                value: repo.as_str().into(),
            }))
            .collect(),
        );
        for collection in [&self.entities, &self.summaries] {
            let listed = collection
                .run(|| {
                    let page = Page::new(1).filter(outside.clone());
                    async move { collection.collection().list(&page).await }
                })
                .await?;
            let repo = listed
                .objects
                .first()
                .and_then(|object| object.properties.get("repoId"))
                .and_then(Value::as_str);
            if let Some(repo) = repo {
                return Ok(Some(repo.to_string()));
            }
        }
        Ok(None)
    }

    async fn file_held(&self, repo_id: &str, file_path: &str) -> Result<usize> {
        let filter = of_file(repo_id, file_path);
        let mut held = 0;
        for collection in [&self.entities, &self.summaries] {
            let counted = collection
                .run(|| collection.collection().count(Some(&filter)))
                .await?;
            held += usize::try_from(counted)?;
        }
        Ok(held)
    }

    async fn remove_entities(&self, entities: &[VectorCodeEntity]) -> Result<()> {
        let ids = entities.iter().map(entity_id).collect();
        self.delete_ids(&self.entities, ids).await
    }

    async fn remove_summaries(&self, summaries: &[VectorCodeSummary]) -> Result<()> {
        let ids = summaries.iter().map(summary_id).collect();
        self.delete_ids(&self.summaries, ids).await
    }

    async fn sweep(&self, repo_id: &str, generation: &str) -> Result<usize> {
        let stale = || {
            Filter::And(vec![
                of_repo(repo_id),
                Filter::Or(vec![
                    Filter::IsNull {
                        property: FILL.to_string(),
                    },
                    Filter::Ne {
                        property: FILL.to_string(),
                        value: generation.into(),
                    },
                ]),
            ])
        };
        let entities = self.delete_where(&self.entities, stale()).await?;
        let summaries = self.delete_where(&self.summaries, stale()).await?;
        Ok(entities.successful + summaries.successful)
    }

    async fn summary_count(&self, repo_id: &str) -> Result<usize> {
        let filter = of_repo(repo_id);
        let counted = self
            .summaries
            .run(|| self.summaries.collection().count(Some(&filter)))
            .await?;
        Ok(usize::try_from(counted)?)
    }

    async fn entity_count(&self, repo_id: &str) -> Result<usize> {
        let filter = of_repo(repo_id);
        let counted = self
            .entities
            .run(|| self.entities.collection().count(Some(&filter)))
            .await?;
        Ok(usize::try_from(counted)?)
    }
}

/// `CodeEntity` in the shape that stores every property, those the slim
/// shape leaves out among them, and indexes its text for keywords.
#[cfg(any(test, feature = "testing"))]
pub fn collection_keeping_text() -> Collection {
    collection(
        CollectionName::new(CODE_ENTITY_COLLECTION).unwrap(),
        vec![
            searched("searchableContent"),
            filtered("entityId"),
            filtered("repoId"),
            searched("qualifiedName"),
            searched("name"),
            filtered("kind"),
            filtered("filePath"),
            filtered("language"),
            searched("signature"),
            searched("docComment"),
            searched("summary"),
            Property::new("startLine", DataType::Int),
            Property::new("endLine", DataType::Int),
            filtered(FILL),
            filtered(HASH),
        ],
        local_embedding(&["searchableContent"]),
    )
}

#[cfg(test)]
mod tests {
    use workforce_vellev::testing::{ByNode, Node};

    use super::*;

    /// Vectors copied from Weaviate, which the node cannot embed queries
    /// to match.
    fn copied_from_weaviate() -> Collection {
        Collection {
            embedding: vellev_core::Embedding {
                provider: vellev_core::EmbeddingProvider::None,
                model: "weaviate:text2vec-transformers".into(),
                dimensions: 384,
                query_prefix: None,
                document_prefix: None,
                properties: Vec::new(),
            },
            ..collection_keeping_text()
        }
    }

    /// An insert-error sink that keeps what it hears.
    #[derive(Default)]
    struct Heard(std::sync::Mutex<Vec<String>>);

    impl InsertErrorSink for Heard {
        fn record(&self, message: String) {
            self.0.lock().unwrap().push(message);
        }
    }

    /// The properties of every entity of `repo_id` the store holds.
    async fn held(store: &VellevCodeVectorStore, repo_id: &str) -> Vec<Map<String, Value>> {
        let page = Page::new(100).filter(of_repo(repo_id));
        let listed = store.entities.collection().list(&page).await.unwrap();
        listed.objects.into_iter().map(|o| o.properties).collect()
    }

    /// An entity with a signature, a doc comment and a summary, all in
    /// what is embedded.
    fn documented(repo: &str, id: &str, said: &str) -> VectorCodeEntity {
        let (signature, doc) = (format!("pub fn {id}(path: &str)"), format!("{said}."));
        VectorCodeEntity {
            signature: signature.clone(),
            doc_comment: doc.clone(),
            summary: said.to_string(),
            searchable_content: format!("crate::{id} {signature} {doc} {said}"),
            start_line: 3,
            end_line: 12,
            ..entity(repo, id, &format!("src/{id}.rs"), "Function", "")
        }
    }

    use workforce_vellev::testing::SMALL;

    /// bge-small, as `[vellev.code]` would name it with bge's prefix.
    fn small() -> Embedding {
        embedding_of(SMALL, 384, Some(LOCAL_QUERY_PREFIX), &[])
    }

    /// A store of `node`'s tenant embedding as `local` does.
    fn embedding_as(node: &Node, local: Embedding) -> VellevCodeVectorStore {
        VellevCodeVectorStore::new(&node.url, &node.key, &node.tenant)
            .unwrap()
            .embedding(local)
            .unwrap()
    }

    #[test]
    fn a_configured_code_model_shapes_both_collections() {
        let store = VellevCodeVectorStore::new("http://127.0.0.1:1", "key", "code").unwrap();
        for (collection, property) in [
            (&store.entities, "searchableContent"),
            (&store.summaries, "summaryText"),
        ] {
            assert_eq!(
                collection.wanted().embedding,
                local_embedding(&[property]),
                "{property}"
            );
        }
        let configured = store.embedding(small()).unwrap();
        for (collection, property) in [
            (&configured.entities, "searchableContent"),
            (&configured.summaries, "summaryText"),
        ] {
            let embedding = &collection.wanted().embedding;
            assert_eq!(
                (embedding.model.as_str(), embedding.dimensions),
                (SMALL, 384)
            );
            assert_eq!(embedding.query_prefix.as_deref(), Some(LOCAL_QUERY_PREFIX));
            assert_eq!(embedding.document_prefix, None);
            assert_eq!(embedding.properties, [property]);
        }
        assert_eq!(
            configured.embedded_as().1,
            configured.entities.wanted().embedding
        );
    }

    #[test]
    fn the_code_model_resolves_to_todays_embedding_unless_configured() {
        let model = |model, dimensions, query_prefix| {
            code_embedding(Some(CodeModel {
                model,
                dimensions,
                query_prefix,
            }))
        };
        assert_eq!(code_embedding(None), local_embedding(&[]));
        assert_eq!(
            model(LOCAL_MODEL, 768, QueryPrefix::Unset),
            local_embedding(&[])
        );
        let mini = "Qdrant/all-MiniLM-L6-v2-onnx";
        assert_eq!(
            model(mini, 384, QueryPrefix::Unset),
            embedding_of(mini, 384, None, &[])
        );
        assert_eq!(
            model(LOCAL_MODEL, 768, QueryPrefix::None).query_prefix,
            None
        );
        assert_eq!(
            model(mini, 384, QueryPrefix::Given("query: ")).query_prefix,
            Some("query: ".to_string())
        );
    }

    #[test]
    fn the_entity_collection_keeps_only_what_is_read_and_embeds_what_it_does_not_keep() {
        let made = entity_collection(&local_embedding(&[])).unwrap();
        made.validate().unwrap();
        let unstored: Vec<&str> = made
            .properties
            .iter()
            .filter(|p| !p.stored)
            .map(|p| p.name.as_str())
            .collect();
        assert_eq!(unstored, ["searchableContent"]);
        assert_eq!(made.embedding.properties, ["searchableContent"]);
        assert!(made.properties.iter().all(|p| !p.searchable));
        let mut filterable: Vec<&str> = made
            .properties
            .iter()
            .filter(|p| p.filterable)
            .map(|p| p.name.as_str())
            .collect();
        filterable.sort_unstable();
        assert_eq!(
            filterable,
            ["entityId", "filePath", "fill", "kind", "repoId", "textHash"]
        );
    }

    #[test]
    fn an_entity_is_written_with_exactly_the_collections_properties() {
        let written: HashSet<String> = entity_properties(&documented("r", "e", "Does it"))
            .keys()
            .cloned()
            .collect();
        let shaped: HashSet<String> = entity_collection(&local_embedding(&[]))
            .unwrap()
            .properties
            .into_iter()
            .map(|p| p.name)
            .collect();
        assert_eq!(written, shaped);
    }

    #[test]
    fn a_write_is_split_into_whole_and_patched_each_in_order() {
        let entities: Vec<VectorCodeEntity> = (0..4)
            .map(|n| entity("r", &format!("e{n}"), "f.rs", "Function", "does a thing"))
            .collect();
        let all: Vec<&VectorCodeEntity> = entities.iter().collect();
        let split_by = |matched: &[usize]| {
            let matched: Matched = matched.iter().map(|&at| entity_id(all[at])).collect();
            split(&all, &matched)
        };
        assert_eq!(split_by(&[]), (all.clone(), vec![]));
        assert_eq!(split_by(&[0, 1, 2, 3]), (vec![], all.clone()));
        assert_eq!(
            split_by(&[3, 1]),
            (vec![all[0], all[2]], vec![all[1], all[3]])
        );
    }

    #[test]
    fn record_ids_are_stable_and_distinct() {
        let entity = |repo: &str, id: &str| VectorCodeEntity {
            entity_id: id.into(),
            repo_id: repo.into(),
            qualified_name: String::new(),
            name: String::new(),
            kind: "Function".into(),
            file_path: String::new(),
            language: String::new(),
            signature: String::new(),
            doc_comment: String::new(),
            summary: String::new(),
            searchable_content: String::new(),
            start_line: 0,
            end_line: 0,
            generation: String::new(),
        };
        assert_eq!(entity_id(&entity("r", "e")), entity_id(&entity("r", "e")));
        assert_ne!(entity_id(&entity("r", "e")), entity_id(&entity("s", "e")));
        assert_eq!(
            entity_id(&entity("r", "e")).0,
            Uuid::new_v5(
                &Uuid::NAMESPACE_URL,
                br#"["CodeEntity",["r","e","","","0"]]"#
            )
        );
        assert_ne!(
            entity_id(&entity("a|b", "c")),
            entity_id(&entity("a", "b|c")),
            "parts that join alike are told apart"
        );
        let summary = |granularity: &str| VectorCodeSummary {
            entity_id: "e".into(),
            repo_id: "r".into(),
            granularity: granularity.into(),
            file_path: String::new(),
            summary_text: String::new(),
            is_stale: false,
            generation: String::new(),
        };
        assert_ne!(summary_id(&summary("file")), summary_id(&summary("module")));
        assert_ne!(
            summary_id(&summary("file")).0,
            entity_id(&entity("r", "e")).0
        );
    }

    /// The graph gives `impl A { fn new }` and `impl B { fn new }` in one
    /// file the same entity id; their vectors stay apart.
    #[test]
    fn methods_of_one_name_in_one_file_keep_their_own_vectors() {
        let method = |qualified: &str, line: u32| VectorCodeEntity {
            entity_id: "same-graph-id".into(),
            qualified_name: qualified.into(),
            signature: "pub fn new() -> Self".into(),
            start_line: line,
            ..entity("repo", "new", "src/lib.rs", "Method", "new")
        };
        assert_ne!(
            entity_id(&method("A::new", 3)),
            entity_id(&method("B::new", 9))
        );
        assert_eq!(
            entity_id(&method("A::new", 3)),
            entity_id(&method("A::new", 3))
        );
    }

    #[test]
    fn a_search_filters_on_repo_and_kind_as_weaviate_does() {
        assert_eq!(search_filter(None, None), None);
        assert_eq!(
            search_filter(Some("repo"), Some(EntityKind::Function)),
            Some(Filter::And(vec![
                eq("repoId", "repo"),
                eq("kind", "Function")
            ]))
        );
        assert_eq!(
            search_filter(None, Some(EntityKind::Struct)),
            Some(Filter::And(vec![eq("kind", "Struct")]))
        );
        assert_eq!(
            of_file("repo", "src/a.rs"),
            Filter::And(vec![eq("repoId", "repo"), eq("filePath", "src/a.rs")])
        );
    }

    #[tokio::test]
    async fn a_node_that_is_not_there_is_not_healthy() {
        let store = VellevCodeVectorStore::new("http://127.0.0.1:1", "key", "workforce").unwrap();
        assert!(!store.health_check().await.unwrap());
    }

    fn entity(repo: &str, id: &str, file: &str, kind: &str, content: &str) -> VectorCodeEntity {
        VectorCodeEntity {
            entity_id: id.into(),
            repo_id: repo.into(),
            qualified_name: format!("crate::{id}"),
            name: id.into(),
            kind: kind.into(),
            file_path: file.into(),
            language: "Rust".into(),
            signature: String::new(),
            doc_comment: String::new(),
            summary: String::new(),
            searchable_content: content.into(),
            start_line: 1,
            end_line: 9,
            generation: String::new(),
        }
    }

    fn summary(repo: &str, id: &str, file: &str, text: &str) -> VectorCodeSummary {
        VectorCodeSummary {
            entity_id: id.into(),
            repo_id: repo.into(),
            granularity: "file".into(),
            file_path: file.into(),
            summary_text: text.into(),
            is_stale: false,
            generation: String::new(),
        }
    }

    /// When a tenant's code vectors are taken over for another graph, a
    /// process still running for the graph they were taken from finds out:
    /// before it removes or sweeps anything it looks again, whenever it
    /// last looked, and removes nothing; and it stops writing once it has
    /// not looked for as long as it may go without.
    ///
    /// Neither half rests on how fast the test runs: a writer that may go
    /// an hour without looking is shown to go on writing, and one that may
    /// go a moment is shown to stop once that moment has surely passed.
    #[tokio::test]
    async fn a_store_whose_vectors_were_taken_over_stops_removing_at_once_and_writing_soon() {
        let Some(node) = Node::embedding().await else {
            return;
        };
        let of = |graph: &str, recheck: Duration| {
            VellevCodeVectorStore::new(&node.url, &node.key, &node.tenant)
                .unwrap()
                .for_graph(graph)
                .rechecking(recheck)
        };
        let hour = Duration::from_secs(3600);
        let moment = Duration::from_millis(200);
        // Three processes of the old graph: one that removes, and two that
        // only write, one looking again hourly, one every moment.
        let (old, writer, quick) = (
            of("graph-old", hour),
            of("graph-old", hour),
            of("graph-old", moment),
        );
        let new = of("graph-new", hour);
        let unowned = VellevCodeVectorStore::new(&node.url, &node.key, &node.tenant).unwrap();
        let one = entity("r", "e", "f.rs", "Function", "does a thing");
        for store in [&old, &writer, &quick] {
            assert_eq!(
                store
                    .index_entities(std::slice::from_ref(&one))
                    .await
                    .unwrap(),
                1
            );
        }
        let refused = new.index_entities(std::slice::from_ref(&one)).await;
        assert!(format!("{:#}", refused.unwrap_err()).contains("another code graph"));

        new.take_over().await.unwrap();
        // The old one looked a moment ago. Nothing is removed all the same.
        for removed in [
            old.delete_repo_entities("r").await.map(|_| ()),
            old.delete_entities_by_file("r", "f.rs").await.map(|_| ()),
            old.sweep("r", "another-fill").await.map(|_| ()),
            old.remove_entities(std::slice::from_ref(&one)).await,
        ] {
            let refused = format!("{:#}", removed.unwrap_err());
            assert!(refused.contains("graph-new"), "{refused}");
        }
        assert_eq!(unowned.entity_count("r").await.unwrap(), 1, "none removed");

        // A writer that may go an hour without looking goes on writing
        // within it: it is not asked to look before every write.
        let written = writer.index_entities(std::slice::from_ref(&one)).await;
        assert_eq!(written.expect("stopped before it was due to look again"), 1);
        // One that may go a moment stops once the moment has passed.
        tokio::time::sleep(moment + Duration::from_millis(50)).await;
        let refused = quick.index_entities(std::slice::from_ref(&one)).await;
        let refused = format!("{:#}", refused.unwrap_err());
        assert!(refused.contains("graph-new"), "{refused}");
        assert_eq!(
            new.index_entities(std::slice::from_ref(&one))
                .await
                .unwrap(),
            1
        );
        new.delete_repo_entities("r").await.unwrap();
        assert_eq!(unowned.entity_count("r").await.unwrap(), 0);
    }

    /// Everything that removes, sweeps, counts, lists or compares does so
    /// within the repo it is given: another repo's entities and summaries,
    /// with the same files and of other fills, are as they were after each.
    #[tokio::test]
    async fn what_is_done_to_one_repos_vectors_leaves_anothers_alone() {
        let Some(node) = Node::embedding().await else {
            return;
        };
        let store = VellevCodeVectorStore::new(&node.url, &node.key, &node.tenant).unwrap();
        let of = |repo: &str, generation: &str| {
            let entities: Vec<VectorCodeEntity> = ["a", "b", "c"]
                .iter()
                .map(|id| {
                    let file = format!("src/{id}.rs");
                    entity(
                        repo,
                        &format!("{repo}-{id}"),
                        &file,
                        "Function",
                        "does a thing",
                    )
                    .of_generation(generation)
                })
                .collect();
            let summaries: Vec<VectorCodeSummary> = entities
                .iter()
                .map(|e| VectorCodeSummary {
                    generation: generation.to_string(),
                    ..summary(repo, &e.entity_id, &e.file_path, "Does a thing.")
                })
                .collect();
            (entities, summaries)
        };
        let (ours, our_summaries) = of("ours", "old");
        let (theirs, their_summaries) = of("theirs", "theirs-old");
        for (entities, summaries) in [(&ours, &our_summaries), (&theirs, &their_summaries)] {
            assert_eq!(store.index_entities(entities).await.unwrap(), 3);
            assert_eq!(store.index_summaries(summaries).await.unwrap(), 3);
        }
        let untouched = || async {
            assert_eq!(store.entity_count("theirs").await.unwrap(), 3);
            assert_eq!(store.summary_count("theirs").await.unwrap(), 3);
            let agreed = store.differing_entities("theirs", &theirs).await.unwrap();
            assert_eq!((agreed.places.len(), agreed.held), (0, 3));
            let agreed = store
                .differing_summaries("theirs", &their_summaries)
                .await
                .unwrap();
            assert_eq!((agreed.places.len(), agreed.held), (0, 3));
        };
        untouched().await;

        // Listed and compared within the repo.
        let (held, _) = store.held_after("ours", false, None, 100).await.unwrap();
        assert_eq!(held.len(), 3);
        assert!(held.iter().all(|held| held.key.starts_with("ours-")));
        let (held, _) = store.held_after("ours", true, None, 100).await.unwrap();
        assert_eq!(held.len(), 3);
        let elsewhere = store.differing_entities("ours", &theirs).await.unwrap();
        assert_eq!((elsewhere.places, elsewhere.held), (vec![0, 1, 2], 0));

        // A file's, where both repos have the file.
        store
            .delete_entities_by_file("ours", "src/a.rs")
            .await
            .unwrap();
        store
            .delete_summaries_by_file("ours", "src/a.rs")
            .await
            .unwrap();
        assert_eq!(store.entity_count("ours").await.unwrap(), 2);
        assert_eq!(store.summary_count("ours").await.unwrap(), 2);
        untouched().await;

        // A sweep of what a fill did not write: the other repo's are of
        // another fill, and none of this one's.
        let (refilled, refilled_summaries) = of("ours", "new");
        store.index_entities(&refilled[1..2]).await.unwrap();
        store
            .index_summaries(&refilled_summaries[1..2])
            .await
            .unwrap();
        assert_eq!(store.sweep("ours", "new").await.unwrap(), 2);
        assert_eq!(store.entity_count("ours").await.unwrap(), 1);
        assert_eq!(store.summary_count("ours").await.unwrap(), 1);
        untouched().await;

        // Removed one by one, and the whole repo.
        store.remove_entities(&refilled[1..2]).await.unwrap();
        store
            .remove_summaries(&refilled_summaries[1..2])
            .await
            .unwrap();
        untouched().await;
        store.index_entities(&refilled).await.unwrap();
        store.index_summaries(&refilled_summaries).await.unwrap();
        store.delete_repo_entities("ours").await.unwrap();
        store.delete_repo_summaries("ours").await.unwrap();
        assert_eq!(store.entity_count("ours").await.unwrap(), 0);
        assert_eq!(store.summary_count("ours").await.unwrap(), 0);
        untouched().await;
    }

    /// Summaries are compared with what the store holds by entity and
    /// granularity together, so that two of one entity are told apart; and
    /// what is held twice under one key, an earlier version beside the one
    /// wanted, is told as differing, entity or summary, though the one
    /// wanted is there.
    #[tokio::test]
    async fn summaries_and_earlier_versions_are_compared_as_they_are_kept() {
        let Some(node) = Node::embedding().await else {
            return;
        };
        let store = VellevCodeVectorStore::new(&node.url, &node.key, &node.tenant).unwrap();
        let of_file = summary("repo", "src/mail.rs", "src/mail.rs", "Sends mail.");
        let of_function = VectorCodeSummary {
            granularity: "function".into(),
            ..summary("repo", "src/mail.rs", "src/mail.rs", "Sends one message.")
        };
        let wanted = vec![of_file.clone(), of_function.clone()];
        assert_eq!(store.index_summaries(&wanted).await.unwrap(), 2);
        let agreed = store.differing_summaries("repo", &wanted).await.unwrap();
        assert_eq!((agreed.places.len(), agreed.held), (0, 2));
        // One of the two changed in the graph: that one, and not the other.
        let changed = vec![
            of_file.clone(),
            VectorCodeSummary {
                summary_text: "Sends one message, and bounces.".into(),
                ..of_function.clone()
            },
        ];
        let found = store.differing_summaries("repo", &changed).await.unwrap();
        assert_eq!((found.places, found.held), (vec![1], 2));
        // Asked about one alone, the other is not counted as held under it.
        let alone = store
            .differing_summaries("repo", &wanted[..1])
            .await
            .unwrap();
        assert_eq!((alone.places.len(), alone.held), (0, 1));

        // An entity as it is, and as it was when it began on another line:
        // kept under another id, beside the one wanted.
        let now = entity(
            "repo",
            "Config",
            "src/config.rs",
            "Struct",
            "the configuration",
        );
        let before = VectorCodeEntity {
            start_line: 40,
            ..now.clone()
        };
        store.index_entities(&[now.clone(), before]).await.unwrap();
        assert_eq!(store.entity_count("repo").await.unwrap(), 2);
        let found = store
            .differing_entities("repo", std::slice::from_ref(&now))
            .await
            .unwrap();
        assert_eq!((found.places, found.held), (vec![0], 2));

        // A doc comment changed, which is kept only in the hash: that
        // place, and not the other.
        let mail = [
            documented("docs", "send", "Sends one message"),
            documented("docs", "bounce", "Bounces one message"),
        ];
        store.index_entities(&mail).await.unwrap();
        let redocumented = [
            mail[0].clone(),
            VectorCodeEntity {
                doc_comment: "Bounces it twice.".into(),
                ..mail[1].clone()
            },
        ];
        let found = store
            .differing_entities("docs", &redocumented)
            .await
            .unwrap();
        assert_eq!((found.places, found.held), (vec![1], 2));
    }

    /// An earlier version held beside a record wanted is stale, and the
    /// record is not missing; the wanted record's own id is never stale,
    /// whatever it holds. A summary has no other id to be held at.
    #[tokio::test]
    async fn earlier_versions_are_stale_but_the_wanted_id_never_is() {
        let Some(node) = Node::embedding().await else {
            return;
        };
        let (store, _) = crate::testing::counted(&node.url, &node.key, &node.tenant, None);
        let now = entity(
            "repo",
            "Config",
            "src/config.rs",
            "Struct",
            "the configuration",
        );
        let before = VectorCodeEntity {
            start_line: 40,
            ..now.clone()
        };
        store
            .index_entities(&[now.clone(), before.clone()])
            .await
            .unwrap();
        let earlier = vec![entity_id(&before).0.to_string()];
        let found = store
            .differing_entities("repo", std::slice::from_ref(&now))
            .await
            .unwrap();
        assert_eq!((found.places, found.missing), (vec![0], vec![]));
        assert_eq!(found.stale, earlier);
        let changed = VectorCodeEntity {
            end_line: 30,
            ..now.clone()
        };
        let found = store
            .differing_entities("repo", std::slice::from_ref(&changed))
            .await
            .unwrap();
        assert_eq!((found.places, found.missing), (vec![0], vec![0]));
        assert_eq!(found.stale, earlier, "its own id is not stale");

        let held = summary("repo", "src/config.rs", "src/config.rs", "Configures.");
        store
            .index_summaries(std::slice::from_ref(&held))
            .await
            .unwrap();
        let rewritten = VectorCodeSummary {
            summary_text: "Configures, and checks.".into(),
            ..held.clone()
        };
        let found = store
            .differing_summaries("repo", std::slice::from_ref(&rewritten))
            .await
            .unwrap();
        assert_eq!((found.places, found.missing), (vec![0], vec![0]));
        assert!(found.stale.is_empty(), "{:?}", found.stale);
    }

    /// A second object of a record's text, at an id not the record's own,
    /// is stale whichever of the two is listed first, and once it is
    /// removed the record agrees.
    #[tokio::test]
    async fn a_second_copy_of_a_records_text_is_stale() {
        let Some(node) = Node::embedding().await else {
            return;
        };
        let (store, _) = crate::testing::counted(&node.url, &node.key, &node.tenant, None);
        let wanted = entity(
            "repo",
            "Config",
            "src/config.rs",
            "Struct",
            "the configuration",
        );
        store
            .index_entities(std::slice::from_ref(&wanted))
            .await
            .unwrap();
        let first = ObjectId(Uuid::nil());
        let vector = vec![0.5_f32; store.embedded_as().1.dimensions as usize];
        let copy = Change::upsert(
            CollectionName::new(CODE_ENTITY_COLLECTION).unwrap(),
            first,
            Content::new(entity_properties(&wanted)).vector(vector),
        );
        store.tenants.writes.apply(vec![copy]).await.unwrap();
        let wanted = std::slice::from_ref(&wanted);
        let found = store.differing_entities("repo", wanted).await.unwrap();
        assert_eq!((found.places, found.missing), (vec![0], vec![]));
        assert_eq!(found.stale, [first.0.to_string()]);
        store.remove_held(false, &found.stale).await.unwrap();
        let found = store.differing_entities("repo", wanted).await.unwrap();
        assert!(
            found.places.is_empty() && found.stale.is_empty(),
            "{found:?}"
        );
    }

    /// A file's records are counted in both collections, and no other
    /// file's or repo's; a compare of more than a page is asked a page at
    /// a time, and finds what a compare of each page does.
    #[tokio::test]
    async fn what_a_file_holds_is_counted_in_both_collections() {
        let Some(node) = Node::embedding().await else {
            return;
        };
        let (store, _) = crate::testing::counted(&node.url, &node.key, &node.tenant, None);
        let many: Vec<VectorCodeEntity> = (0..250)
            .map(|n| {
                let id = format!("e{n:03}");
                entity(
                    "repo",
                    &id,
                    "src/many.rs",
                    "Function",
                    &format!("does {id}"),
                )
            })
            .collect();
        let others = [
            entity("repo", "other", "src/other.rs", "Function", "does another"),
            entity("elsewhere", "e000", "src/many.rs", "Function", "does e000"),
        ];
        store.index_entities(&many).await.unwrap();
        store.index_entities(&others).await.unwrap();
        store
            .index_summaries(&[
                summary("repo", "e000", "src/many.rs", "Does many things."),
                summary("elsewhere", "e000", "src/many.rs", "Does many things."),
            ])
            .await
            .unwrap();
        assert_eq!(store.file_held("repo", "src/many.rs").await.unwrap(), 251);
        assert_eq!(store.file_held("repo", "src/other.rs").await.unwrap(), 1);
        assert_eq!(store.file_held("repo", "src/none.rs").await.unwrap(), 0);

        let mut wanted = many.clone();
        for at in [0, 120, 249] {
            wanted[at].doc_comment = "changed".into();
        }
        let whole = store.differing_entities("repo", &wanted).await.unwrap();
        assert_eq!(whole.places, [0, 120, 249]);
        assert_eq!(whole.held, 250);
        let mut paged = Vec::new();
        for (page, chunk) in wanted.chunks(VERIFY_PAGE).enumerate() {
            let found = store.differing_entities("repo", chunk).await.unwrap();
            paged.extend(found.places.iter().map(|at| page * VERIFY_PAGE + at));
        }
        assert_eq!(whole.places, paged);
    }

    /// Of every repo but those listed, what the store holds is found, and
    /// a record of no repo never is.
    #[tokio::test]
    async fn held_outside_finds_a_dropped_repo_and_never_a_record_with_none() {
        let Some(node) = Node::embedding().await else {
            return;
        };
        let (store, _) = crate::testing::counted(&node.url, &node.key, &node.tenant, None);
        let repos = |repos: &[&str]| repos.iter().map(|r| r.to_string()).collect::<Vec<_>>();
        store
            .index_entities(&[entity("kept", "a", "src/a.rs", "Function", "does a")])
            .await
            .unwrap();
        store
            .index_summaries(&[summary("dropped", "b", "src/b.rs", "Does b.")])
            .await
            .unwrap();
        let mut unowned = entity_properties(&entity("x", "c", "src/c.rs", "Function", "does c"));
        unowned.remove("repoId");
        let vector = vec![0.5_f32; store.embedded_as().1.dimensions as usize];
        let written = Change::upsert(
            CollectionName::new(CODE_ENTITY_COLLECTION).unwrap(),
            ObjectId(Uuid::new_v4()),
            Content::new(unowned).vector(vector),
        );
        store.tenants.writes.apply(vec![written]).await.unwrap();

        assert_eq!(
            store
                .held_outside(&repos(&["kept", "dropped"]))
                .await
                .unwrap(),
            None
        );
        assert_eq!(
            store
                .held_outside(&repos(&["kept"]))
                .await
                .unwrap()
                .as_deref(),
            Some("dropped")
        );
        store.delete_repo_summaries("dropped").await.unwrap();
        assert_eq!(store.held_outside(&repos(&["kept"])).await.unwrap(), None);
        assert_eq!(
            store.held_outside(&[]).await.unwrap().as_deref(),
            Some("kept")
        );
    }

    /// The node holds `CodeEntity` as wanted: its searchable content
    /// embedded and not kept, nothing indexed for keywords, and nothing
    /// kept that nothing reads.
    #[tokio::test]
    async fn the_slim_collection_is_made_as_wanted() {
        for local in [local_embedding(&[]), small()] {
            let Some(node) = Node::embedding().await else {
                return;
            };
            made_as_wanted(embedding_as(&node, local)).await;
        }
    }

    /// The store's collections as the node made them: as wanted, and an
    /// entity found by its text.
    async fn made_as_wanted(store: VellevCodeVectorStore) {
        store.ensure_collections().await.unwrap();
        for collection in [&store.entities, &store.summaries] {
            let made = collection.collection().schema().await.unwrap().unwrap();
            assert_eq!(made.embedding, collection.wanted().embedding);
        }
        let entity = documented("repo", "parse_config", "Reads the TOML configuration file");
        assert_eq!(store.index_entities(&[entity]).await.unwrap(), 1);
        let found = store
            .search_entities("reading the configuration", Some("repo"), None, 5)
            .await
            .unwrap();
        assert_eq!(found[0].entity_id, "parse_config");
        let made = store.entities.collection().schema().await.unwrap().unwrap();
        assert!(!made.property("searchableContent").unwrap().stored);
        assert!(made
            .embedding
            .properties
            .contains(&"searchableContent".to_string()));
        assert!(made.properties.iter().all(|p| !p.searchable), "{made:?}");
        let mut names: Vec<&str> = made
            .properties
            .iter()
            .map(|p| p.name.as_str())
            .filter(|name| !name.starts_with("incarnation_"))
            .collect();
        names.sort_unstable();
        assert_eq!(
            names,
            [
                "endLine",
                "entityId",
                "filePath",
                "fill",
                "kind",
                "qualifiedName",
                "repoId",
                "searchableContent",
                "startLine",
                "textHash"
            ]
        );
    }

    /// What is embedded is not kept, nor what nothing reads, whoever makes
    /// the vectors; and a search by the text finds the entity with all a
    /// result says of it.
    #[tokio::test]
    async fn the_text_is_embedded_not_kept() {
        let Some(node) = Node::embedding().await else {
            return;
        };
        let plain = VellevCodeVectorStore::new(&node.url, &node.key, &node.tenant).unwrap();
        let seeded = VellevCodeVectorStore::new(&node.url, &node.key, &node.tenant)
            .unwrap()
            .seeded_by(Seeding::new(Arc::new(ByNode::of(&node)), 8, 1));
        for (store, repo) in [(&plain, "plain"), (&seeded, "seeded")] {
            let one = documented(repo, "parse_config", "Reads the TOML configuration file");
            assert_eq!(
                store
                    .index_entities(std::slice::from_ref(&one))
                    .await
                    .unwrap(),
                1
            );
            let held = held(store, repo).await;
            assert_eq!(held.len(), 1, "{repo}");
            for left_out in [
                "searchableContent",
                "name",
                "language",
                "signature",
                "docComment",
                "summary",
            ] {
                assert!(!held[0].contains_key(left_out), "{repo}: {left_out} kept");
            }
            let found = store
                .search_entities(&one.searchable_content, Some(repo), None, 5)
                .await
                .unwrap();
            let hit = found.first().expect("found by its text");
            assert_eq!(
                (
                    hit.entity_id.as_str(),
                    hit.repo_id.as_str(),
                    hit.qualified_name.as_str(),
                    hit.kind.as_str(),
                    hit.file_path.as_str(),
                    hit.start_line,
                    hit.end_line
                ),
                (
                    "parse_config",
                    repo,
                    "crate::parse_config",
                    "Function",
                    "src/parse_config.rs",
                    3,
                    12
                )
            );
        }
    }

    /// Writing again what the store holds, under a new fill as a fill
    /// does, has nothing embedded: only the fill changes. What has changed
    /// is embedded, and only that.
    #[tokio::test]
    async fn a_refill_embeds_nothing_that_is_unchanged() {
        let Some(node) = Node::embedding().await else {
            return;
        };
        let by = Arc::new(ByNode::of(&node));
        let store = VellevCodeVectorStore::new(&node.url, &node.key, &node.tenant)
            .unwrap()
            .seeded_by(Seeding::new(by.clone(), 8, 2));
        let embedded = || by.asked.load(std::sync::atomic::Ordering::SeqCst);
        let first: Vec<VectorCodeEntity> = ["parse", "send", "render"]
            .iter()
            .map(|id| {
                documented("repo", id, &format!("{id}s what it is given")).of_generation("one")
            })
            .collect();
        assert_eq!(store.index_entities(&first).await.unwrap(), 3);
        assert_eq!(embedded(), 3);

        let again: Vec<VectorCodeEntity> = first
            .iter()
            .cloned()
            .map(|entity| entity.of_generation("two"))
            .collect();
        assert_eq!(store.index_entities(&again).await.unwrap(), 3);
        assert_eq!(embedded(), 3, "nothing embedded again");
        let fills: Vec<Value> = held(&store, "repo")
            .await
            .into_iter()
            .map(|properties| properties[FILL].clone())
            .collect();
        assert_eq!(fills, [json!("two"), json!("two"), json!("two")]);
        let found = store
            .search_entities(&again[1].searchable_content, Some("repo"), None, 3)
            .await
            .unwrap();
        assert_eq!(found[0].entity_id, "send");

        let mut changed = again.clone();
        changed[1] = VectorCodeEntity {
            doc_comment: "Sends it twice.".into(),
            searchable_content: format!("{} Sends it twice.", again[1].searchable_content),
            ..again[1].clone()
        };
        assert_eq!(store.index_entities(&changed).await.unwrap(), 3);
        assert_eq!(embedded(), 4, "the changed one alone");
        let agreed = store.differing_entities("repo", &changed).await.unwrap();
        assert_eq!((agreed.places.len(), agreed.held), (0, 3));
    }

    /// A patch whose object changed or went meanwhile is refused, and its
    /// entity is written whole instead, as if it had not been held: no
    /// failure is told of.
    #[tokio::test]
    async fn a_patch_that_loses_a_race_is_written_whole_unreported() {
        let Some(node) = Node::embedding().await else {
            return;
        };
        let store = VellevCodeVectorStore::new(&node.url, &node.key, &node.tenant).unwrap();
        let heard = Arc::new(Heard::default());
        store.set_insert_error_sink(Arc::clone(&heard) as Arc<dyn InsertErrorSink>);
        let before = documented("repo", "send", "Sends one message").of_generation("one");
        assert_eq!(
            store
                .index_entities(std::slice::from_ref(&before))
                .await
                .unwrap(),
            1
        );
        let after = VectorCodeEntity {
            doc_comment: "Sends one message, and bounces.".into(),
            searchable_content: format!("{} And bounces.", before.searchable_content),
            ..before.clone()
        }
        .of_generation("two");
        assert_eq!(entity_id(&after), entity_id(&before), "the same object");
        let held_as_wanted = |entities: Vec<VectorCodeEntity>| {
            let store = &store;
            async move {
                let agreed = store.differing_entities("repo", &entities).await.unwrap();
                (agreed.places.len(), agreed.held)
            }
        };
        let changed = Matched::from([entity_id(&after)]);
        assert_eq!(store.write(&[&after], &changed).await.unwrap(), 1);
        assert_eq!(held_as_wanted(vec![after.clone()]).await, (0, 1));

        let unwritten = documented("repo", "bounce", "Bounces one message");
        let gone = Matched::from([entity_id(&unwritten)]);
        assert_eq!(store.write(&[&unwritten], &gone).await.unwrap(), 1);
        assert_eq!(held_as_wanted(vec![after, unwritten]).await, (0, 2));
        assert!(heard.0.lock().unwrap().is_empty(), "nothing told");
    }

    /// A patch the node fails for any other reason than a lost race is
    /// the batch's failure, told to the insert-error sink, not written
    /// whole instead.
    #[tokio::test]
    async fn a_patch_the_node_fails_reaches_the_insert_error_sink() {
        use axum::response::IntoResponse;
        let mut held = entity_collection(&local_embedding(&[])).unwrap();
        held.properties.push(Property {
            filterable: false,
            ..Property::new("incarnation_0", DataType::Bool)
        });
        let changes: Arc<std::sync::Mutex<Vec<Value>>> = Arc::default();
        let sent = Arc::clone(&changes);
        let app = axum::Router::new().fallback(
            move |method: axum::http::Method, uri: axum::http::Uri, body: axum::body::Bytes| {
                let (held, sent) = (held.clone(), Arc::clone(&sent));
                async move {
                    let path = uri.path();
                    if path.ends_with("/collections") {
                        return axum::Json(json!({ "collections": [] })).into_response();
                    }
                    if method != axum::http::Method::POST {
                        return axum::Json(held).into_response();
                    }
                    if path.ends_with("/count") {
                        return axum::Json(json!({ "count": 1 })).into_response();
                    }
                    if path.ends_with("/nearest") {
                        return axum::Json(json!({ "results": [] })).into_response();
                    }
                    if let Ok(body) = serde_json::from_slice(&body) {
                        sent.lock().unwrap().push(body);
                    }
                    axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response()
                }
            },
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let store = VellevCodeVectorStore::new(&url, "key", "workforce").unwrap();
        let heard = Arc::new(Heard::default());
        store.set_insert_error_sink(Arc::clone(&heard) as Arc<dyn InsertErrorSink>);
        let held = entity("r", "e", "f", "Function", "does a thing");
        assert!(store.index_entities(&[held]).await.is_err());
        let heard = heard.0.lock().unwrap();
        assert_eq!(heard.len(), 1, "{heard:?}");
        assert!(heard[0].contains("entities"), "{heard:?}");
        let ops: Vec<Value> = changes
            .lock()
            .unwrap()
            .iter()
            .flat_map(|batch| batch["changes"].as_array().cloned().unwrap_or_default())
            .collect();
        assert!(
            !ops.is_empty() && ops.iter().all(|op| op["op"] == "patch"),
            "patched, never written whole: {ops:?}"
        );
    }

    /// A `CodeEntity` made before the slim shape, keeping more than it
    /// needs, is used as it is: written to, searched and compared, and
    /// its fills patched.
    #[tokio::test]
    async fn a_collection_that_keeps_its_embedded_text_is_still_used() {
        let Some(node) = Node::embedding().await else {
            return;
        };
        let earlier = collection_keeping_text();
        let tenant = node.tenant();
        tenant
            .collection(&earlier.name)
            .create(&earlier)
            .await
            .unwrap();
        let store = VellevCodeVectorStore::new(&node.url, &node.key, &node.tenant).unwrap();
        let logged = workforce_vellev::testing::logged::during(async {
            store.ensure_collections().await.unwrap();
        })
        .await;
        let warned: Vec<&str> = logged
            .lines()
            .filter(|line| line.contains("WARN") && line.contains("collection keeps text"))
            .collect();
        assert!(
            warned.len() == 1 && warned[0].contains("searchableContent"),
            "{logged}"
        );
        let entities = [
            documented("repo", "parse_config", "Reads the TOML configuration file"),
            documented("repo", "send_email", "Delivers a message over SMTP"),
        ];
        assert_eq!(store.index_entities(&entities).await.unwrap(), 2);
        let found = store
            .search_entities("reading the configuration", Some("repo"), None, 5)
            .await
            .unwrap();
        assert_eq!(found[0].entity_id, "parse_config");
        let agreed = store.differing_entities("repo", &entities).await.unwrap();
        assert_eq!((agreed.places.len(), agreed.held), (0, 2));
        let refilled: Vec<VectorCodeEntity> = entities
            .iter()
            .cloned()
            .map(|entity| entity.of_generation("two"))
            .collect();
        assert_eq!(store.index_entities(&refilled).await.unwrap(), 2);
        let fills: Vec<Value> = held(&store, "repo")
            .await
            .into_iter()
            .map(|properties| properties[FILL].clone())
            .collect();
        assert_eq!(fills, [json!("two"), json!("two")]);
    }

    /// The text a code collection of bge-base at 768 is refused with, by a
    /// store configured for bge-small at 384.
    fn refused_as_bge_base(e: &eyre::Report, collection: &str) {
        let Some(Unusable::Refused(_)) = e.downcast_ref::<Unusable>() else {
            panic!("refused, not {e:?}");
        };
        let told = format!("{e:#}");
        for said in [
            &format!("'{collection}'"),
            "it embeds with 'BAAI/bge-base-en-v1.5', expected 'BAAI/bge-small-en-v1.5'",
            "vectors have 768 dimensions, expected 384",
        ] {
            assert!(told.contains(said), "{said}: {told}");
        }
    }

    /// A collection of another model is refused, naming both models and
    /// sizes, and nothing is written to it or added to its schema.
    #[tokio::test]
    async fn a_collection_of_another_size_is_refused_naming_both_and_nothing_written() {
        let today = local_embedding(&[]);
        for made in [
            entity_collection(&today).unwrap(),
            summary_collection(&today).unwrap(),
        ] {
            let Some(node) = Node::embedding().await else {
                return;
            };
            let tenant = node.tenant();
            let held = tenant.collection(&made.name);
            held.create(&made).await.unwrap();
            let before = held.schema().await.unwrap().unwrap();
            let store = embedding_as(&node, small());
            let name = made.name.to_string();
            refused_as_bge_base(&store.ensure_collections().await.unwrap_err(), &name);
            let content = "parse_config reads the configuration";
            let written = match name.as_str() {
                CODE_ENTITY_COLLECTION => store
                    .index_entities(&[entity("r", "e", "f", "Function", content)])
                    .await
                    .map(|_| ()),
                _ => store
                    .index_summaries(&[summary("r", "e", "f", content)])
                    .await
                    .map(|_| ()),
            };
            assert!(written.is_err(), "{name}");
            assert_eq!(held.schema().await.unwrap().unwrap(), before, "{name}");
            assert_eq!(held.count(None).await.unwrap(), 0, "{name}");
        }
    }

    /// A collection replaced underneath a store, at another model's size
    /// as a restored backup of another model would be, is refused once
    /// the store looks again, and nothing is written to it.
    #[tokio::test]
    async fn a_collection_replaced_underneath_at_another_size_is_refused_at_the_recheck() {
        let Some(node) = Node::embedding().await else {
            return;
        };
        let store = embedding_as(&node, small());
        let content = "parse_config reads the configuration";
        let one = entity("r", "e", "f", "Function", content);
        assert_eq!(
            store
                .index_entities(std::slice::from_ref(&one))
                .await
                .unwrap(),
            1
        );
        let tenant = node.tenant();
        let held = tenant.collection(&store.entities.wanted().name);
        held.drop().await.unwrap();
        held.create(&entity_collection(&local_embedding(&[])).unwrap())
            .await
            .unwrap();
        let rechecked = store.entities.recheck().await.map(|_| ()).unwrap_err();
        refused_as_bge_base(&rechecked, CODE_ENTITY_COLLECTION);
        let searched = store.search_entities(content, None, None, 5).await;
        refused_as_bge_base(&searched.unwrap_err(), CODE_ENTITY_COLLECTION);
        let written = store.index_entities(std::slice::from_ref(&one)).await;
        refused_as_bge_base(&written.unwrap_err(), CODE_ENTITY_COLLECTION);
        assert_eq!(held.count(None).await.unwrap(), 0);
    }

    /// Needs a node that runs the local model.
    #[tokio::test]
    async fn code_is_indexed_found_counted_and_deleted() {
        let Some(node) = Node::embedding().await else {
            return;
        };
        let store = VellevCodeVectorStore::new(&node.url, &node.key, &node.tenant).unwrap();
        store.ensure_collections().await.unwrap();
        assert!(store.health_check().await.unwrap());
        let entities = [
            entity(
                "repo",
                "parse_config",
                "src/config.rs",
                "Function",
                "parse_config reads the TOML configuration file",
            ),
            entity(
                "repo",
                "Config",
                "src/config.rs",
                "Struct",
                "Config holds the settings loaded from TOML",
            ),
            entity(
                "repo",
                "send_email",
                "src/mail.rs",
                "Function",
                "send_email delivers a message over SMTP",
            ),
            entity(
                "other",
                "parse_config",
                "src/config.rs",
                "Function",
                "parse_config reads the TOML configuration file",
            ),
            entity("repo", "blank", "src/blank.rs", "Function", "  "),
        ];
        assert_eq!(
            store.index_entities(&entities).await.unwrap(),
            4,
            "the blank one is skipped"
        );
        assert_eq!(store.index_entities(&entities).await.unwrap(), 4);
        assert_eq!(
            store.entity_count("repo").await.unwrap(),
            3,
            "indexing again replaces"
        );
        assert_eq!(store.entity_count("other").await.unwrap(), 1);

        let found = store
            .search_entities("reading the configuration", None, None, 10)
            .await
            .unwrap();
        assert!(found[0].entity_id == "parse_config", "{found:?}");
        assert!(found.iter().all(|r| (0.0..=1.0).contains(&r.score)));
        let in_repo = store
            .search_entities(
                "reading the configuration",
                Some("repo"),
                Some(EntityKind::Struct),
                10,
            )
            .await
            .unwrap();
        assert_eq!(
            in_repo
                .iter()
                .map(|r| (r.repo_id.as_str(), r.entity_id.as_str()))
                .collect::<Vec<_>>(),
            [("repo", "Config")]
        );
        assert_eq!(in_repo[0].file_path, "src/config.rs");
        assert_eq!((in_repo[0].start_line, in_repo[0].end_line), (1, 9));

        let summaries = [
            summary(
                "repo",
                "src/config.rs",
                "src/config.rs",
                "Loads configuration",
            ),
            summary("repo", "src/mail.rs", "src/mail.rs", "Sends mail"),
        ];
        assert_eq!(store.index_summaries(&summaries).await.unwrap(), 2);
        assert_eq!(store.index_summaries(&summaries).await.unwrap(), 2);
        let summaries = store.summaries.records().await.unwrap();
        assert_eq!(summaries.count(None).await.unwrap(), 2);

        let by_file = store
            .delete_entities_by_file("repo", "src/config.rs")
            .await
            .unwrap();
        assert_eq!(by_file.successful, 2);
        assert_eq!(store.entity_count("repo").await.unwrap(), 1);
        assert_eq!(
            store
                .delete_summaries_by_file("repo", "src/mail.rs")
                .await
                .unwrap()
                .successful,
            1
        );
        assert_eq!(
            store.delete_repo_entities("repo").await.unwrap().successful,
            1
        );
        assert_eq!(
            store
                .delete_repo_summaries("repo")
                .await
                .unwrap()
                .successful,
            1
        );
        assert_eq!(store.entity_count("repo").await.unwrap(), 0);
        assert_eq!(
            store.entity_count("other").await.unwrap(),
            1,
            "another repo is untouched"
        );
    }

    /// A collection as vellev's importer makes it from a Weaviate class
    /// copied with its text2vec-transformers vectors is refused for good,
    /// saying to drop it and index again: no write or search reaches it,
    /// and the readiness check reports why.
    #[tokio::test]
    async fn a_collection_holding_vectors_copied_from_weaviate_is_refused() {
        let Some(node) = Node::fresh().await else {
            return;
        };
        let tenant = node.tenant();
        let imported = copied_from_weaviate();
        tenant
            .collection(&imported.name)
            .create(&imported)
            .await
            .unwrap();
        let store = VellevCodeVectorStore::new(&node.url, &node.key, &node.tenant).unwrap();
        let refused = store.ensure_collections().await.unwrap_err().to_string();
        assert!(refused.contains("copied from Weaviate"), "{refused}");
        assert!(refused.contains(REINDEX), "{refused}");

        let unhealthy = store.health_check().await.unwrap_err().to_string();
        assert!(unhealthy.contains(REINDEX), "{unhealthy}");
        let content = "parse_config reads the configuration";
        assert!(store
            .index_entities(&[entity("r", "e", "f", "Function", content)])
            .await
            .is_err());
        assert!(store.search_entities(content, None, None, 5).await.is_err());
        assert!(store.entity_count("r").await.is_err());
        assert!(store.delete_repo_entities("r").await.is_err());
        let held = tenant.collection(&imported.name).count(None).await.unwrap();
        assert_eq!(held, 0, "nothing was written to it");
    }

    /// Indexing `impl A { fn new }` and `impl B { fn new }` keeps both.
    #[tokio::test]
    async fn methods_of_one_name_in_one_file_are_both_indexed() {
        let Some(node) = Node::embedding().await else {
            return;
        };
        let store = VellevCodeVectorStore::new(&node.url, &node.key, &node.tenant).unwrap();
        let method = |qualified: &str, line: u32| VectorCodeEntity {
            entity_id: "same-graph-id".into(),
            qualified_name: qualified.into(),
            start_line: line,
            ..entity(
                "repo",
                "new",
                "src/lib.rs",
                "Method",
                &format!("{qualified} builds one"),
            )
        };
        store
            .index_entities(&[method("A::new", 3), method("B::new", 9)])
            .await
            .unwrap();
        assert_eq!(store.entity_count("repo").await.unwrap(), 2);
    }

    /// A key vellev does not accept, or a tenant it does not have, shows as
    /// unhealthy naming the key or the tenant, and is not taken for a node
    /// that is merely slow.
    #[tokio::test]
    async fn a_refused_key_or_a_missing_tenant_is_unhealthy_saying_which() {
        let Some(node) = Node::fresh().await else {
            return;
        };
        let wrong_key = VellevCodeVectorStore::new(&node.url, "no-such-key", &node.tenant).unwrap();
        let refused = wrong_key.health_check().await.unwrap_err().to_string();
        assert!(
            refused.contains("does not accept the key") && refused.contains(&node.tenant),
            "{refused}"
        );

        let missing = format!("{}-missing", node.tenant);
        let no_tenant = VellevCodeVectorStore::new(&node.url, &node.key, &missing).unwrap();
        let refused = no_tenant.health_check().await.unwrap_err().to_string();
        assert!(refused.contains(&missing), "{refused}");
        assert!(no_tenant
            .index_entities(&[entity("r", "e", "f", "Function", "a note")])
            .await
            .is_err());
    }

    /// A refused collection the operator drops is used again at the next
    /// health check, without a restart.
    #[tokio::test]
    async fn a_refused_collection_dropped_by_the_operator_is_used_again() {
        let Some(node) = Node::fresh().await else {
            return;
        };
        let tenant = node.tenant();
        // Of another model's vectors, as one copied from Weaviate is: a
        // node will not make a collection of its own model with the wrong
        // number of dimensions (from 0.2 it checks them).
        let wrong = copied_from_weaviate();
        tenant.collection(&wrong.name).create(&wrong).await.unwrap();
        let store = VellevCodeVectorStore::new(&node.url, &node.key, &node.tenant).unwrap();
        assert!(store.health_check().await.is_err());
        tenant.collection(&wrong.name).drop().await.unwrap();
        assert!(store.health_check().await.unwrap(), "looked at again");
    }

    /// Needs a node that runs the local model.
    #[tokio::test]
    async fn a_node_that_comes_up_late_is_made_sure_of_on_first_use() {
        let Some(node) = Node::embedding().await else {
            return;
        };
        let (url, port) = Node::not_up_yet().await;
        let store = VellevCodeVectorStore::new(&url, &node.key, &node.tenant).unwrap();
        let content = "parse_config reads the configuration";
        let config = || [entity("r", "e", "f", "Function", content)];
        assert!(store.ensure_collections().await.is_err());
        assert!(!store.health_check().await.unwrap(), "down, not refused");
        assert!(store.index_entities(&config()).await.is_err());

        node.come_up_at(port).await;
        assert!(store.health_check().await.unwrap());
        assert_eq!(store.index_entities(&config()).await.unwrap(), 1);
        assert_eq!(store.entity_count("r").await.unwrap(), 1);
    }

    /// What the node is to embed is written sixteen at a time, one write
    /// after another, however many are stored at once: a question the
    /// node must embed meanwhile waits for one small write, not hundreds.
    #[tokio::test]
    async fn the_node_is_given_few_texts_to_embed_at_a_time() {
        use axum::response::IntoResponse;
        use std::sync::atomic::{AtomicUsize, Ordering};
        let (now, most) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let sizes = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut held = entity_collection(&local_embedding(&[])).unwrap();
        held.properties
            .push(Property::new("incarnation_1", DataType::Bool));
        let (counted, peak, seen) = (now.clone(), most.clone(), sizes.clone());
        let app = axum::Router::new().fallback(
            move |method: axum::http::Method, uri: axum::http::Uri, body: String| {
                let (held, now, most, sizes) =
                    (held.clone(), counted.clone(), peak.clone(), seen.clone());
                async move {
                    if uri.path().ends_with("/collections") {
                        return axum::Json(json!({ "collections": [] })).into_response();
                    }
                    if method != axum::http::Method::POST {
                        return axum::Json(held).into_response();
                    }
                    if uri.path().ends_with("/count") {
                        return axum::Json(json!({ "count": 0 })).into_response();
                    }
                    if uri.path().ends_with("/nearest") {
                        return axum::Json(json!({ "results": [] })).into_response();
                    }
                    let writes = body.matches("\"op\":\"upsert\"").count();
                    sizes.lock().unwrap().push(writes);
                    most.fetch_max(now.fetch_add(1, Ordering::SeqCst) + 1, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    now.fetch_sub(1, Ordering::SeqCst);
                    let outcomes: Vec<_> = (0..writes)
                        .map(|_| json!({ "outcome": "applied" }))
                        .collect();
                    axum::Json(json!({ "incarnation": 1, "position": 1, "outcomes": outcomes }))
                        .into_response()
                }
            },
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let store = VellevCodeVectorStore::new(&url, "key", "workforce").unwrap();
        let entities: Vec<VectorCodeEntity> = (0..70)
            .map(|n| entity("r", &format!("e{n}"), "f", "Function", "does a thing"))
            .collect();
        assert_eq!(store.index_entities(&entities).await.unwrap(), 70);
        assert_eq!(*sizes.lock().unwrap(), [16, 16, 16, 16, 6]);
        assert_eq!(most.load(Ordering::SeqCst), 1, "one write at a time");
        assert_eq!(NODE_EMBEDS_AT_ONCE, 16);
    }

    /// A cluster that writes format 1 cannot hold the slim collection,
    /// and says so: that is what the store reports, and its readiness
    /// check fails with it, rather than as a node not there.
    #[tokio::test]
    async fn a_cluster_writing_format_1_refuses_the_collection_plainly() {
        use axum::response::IntoResponse;
        const REFUSED: &str = "CodeEntity has a property it does not store, which only format 2 can hold, and the cluster writes format 1: it rises to 2 once every node reads format 2 and one is let raise it (VELLEV_MAX_FORMAT=2)";
        let app = axum::Router::new().fallback(
            |method: axum::http::Method, uri: axum::http::Uri| async move {
                let answer = |status: u16, code: &str, message: &str| {
                    (
                        axum::http::StatusCode::from_u16(status).unwrap(),
                        axum::Json(json!({ "error": { "code": code, "message": message } })),
                    )
                        .into_response()
                };
                let path = uri.path();
                if path.ends_with("/collections") || path.ends_with("/health/ready") {
                    return axum::Json(json!({ "collections": [] })).into_response();
                }
                if method == axum::http::Method::GET {
                    return answer(404, "not_found", "no such collection");
                }
                answer(400, "invalid", REFUSED)
            },
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let store = VellevCodeVectorStore::new(&url, "key", "workforce").unwrap();
        let refused = store.ensure_collections().await.unwrap_err();
        match refused.downcast_ref::<Unusable>() {
            Some(Unusable::Invalid { message, .. }) => {
                assert!(message.contains(REFUSED), "{message}")
            }
            other => panic!("expected the node's refusal, got {other:?}"),
        }
        let unhealthy = store.health_check().await.unwrap_err().to_string();
        assert!(unhealthy.contains("VELLEV_MAX_FORMAT=2"), "{unhealthy}");
    }

    #[tokio::test]
    async fn a_failed_batch_reaches_the_insert_error_sink() {
        let store = VellevCodeVectorStore::new("http://127.0.0.1:1", "key", "workforce").unwrap();
        let heard = Arc::new(Heard::default());
        store.set_insert_error_sink(Arc::clone(&heard) as Arc<dyn InsertErrorSink>);
        let content = "parse_config reads the configuration";
        assert!(store
            .index_entities(&[entity("r", "e", "f", "Function", content)])
            .await
            .is_err());
        assert!(store
            .index_summaries(&[summary("r", "e", "f", content)])
            .await
            .is_err());
        let heard = heard.0.lock().unwrap();
        assert_eq!(heard.len(), 2, "{heard:?}");
        assert!(heard[0].contains("entities") && heard[1].contains("summaries"));
    }

    /// Each store names its own database, as `[knowledge] vector_backend`
    /// does: the graph keeps its fills and pending files under that name.
    #[test]
    fn each_store_names_its_own_database() {
        let vellev = VellevCodeVectorStore::new("http://127.0.0.1:1", "key", "workforce").unwrap();
        assert_eq!(vellev.backend(), "vellev");
        let weaviate = crate::vector::WeaviateCodeVectorStore::new("http://127.0.0.1:1", None);
        assert_eq!(weaviate.backend(), "weaviate");
    }

    /// Code vectors are found by meaning only: nothing asks the node a
    /// keyword (BM25) or hybrid question of CodeEntity or CodeSummary, so
    /// their schema need keep no keyword index (track 2, W0). Every way
    /// the store reads or writes is taken, and every request the node is
    /// sent is looked at.
    #[tokio::test]
    async fn no_keyword_or_hybrid_question_is_asked_of_the_code_collections() {
        use axum::response::IntoResponse;
        let asked = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let mut entities = entity_collection(&local_embedding(&[])).unwrap();
        entities
            .properties
            .push(Property::new("incarnation_1", DataType::Bool));
        let mut summaries = summary_collection(&local_embedding(&[])).unwrap();
        summaries
            .properties
            .push(Property::new("incarnation_1", DataType::Bool));
        let seen = asked.clone();
        let app = axum::Router::new().fallback(
            move |method: axum::http::Method, uri: axum::http::Uri| {
                let (entities, summaries, seen) =
                    (entities.clone(), summaries.clone(), seen.clone());
                async move {
                    let path = uri.path().to_string();
                    seen.lock().unwrap().push(format!("{method} {path}"));
                    if path.ends_with("/collections") {
                        return axum::Json(json!({ "collections": [] })).into_response();
                    }
                    if method == axum::http::Method::GET {
                        if path.contains(CODE_SUMMARY_COLLECTION) {
                            return axum::Json(summaries).into_response();
                        }
                        if path.contains(CODE_ENTITY_COLLECTION) {
                            return axum::Json(entities).into_response();
                        }
                        return axum::Json(json!({})).into_response();
                    }
                    if path.ends_with("/count") {
                        return axum::Json(json!({ "count": 0 })).into_response();
                    }
                    if path.ends_with("/list") {
                        return axum::Json(json!({ "objects": [] })).into_response();
                    }
                    if ["/nearest", "/keyword", "/hybrid"]
                        .iter()
                        .any(|route| path.ends_with(route))
                    {
                        return axum::Json(json!({ "results": [] })).into_response();
                    }
                    axum::Json(json!({ "incarnation": 1, "position": 1, "outcomes": [] }))
                        .into_response()
                }
            },
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let store = VellevCodeVectorStore::new(&url, "key", "workforce").unwrap();
        let entity = entity("r", "e", "src/lib.rs", "Function", "parses a config file");
        let summary = VectorCodeSummary {
            entity_id: "e".into(),
            repo_id: "r".into(),
            granularity: "function".into(),
            file_path: "src/lib.rs".into(),
            summary_text: "parses a config file".into(),
            is_stale: false,
            generation: String::new(),
        };
        // Each is taken whether it succeeds against this node or not: what
        // matters is what it asks.
        let _ = store.ensure_collections().await;
        let _ = store.health_check().await;
        let before = asked.lock().unwrap().len();
        for (repo, kind) in [
            (None, None),
            (Some("r"), None),
            (Some("r"), Some(EntityKind::Function)),
        ] {
            let _ = store
                .search_entities("config parsing", repo, kind, 10)
                .await;
        }
        let during = asked.lock().unwrap()[before..].to_vec();
        let _ = store.index_entities(std::slice::from_ref(&entity)).await;
        let _ = store.index_summaries(std::slice::from_ref(&summary)).await;
        let _ = store.entity_count("r").await;
        let _ = store.identity(true).await;
        let _ = store
            .differing_entities("r", std::slice::from_ref(&entity))
            .await;
        let _ = store
            .differing_summaries("r", std::slice::from_ref(&summary))
            .await;
        let _ = store.held_after("r", false, None, 10).await;
        let _ = store.held_after("r", true, None, 10).await;
        let _ = store.remove_entities(std::slice::from_ref(&entity)).await;
        let _ = store.remove_summaries(std::slice::from_ref(&summary)).await;
        let _ = store.delete_entities_by_file("r", "src/lib.rs").await;
        let _ = store.delete_summaries_by_file("r", "src/lib.rs").await;
        let _ = store.delete_repo_entities("r").await;
        let _ = store.delete_repo_summaries("r").await;
        let _ = store.sweep("r", "g").await;

        let asked = asked.lock().unwrap();
        let searched = during
            .iter()
            .filter(|request| request.ends_with(&format!("{CODE_ENTITY_COLLECTION}/nearest")))
            .count();
        assert_eq!(
            searched, 3,
            "each search asked the node by meaning: {during:#?}"
        );
        let lexical: Vec<_> = asked
            .iter()
            .filter(|request| request.ends_with("/keyword") || request.ends_with("/hybrid"))
            .collect();
        assert!(
            lexical.is_empty(),
            "keyword or hybrid questions: {lexical:#?}"
        );
    }
}
