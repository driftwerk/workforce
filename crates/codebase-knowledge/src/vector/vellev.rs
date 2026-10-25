//! The [`CodeVectorStore`] on vellev: `CodeEntity` and `CodeSummary`
//! collections of one tenant, named as in Weaviate and embedded by the
//! node's own bge, so nothing but the database changes for the indexer.
//!
//! A collection is made sure of before its first use, and again before
//! each use while the node does not answer. One of the wrong shape is
//! refused for good: nothing is written to or read from it, and the
//! readiness check reports why. That is what becomes of a collection
//! vellev's importer copied from Weaviate, holding `text2vec-transformers`
//! vectors the node cannot embed queries to match: it is to be dropped,
//! and a full re-index rebuilds it from the repositories.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use eyre::Result;
use serde_json::{json, Map, Value};
use tracing::{debug, info, warn};
use uuid::Uuid;
use vellev_client::api::{Change, Nearest, Page};
use vellev_client::{Client, Tenant};
use vellev_core::{Collection, CollectionName, DataType, ObjectId, Property};
use vellev_index::filter::Filter;
use workforce_vellev::seeding::{apply_embedded, Seeding};
use workforce_vellev::{collection, local_embedding, Cap, Ensured, Unusable};

use super::health::{InsertErrorSink, InsertErrorSlot};
use super::{
    entity_text, search_result, search_text, summary_text, vectorizable, CodeVectorStore,
    DeleteOutcome, Differing, VectorCodeEntity, VectorCodeSummary, VectorSearchResult,
    CODE_ENTITY_COLLECTION, CODE_SUMMARY_COLLECTION,
};
use crate::graph::entities::EntityKind;

/// Records per delete, each one atomic step.
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

/// The [`CodeVectorStore`] on vellev.
pub struct VellevCodeVectorStore {
    base_url: String,
    /// The tenant as writes the node embeds, a batch at a time, reach it.
    writes: Tenant,
    tenants: workforce_vellev::Tenants,
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
    /// Set once the tenant's code vectors are found to be this graph's.
    /// When the tenant's code vectors were last found to be this graph's.
    owned: std::sync::Mutex<Option<std::time::Instant>>,
    /// For how long that is taken as still so, by a write.
    claim_recheck: Duration,
}

impl VellevCodeVectorStore {
    /// The code vectors of `tenant` on the node at `url`, reached with
    /// `api_key`; every request is bounded by the client's timeout.
    pub fn new(url: &str, api_key: &str, tenant: &str) -> Result<Self> {
        let tenants = workforce_vellev::connect(url, api_key, tenant)?;
        Ok(Self {
            base_url: url.to_string(),
            entities: Ensured::new(tenants.reads.clone(), entity_collection()?, REINDEX)
                .identified(),
            summaries: Ensured::new(tenants.reads.clone(), summary_collection()?, REINDEX)
                .identified(),
            tenants: tenants.clone(),
            writes: tenants.writes,
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

    async fn owned_within(&self, recheck: Duration) -> Result<()> {
        let Some(graph) = self.graph.get() else {
            return Ok(());
        };
        let last = *self.owned.lock().unwrap_or_else(|e| e.into_inner());
        if last.is_some_and(|at| at.elapsed() < recheck) {
            return Ok(());
        }
        let owner = workforce_vellev::index_claim(&self.tenants.reads, OWNER, graph).await?;
        let ours = owner == *graph;
        *self.owned.lock().unwrap_or_else(|e| e.into_inner()) = ours.then(std::time::Instant::now);
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
    pub fn embedded_as(&self) -> Result<(&workforce_vellev::Tenants, vellev_core::Embedding)> {
        Ok((&self.tenants, entity_collection()?.embedding))
    }

    /// Store `changes` into `collection`, a batch at a time; the sink hears
    /// of a batch that failed, or of a collection not to be written to, and
    /// the failure ends the indexing.
    async fn store(&self, what: &str, collection: &Ensured, changes: Vec<Change>) -> Result<usize> {
        self.owned().await?;
        let told = |e: eyre::Report| {
            let message = format!("vellev batch index of {what} failed: {e}");
            warn!(error = %e, what, "vellev batch insert failed");
            self.insert_errors.notify(&message);
            eyre::eyre!(message)
        };
        if let Some(seeding) = &self.seeding {
            collection.records().await.map_err(told)?;
            let stored = apply_embedded(&self.writes, collection.wanted(), seeding, changes)
                .await
                .map_err(told)?;
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
                    async move { self.writes.apply(batch).await }
                })
                .await;
            applied.map_err(told)?;
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

    /// The places in `wanted`, `(key, hash)` pairs of one repo, of those
    /// `collection` lacks, holds otherwise, or holds an earlier version of
    /// besides: told by counting where all are held (see
    /// [`workforce_vellev::unmatched`]).
    async fn differing(
        &self,
        collection: &Ensured,
        repo_id: &str,
        key: &str,
        wanted: &[(String, String)],
    ) -> Result<Differing> {
        let scope = of_repo(repo_id);
        let found = collection
            .run(|| {
                workforce_vellev::unmatched(
                    collection.collection(),
                    Some(&scope),
                    (key, HASH),
                    wanted,
                    &self.cap,
                )
            })
            .await?;
        // An earlier version held beside the wanted one differs too.
        let keys: std::collections::HashSet<&String> = found
            .differing
            .iter()
            .chain(found.stale.iter().map(|(_, key)| key))
            .collect();
        Ok(Differing {
            places: wanted
                .iter()
                .enumerate()
                .filter(|(_, (key, _))| keys.contains(key))
                .map(|(at, _)| at)
                .collect(),
            held: usize::try_from(found.held)?,
        })
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
                        .map(|id| Change::Delete {
                            collection: collection.name().clone(),
                            id: *id,
                        })
                        .collect();
                    async move { self.writes.apply(deletes).await }
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
                    &self.writes,
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

/// `CodeEntity` as Weaviate has it, embedded by the node on its searchable
/// content.
fn entity_collection() -> Result<Collection> {
    Ok(collection(
        CollectionName::new(CODE_ENTITY_COLLECTION)?,
        [
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
        ]
        .into(),
        local_embedding(&["searchableContent"]),
    ))
}

/// `CodeSummary` as Weaviate has it, embedded by the node on its text.
fn summary_collection() -> Result<Collection> {
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
        local_embedding(&["summaryText"]),
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
    Ok(Change::Upsert {
        collection: CollectionName::new(records)?,
        id,
        properties: match properties {
            Value::Object(properties) => properties,
            _ => Map::new(),
        },
        vector: None,
    })
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

    fn set_insert_error_sink(&self, sink: Arc<dyn InsertErrorSink>) {
        self.insert_errors.set(sink);
    }

    async fn index_entities(&self, entities: &[VectorCodeEntity]) -> Result<usize> {
        let changes = vectorizable(entities, entity_text, "code_index_entities", "entities")
            .into_iter()
            .map(|(entity, searchable)| {
                upsert(
                    CODE_ENTITY_COLLECTION,
                    entity_id(entity),
                    json!({
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
                        FILL: entity.generation,
                        HASH: entity.record_hash(),
                    }),
                )
            })
            .collect::<Result<Vec<_>>>()?;
        self.store("entities", &self.entities, changes).await
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
                    let nearest = Nearest {
                        vector: None,
                        text: Some(query.as_str().to_string()),
                        limit,
                        filter: filter.clone(),
                        oversample: None,
                        vectors: false,
                    };
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
        workforce_vellev::index_unmark(&self.tenants.reads, OWNER).await?;
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
        let wanted: Vec<(String, String)> = entities
            .iter()
            .map(|entity| (entity.entity_id.clone(), entity.record_hash()))
            .collect();
        self.differing(&self.entities, repo_id, "entityId", &wanted)
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
        self.differing(&self.summaries, repo_id, SUMMARY_KEY, &wanted)
            .await
    }

    async fn held_after(
        &self,
        repo_id: &str,
        summaries: bool,
        after: Option<String>,
        limit: usize,
    ) -> Result<(Vec<(String, String)>, Option<String>)> {
        let (collection, key) = match summaries {
            true => (&self.summaries, SUMMARY_KEY),
            false => (&self.entities, "entityId"),
        };
        let after = match after {
            Some(after) => Some(ObjectId(after.parse()?)),
            None => None,
        };
        let filter = of_repo(repo_id);
        let listed = collection
            .run(|| {
                self.cap.ask(limit, |limit| {
                    let page = Page {
                        filter: Some(filter.clone()),
                        after,
                        limit,
                        vectors: false,
                    };
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
                (property(key), property("filePath"))
            })
            .collect();
        Ok((held, listed.next.map(|next| next.0.to_string())))
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

#[cfg(test)]
mod tests {
    use workforce_vellev::testing::Node;

    use super::*;

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
        assert!(held.iter().all(|(key, _)| key.starts_with("ours-")));
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
        let mut imported = entity_collection().unwrap();
        imported.embedding = vellev_core::Embedding {
            provider: vellev_core::EmbeddingProvider::None,
            model: "weaviate:text2vec-transformers".into(),
            dimensions: 384,
            query_prefix: None,
            document_prefix: None,
            properties: Vec::new(),
        };
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
        let mut wrong = entity_collection().unwrap();
        wrong.embedding = vellev_core::Embedding {
            provider: vellev_core::EmbeddingProvider::None,
            model: "weaviate:text2vec-transformers".into(),
            dimensions: 384,
            query_prefix: None,
            document_prefix: None,
            properties: Vec::new(),
        };
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
        let mut held = entity_collection().unwrap();
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

    #[tokio::test]
    async fn a_failed_batch_reaches_the_insert_error_sink() {
        struct Heard(std::sync::Mutex<Vec<String>>);
        impl InsertErrorSink for Heard {
            fn record(&self, message: String) {
                self.0.lock().unwrap().push(message);
            }
        }
        let store = VellevCodeVectorStore::new("http://127.0.0.1:1", "key", "workforce").unwrap();
        let heard = Arc::new(Heard(Default::default()));
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
        let mut entities = entity_collection().unwrap();
        entities
            .properties
            .push(Property::new("incarnation_1", DataType::Bool));
        let mut summaries = summary_collection().unwrap();
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
