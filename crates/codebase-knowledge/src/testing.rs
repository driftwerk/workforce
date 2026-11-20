//! Vector stores kept in memory, for tests here and in other crates: one
//! that keeps what it is given as a real store does and can be made to
//! fail or change the graph as told ([`Held`]), and one that counts every
//! call made of it ([`Counting`]); a vellev store whose embeds are
//! counted ([`counted`]), and a collection of entities in the shape that
//! keeps their text ([`collection_keeping_text`]).

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use eyre::{eyre, Result};
use futures::future::BoxFuture;
use vellev_core::Embedding;
use workforce_vellev::seeding::Seeding;
use workforce_vellev::testing::Counted;

use crate::graph::entities::{CodeEntity, EntityKind};
use crate::graph::vectors::FillStage;
use crate::graph::{CodeGraph, VectorKeys};
use crate::indexer::fill::VectorFiller;
pub use crate::vector::vellev::collection_keeping_text;
use crate::vector::{
    health::InsertErrorSink, CodeVectorStore, DeleteOutcome, Differing, HeldRecord,
    VectorCodeEntity, VectorCodeSummary, VectorSearchResult, VellevCodeVectorStore,
};

/// What `graph` has of the repo, as a store holds it.
pub async fn as_held(
    graph: &CodeGraph,
    repo_id: &str,
) -> (Vec<VectorCodeEntity>, Vec<VectorCodeSummary>) {
    let entities = graph.all_entities(repo_id).await.expect("the entities");
    let summaries = graph
        .summaries_page(repo_id, "", None, 1_000_000)
        .await
        .expect("the summaries");
    (
        entities
            .iter()
            .map(|entity| VectorCodeEntity::from_code_entity(entity, repo_id, None))
            .collect(),
        summaries
            .iter()
            .map(|summary| VectorCodeSummary::of(summary, repo_id, ""))
            .collect(),
    )
}

/// Step the repo's fill or adoption until it is done, doing `each` after
/// every step: the stage each step left it at.
pub async fn stepped<F: Future<Output = ()>>(
    filler: &VectorFiller,
    repo_id: &str,
    mut each: impl FnMut() -> F,
) -> Vec<FillStage> {
    let mut stages = Vec::new();
    for _ in 0..10_000 {
        let fill = filler.step(repo_id).await.expect("a step");
        stages.push(fill.stage);
        each().await;
        if fill.stage == FillStage::Done {
            return stages;
        }
    }
    panic!("the fill of '{repo_id}' did not end");
}

/// A store on the vellev tenant `tenant` of `url`, embedding as `local`
/// where said, given vectors made from each text's hash, which are
/// counted: what is written whole.
pub fn counted(
    url: &str,
    key: &str,
    tenant: &str,
    local: Option<&Embedding>,
) -> (VellevCodeVectorStore, Arc<Counted>) {
    let store = VellevCodeVectorStore::new(url, key, tenant).expect("a store");
    let store = match local {
        Some(local) => store.embedding(local.clone()).expect("the embedding"),
        None => store,
    };
    let counted = Arc::new(Counted::new(store.embedded_as().1.dimensions as usize));
    (
        store.seeded_by(Seeding::new(counted.clone(), 200, 2)),
        counted,
    )
}

/// What a [`Held`] store does to the graph, as an index run would, as a
/// write of entities comes in and before it lands.
pub enum Meanwhile {
    /// A file's entities leave the graph, and its vectors the store.
    FileDeleted(&'static str),
    /// The collection is lost with all it holds.
    Lost,
}

/// Vectors kept in memory, each under what a real store makes its id from.
/// It counts what it is asked to embed and to purge, fails what is
/// switched on, and does a [`Meanwhile`] once.
#[derive(Default)]
pub struct Held {
    pub entities: Mutex<BTreeMap<String, VectorCodeEntity>>,
    pub summaries: Mutex<BTreeMap<String, VectorCodeSummary>>,
    /// How many times the collection was made.
    pub made: AtomicUsize,
    /// Entities and summaries embedded, in all.
    pub embedded: AtomicUsize,
    /// Whole-repo purges asked for.
    pub repo_purges: AtomicUsize,
    /// A file's purges asked for, of entities or summaries.
    pub file_purges: AtomicUsize,
    /// Writes of entities or summaries asked for, however many each held.
    pub writes: AtomicUsize,
    pub fail_index: AtomicBool,
    pub fail_file_purge: AtomicBool,
    /// The next look for a repo held besides the graph's fails.
    pub fail_held_outside: AtomicBool,
    /// A repo registered in the graph as the next look for a repo held
    /// besides the graph's comes in.
    pub registered_meanwhile: Mutex<Option<String>>,
    /// A search is accepted and never answered.
    pub hang_search: AtomicBool,
    /// Keeps no track of what it holds of a repo, as Weaviate's store.
    pub untracked: AtomicBool,
    /// What a search finds.
    pub found: Mutex<Vec<VectorSearchResult>>,
    /// What a search finds by the repo and text asked, in place of `found`.
    pub answers: Mutex<BTreeMap<(String, String), Vec<VectorSearchResult>>>,
    /// Every search asked.
    pub searches: Mutex<Vec<Search>>,
    /// The search, counted from one, that fails.
    pub failing_search: Mutex<Option<usize>>,
    /// What is done after the search counted from one, before it is answered.
    pub after_search: Mutex<Option<(usize, Hook)>>,
    /// Pages of what is held that were listed.
    pub listed: AtomicUsize,
    /// The first entity of each page of entities compared.
    pub compared_from: Mutex<Vec<String>>,
    /// Writes of entities that succeed before every later one fails.
    pub writes_left: Mutex<Option<usize>>,
    /// Writes of entities that land before the next one lands and never
    /// returns, as a worker stopped between a write and its record.
    pub park_after: Mutex<Option<usize>>,
    pub meanwhile: Mutex<Option<Meanwhile>>,
    /// Entities that come into the graph as the next write of entities
    /// comes in: an index run's, between a page's two reads.
    pub crowd: Mutex<Vec<CodeEntity>>,
    /// The crowd comes in as every write does, changed again each time.
    pub restless: AtomicBool,
    /// Entities that come into the graph and the store as the next
    /// removal by id comes in: an index run's, and its write.
    pub on_remove: Mutex<Vec<CodeEntity>>,
    /// An entity that is to be held whenever entities are written, and
    /// whether a write came in while it was not.
    pub kept: Mutex<Option<String>>,
    pub unkept: AtomicBool,
    pub graph: Mutex<Option<Arc<CodeGraph>>>,
    /// Tell which collection is held from what was last looked at afresh,
    /// as a real store does between its looks.
    pub remembering: AtomicBool,
    pub remembered: AtomicUsize,
    /// A write of entities waits here for leave to go on, if set: one
    /// permit lets one write through.
    pub gate: Mutex<Option<Arc<tokio::sync::Semaphore>>>,
    /// As many writes of entities as this has permits are with the store
    /// at once, if set, as with an embedder that takes so many requests:
    /// the others wait their turn before they come to the gate.
    pub room: Mutex<Option<Arc<tokio::sync::Semaphore>>>,
    /// Writes of entities that have had their turn.
    pub entered: AtomicUsize,
    /// Writes of entities come in and not yet gone, however they go:
    /// stored, failed, or dropped where they wait.
    pub writing: Arc<AtomicUsize>,
}

/// What a [`Held`] store does after a search, before answering it.
pub type Hook = Arc<dyn Fn() -> BoxFuture<'static, ()> + Send + Sync>;

/// A search asked of a [`Held`] store.
#[derive(Debug, Clone, PartialEq)]
pub struct Search {
    pub text: String,
    pub repo_id: Option<String>,
    pub limit: usize,
}

/// One write counted as under way until this is dropped.
struct Writing(Arc<AtomicUsize>);

impl Writing {
    fn begun(under_way: &Arc<AtomicUsize>) -> Self {
        under_way.fetch_add(1, Ordering::SeqCst);
        Self(Arc::clone(under_way))
    }
}

impl Drop for Writing {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// What a real store makes an entity's id from.
pub fn entity_key(entity: &VectorCodeEntity) -> String {
    format!(
        "{}|{}|{}|{}|{}",
        entity.repo_id,
        entity.entity_id,
        entity.qualified_name,
        entity.signature,
        entity.start_line
    )
}

/// What a real store makes a summary's id from.
pub fn summary_key(summary: &VectorCodeSummary) -> String {
    format!(
        "{}|{}|{}",
        summary.repo_id, summary.granularity, summary.entity_id
    )
}

/// How `wanted`, each with its key, id and hash, differs from `held`, each
/// with its key and hash by id, as vellev tells it: the first object held
/// under a key with the wanted hash is the one wanted, and every other
/// under the key is stale, but for the wanted record's own id.
fn differing_from<'a>(
    held: impl Iterator<Item = (&'a String, String, String)> + Clone,
    wanted: impl Iterator<Item = (String, String, String)>,
) -> Differing {
    let mut differing = Differing::default();
    for (at, (key, id, hash)) in wanted.enumerate() {
        let under: Vec<(&String, String)> = held
            .clone()
            .filter(|(_, held_key, _)| *held_key == key)
            .map(|(held_id, _, held_hash)| (held_id, held_hash))
            .collect();
        let right = under.iter().position(|(_, held_hash)| *held_hash == hash);
        let stale: Vec<String> = under
            .iter()
            .enumerate()
            .filter(|(place, _)| Some(*place) != right)
            .map(|(_, (held_id, _))| (*held_id).clone())
            .collect();
        if right.is_none() {
            differing.missing.push(at);
        }
        if right.is_none() || !stale.is_empty() {
            differing.places.push(at);
        }
        differing.held += under.len();
        differing
            .stale
            .extend(stale.into_iter().filter(|held_id| *held_id != id));
    }
    differing
}

impl Held {
    /// The graph ids of the entities held.
    pub fn ids(&self) -> Vec<String> {
        let held = self.entities.lock().unwrap();
        let mut ids: Vec<String> = held.values().map(|e| e.entity_id.clone()).collect();
        ids.sort();
        ids
    }

    pub fn files(&self) -> Vec<String> {
        let held = self.entities.lock().unwrap();
        let mut files: Vec<String> = held.values().map(|e| e.file_path.clone()).collect();
        files.sort();
        files.dedup();
        files
    }

    pub fn generations(&self) -> Vec<String> {
        let held = self.entities.lock().unwrap();
        let mut generations: Vec<String> = held.values().map(|e| e.generation.clone()).collect();
        generations.sort();
        generations.dedup();
        generations
    }

    pub fn embedded(&self) -> usize {
        self.embedded.load(Ordering::SeqCst)
    }

    /// Lose the collection with all it holds, as a restored backup that
    /// left it out does: made again, it is another.
    pub fn lose(&self) {
        self.entities.lock().unwrap().clear();
        self.summaries.lock().unwrap().clear();
        self.made.fetch_add(1, Ordering::SeqCst);
    }

    fn refuse(&self, switch: &AtomicBool, what: &str) -> Result<()> {
        if switch.load(Ordering::SeqCst) {
            return Err(eyre!("the vector store refuses to {what}"));
        }
        Ok(())
    }

    /// Hold a vector of `repo_id`'s of an entity the graph does not have.
    pub fn hold_stale(&self, repo_id: &str, id: &str) {
        let stale = VectorCodeEntity {
            entity_id: id.to_string(),
            repo_id: repo_id.to_string(),
            qualified_name: id.to_string(),
            name: id.to_string(),
            kind: "Function".into(),
            file_path: "src/gone.rs".into(),
            language: "Rust".into(),
            signature: String::new(),
            doc_comment: String::new(),
            summary: String::new(),
            searchable_content: id.to_string(),
            start_line: 1,
            end_line: 2,
            generation: String::new(),
        };
        self.hold(&[stale]);
    }

    /// Hold `entities` as they are, embedding nothing.
    pub fn hold(&self, entities: &[VectorCodeEntity]) {
        let mut held = self.entities.lock().unwrap();
        for entity in entities {
            held.insert(entity_key(entity), entity.clone());
        }
    }

    /// Hold `summaries` as they are, embedding nothing.
    pub fn hold_summaries(&self, summaries: &[VectorCodeSummary]) {
        let mut held = self.summaries.lock().unwrap();
        for summary in summaries {
            held.insert(summary_key(summary), summary.clone());
        }
    }

    /// Hold the repo as `graph` has it, as a store built elsewhere does.
    pub async fn hold_graph(&self, graph: &CodeGraph, repo_id: &str) {
        let (entities, summaries) = as_held(graph, repo_id).await;
        self.hold(&entities);
        self.hold_summaries(&summaries);
    }

    fn graph(&self) -> Arc<CodeGraph> {
        self.graph.lock().unwrap().clone().expect("a graph")
    }
}

#[async_trait]
impl CodeVectorStore for Held {
    fn backend(&self) -> &'static str {
        VectorKeys::VELLEV
    }
    async fn ensure_collections(&self) -> Result<()> {
        Ok(())
    }

    async fn health_check(&self) -> Result<bool> {
        Ok(true)
    }

    fn base_url(&self) -> &str {
        "memory"
    }

    fn set_insert_error_sink(&self, _sink: Arc<dyn InsertErrorSink>) {}

    async fn index_entities(&self, entities: &[VectorCodeEntity]) -> Result<usize> {
        self.writes.fetch_add(1, Ordering::SeqCst);
        self.refuse(&self.fail_index, "index")?;
        if entities.is_empty() {
            return Ok(0);
        }
        let _writing = Writing::begun(&self.writing);
        let room = self.room.lock().unwrap().clone();
        let _turn = match room {
            Some(room) => Some(room.acquire_owned().await.expect("open")),
            None => None,
        };
        self.entered.fetch_add(1, Ordering::SeqCst);
        let gate = self.gate.lock().unwrap().clone();
        if let Some(gate) = gate {
            gate.acquire().await.expect("open").forget();
        }
        if let Some(left) = self.writes_left.lock().unwrap().as_mut() {
            if *left == 0 {
                return Err(eyre!("the vector store went away"));
            }
            *left -= 1;
        }
        let kept = self.kept.lock().unwrap().clone();
        if let Some(kept) = kept {
            let held = self.entities.lock().unwrap();
            if !held.values().any(|entity| entity.entity_id == kept) {
                self.unkept.store(true, Ordering::SeqCst);
            }
        }
        let crowd = {
            let mut crowd = self.crowd.lock().unwrap();
            match self.restless.load(Ordering::SeqCst) {
                true => {
                    for entity in crowd.iter_mut() {
                        let said = entity.doc_comment.clone().unwrap_or_default();
                        entity.doc_comment = Some(format!("{said} Again."));
                    }
                    crowd.clone()
                }
                false => std::mem::take(&mut *crowd),
            }
        };
        if !crowd.is_empty() {
            self.graph().upsert_entities(&crowd).await?;
        }
        let meanwhile = self.meanwhile.lock().unwrap().take();
        match meanwhile {
            Some(Meanwhile::FileDeleted(file)) => {
                let repo = entities
                    .first()
                    .map_or("", |entity| entity.repo_id.as_str());
                self.graph().delete_file_entities(repo, file).await?;
                let mut held = self.entities.lock().unwrap();
                held.retain(|_, entity| entity.file_path != file);
            }
            Some(Meanwhile::Lost) => self.lose(),
            None => {}
        }
        self.hold(entities);
        self.embedded.fetch_add(entities.len(), Ordering::SeqCst);
        let parked = {
            let mut park = self.park_after.lock().unwrap();
            let last = park.is_some_and(|left| left <= 1);
            *park = park.filter(|_| !last).map(|left| left - 1);
            last
        };
        if parked {
            std::future::pending::<()>().await;
        }
        Ok(entities.len())
    }

    async fn search_entities(
        &self,
        query: &str,
        repo_id: Option<&str>,
        _kind: Option<EntityKind>,
        limit: usize,
    ) -> Result<Vec<VectorSearchResult>> {
        if self.hang_search.load(Ordering::SeqCst) {
            std::future::pending::<()>().await;
        }
        let n = {
            let mut searches = self.searches.lock().unwrap();
            searches.push(Search {
                text: query.to_string(),
                repo_id: repo_id.map(str::to_string),
                limit,
            });
            searches.len()
        };
        if *self.failing_search.lock().unwrap() == Some(n) {
            return Err(eyre!("the store went away at search {n}"));
        }
        let after = self.after_search.lock().unwrap().clone();
        if let Some((_, hook)) = after.filter(|(after, _)| *after == n) {
            hook().await;
        }
        let key = (repo_id.unwrap_or_default().to_string(), query.to_string());
        let answered = self.answers.lock().unwrap().get(&key).cloned();
        Ok(answered.unwrap_or_else(|| self.found.lock().unwrap().clone()))
    }

    async fn differing_entities(
        &self,
        repo_id: &str,
        entities: &[VectorCodeEntity],
    ) -> Result<Differing> {
        if let Some(first) = entities.first() {
            let mut compared = self.compared_from.lock().unwrap();
            compared.push(first.entity_id.clone());
        }
        let held = self.entities.lock().unwrap();
        let held = held
            .iter()
            .filter(|(_, held)| held.repo_id == repo_id)
            .map(|(id, held)| (id, held.entity_id.clone(), held.record_hash()));
        let wanted = entities.iter().map(|wanted| {
            (
                wanted.entity_id.clone(),
                entity_key(wanted),
                wanted.record_hash(),
            )
        });
        Ok(differing_from(held, wanted))
    }

    async fn differing_summaries(
        &self,
        repo_id: &str,
        summaries: &[VectorCodeSummary],
    ) -> Result<Differing> {
        let held = self.summaries.lock().unwrap();
        let held = held
            .iter()
            .filter(|(_, held)| held.repo_id == repo_id)
            .map(|(id, held)| (id, held.key(), held.record_hash()));
        let wanted = summaries
            .iter()
            .map(|wanted| (wanted.key(), summary_key(wanted), wanted.record_hash()));
        Ok(differing_from(held, wanted))
    }

    async fn held_after(
        &self,
        repo_id: &str,
        summaries: bool,
        after: Option<String>,
        limit: usize,
    ) -> Result<(Vec<HeldRecord>, Option<String>)> {
        self.listed.fetch_add(1, Ordering::SeqCst);
        let record = |id: &String, key: String, file_path: &String| HeldRecord {
            id: id.clone(),
            key,
            file_path: file_path.clone(),
        };
        let all: Vec<HeldRecord> = if summaries {
            let held = self.summaries.lock().unwrap();
            held.iter()
                .filter(|(_, s)| s.repo_id == repo_id)
                .map(|(id, s)| record(id, s.key(), &s.file_path))
                .collect()
        } else {
            let held = self.entities.lock().unwrap();
            held.iter()
                .filter(|(_, e)| e.repo_id == repo_id)
                .map(|(id, e)| record(id, e.entity_id.clone(), &e.file_path))
                .collect()
        };
        let rest: Vec<HeldRecord> = all
            .into_iter()
            .filter(|held| after.as_ref().is_none_or(|after| held.id > *after))
            .collect();
        let next = (rest.len() > limit).then(|| rest[limit - 1].id.clone());
        Ok((rest.into_iter().take(limit).collect(), next))
    }

    async fn remove_held(&self, summaries: bool, ids: &[String]) -> Result<()> {
        let added = std::mem::take(&mut *self.on_remove.lock().unwrap());
        if !added.is_empty() {
            self.graph().upsert_entities(&added).await?;
            let written: Vec<VectorCodeEntity> = added
                .iter()
                .map(|entity| VectorCodeEntity::from_code_entity(entity, &entity.repo_id, None))
                .collect();
            self.hold(&written);
        }
        match summaries {
            true => self
                .summaries
                .lock()
                .unwrap()
                .retain(|id, _| !ids.contains(id)),
            false => self
                .entities
                .lock()
                .unwrap()
                .retain(|id, _| !ids.contains(id)),
        }
        Ok(())
    }

    async fn held_outside(&self, repos: &[String]) -> Result<Option<String>> {
        if self.fail_held_outside.swap(false, Ordering::SeqCst) {
            return Err(eyre!("the vector store could not be listed"));
        }
        let registered = self.registered_meanwhile.lock().unwrap().take();
        if let Some(repo) = registered {
            self.graph().register_repo(&repo, &repo, "/nowhere").await?;
        }
        let outside = |repo: &String| !repos.contains(repo);
        let entities = self.entities.lock().unwrap();
        let summaries = self.summaries.lock().unwrap();
        Ok(entities
            .values()
            .map(|entity| &entity.repo_id)
            .chain(summaries.values().map(|summary| &summary.repo_id))
            .find(|repo| !repo.is_empty() && outside(repo))
            .cloned())
    }

    async fn file_held(&self, repo_id: &str, file_path: &str) -> Result<usize> {
        let of_file = |repo: &str, file: &str| repo == repo_id && file == file_path;
        let entities = self.entities.lock().unwrap();
        let summaries = self.summaries.lock().unwrap();
        Ok(entities
            .values()
            .filter(|entity| of_file(&entity.repo_id, &entity.file_path))
            .count()
            + summaries
                .values()
                .filter(|summary| of_file(&summary.repo_id, &summary.file_path))
                .count())
    }

    async fn delete_repo_entities(&self, repo_id: &str) -> Result<DeleteOutcome> {
        self.repo_purges.fetch_add(1, Ordering::SeqCst);
        let mut held = self.entities.lock().unwrap();
        held.retain(|_, entity| entity.repo_id != repo_id);
        Ok(DeleteOutcome::default())
    }

    async fn delete_entities_by_file(
        &self,
        repo_id: &str,
        file_path: &str,
    ) -> Result<DeleteOutcome> {
        self.file_purges.fetch_add(1, Ordering::SeqCst);
        self.refuse(&self.fail_file_purge, "purge the file")?;
        let mut held = self.entities.lock().unwrap();
        held.retain(|_, entity| entity.repo_id != repo_id || entity.file_path != file_path);
        Ok(DeleteOutcome::default())
    }

    async fn index_summaries(&self, summaries: &[VectorCodeSummary]) -> Result<usize> {
        self.writes.fetch_add(1, Ordering::SeqCst);
        self.refuse(&self.fail_index, "index")?;
        self.hold_summaries(summaries);
        self.embedded.fetch_add(summaries.len(), Ordering::SeqCst);
        Ok(summaries.len())
    }

    async fn delete_repo_summaries(&self, repo_id: &str) -> Result<DeleteOutcome> {
        let mut held = self.summaries.lock().unwrap();
        held.retain(|_, summary| summary.repo_id != repo_id);
        Ok(DeleteOutcome::default())
    }

    async fn delete_summaries_by_file(
        &self,
        repo_id: &str,
        file_path: &str,
    ) -> Result<DeleteOutcome> {
        self.file_purges.fetch_add(1, Ordering::SeqCst);
        self.refuse(&self.fail_file_purge, "purge the file")?;
        let mut held = self.summaries.lock().unwrap();
        held.retain(|_, summary| summary.repo_id != repo_id || summary.file_path != file_path);
        Ok(DeleteOutcome::default())
    }

    async fn entity_count(&self, repo_id: &str) -> Result<usize> {
        let held = self.entities.lock().unwrap();
        Ok(held.values().filter(|e| e.repo_id == repo_id).count())
    }

    async fn summary_count(&self, repo_id: &str) -> Result<usize> {
        let held = self.summaries.lock().unwrap();
        Ok(held.values().filter(|s| s.repo_id == repo_id).count())
    }

    fn tracks_completeness(&self) -> bool {
        !self.untracked.load(Ordering::SeqCst)
    }

    async fn identity(&self, afresh: bool) -> Result<String> {
        let made = self.made.load(Ordering::SeqCst);
        if afresh || !self.remembering.load(Ordering::SeqCst) {
            self.remembered.store(made, Ordering::SeqCst);
        }
        Ok(format!("making-{}", self.remembered.load(Ordering::SeqCst)))
    }

    async fn remove_entities(&self, entities: &[VectorCodeEntity]) -> Result<()> {
        let mut held = self.entities.lock().unwrap();
        for entity in entities {
            held.remove(&entity_key(entity));
        }
        Ok(())
    }

    async fn remove_summaries(&self, summaries: &[VectorCodeSummary]) -> Result<()> {
        let mut held = self.summaries.lock().unwrap();
        for summary in summaries {
            held.remove(&summary_key(summary));
        }
        Ok(())
    }

    async fn sweep(&self, repo_id: &str, generation: &str) -> Result<usize> {
        let mut entities = self.entities.lock().unwrap();
        let mut summaries = self.summaries.lock().unwrap();
        let before = entities.len() + summaries.len();
        entities.retain(|_, entity| entity.repo_id != repo_id || entity.generation == generation);
        summaries
            .retain(|_, summary| summary.repo_id != repo_id || summary.generation == generation);
        Ok(before - entities.len() - summaries.len())
    }
}

/// How a [`Counting`] store fails, where it does.
pub enum Failing {
    Not,
    Misrouted,
    Otherwise,
}

/// A store that counts every call made of it, keeping track of what it
/// holds or not, and failing which way it is told to where a question
/// asks it first, once `spared` such askings have not: `identity` when it
/// keeps track, the search when not.
pub struct Counting {
    calls: AtomicUsize,
    /// A file's purges asked for, of entities or summaries.
    purges: AtomicUsize,
    /// Writes and deletes asked for, of anything.
    changes: AtomicUsize,
    tracked: bool,
    failing: Failing,
    spared: AtomicUsize,
}

/// What a [`Counting`] store that keeps track says its collection is.
pub const RECORDED: &str = "recorded";

impl Counting {
    pub fn new(tracked: bool, failing: Failing) -> Arc<Self> {
        Self::sparing(tracked, failing, 0)
    }

    pub fn sparing(tracked: bool, failing: Failing, spared: usize) -> Arc<Self> {
        Arc::new(Self {
            calls: AtomicUsize::new(0),
            purges: AtomicUsize::new(0),
            changes: AtomicUsize::new(0),
            tracked,
            failing,
            spared: AtomicUsize::new(spared),
        })
    }

    pub fn called(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    pub fn purged(&self) -> usize {
        self.purges.load(Ordering::SeqCst)
    }

    pub fn changed(&self) -> usize {
        self.changes.load(Ordering::SeqCst)
    }

    fn change(&self) {
        self.call();
        self.changes.fetch_add(1, Ordering::SeqCst);
    }

    fn purge(&self) {
        self.change();
        self.purges.fetch_add(1, Ordering::SeqCst);
    }

    fn call(&self) {
        self.calls.fetch_add(1, Ordering::SeqCst);
    }

    fn asked(&self) -> Result<()> {
        self.call();
        let spare = |left: usize| left.checked_sub(1);
        if self
            .spared
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, spare)
            .is_ok()
        {
            return Ok(());
        }
        match self.failing {
            Failing::Not => Ok(()),
            Failing::Misrouted => Err(eyre::Report::new(workforce_vellev::Unusable::Misrouted {
                tenant: "code".into(),
                node: "http://code-node:18080".into(),
            })),
            Failing::Otherwise => Err(eyre!("the node went away")),
        }
    }
}

#[async_trait]
impl CodeVectorStore for Counting {
    fn backend(&self) -> &'static str {
        self.call();
        VectorKeys::VELLEV
    }
    async fn ensure_collections(&self) -> Result<()> {
        self.call();
        Ok(())
    }
    async fn health_check(&self) -> Result<bool> {
        self.call();
        Ok(true)
    }
    fn base_url(&self) -> &str {
        self.call();
        "http://code-node:18080"
    }
    fn set_insert_error_sink(&self, _sink: Arc<dyn InsertErrorSink>) {
        self.call();
    }
    async fn index_entities(&self, entities: &[VectorCodeEntity]) -> Result<usize> {
        self.change();
        Ok(entities.len())
    }
    async fn search_entities(
        &self,
        _query: &str,
        _repo_id: Option<&str>,
        _kind: Option<EntityKind>,
        _limit: usize,
    ) -> Result<Vec<VectorSearchResult>> {
        if self.tracked {
            self.call();
            return Ok(Vec::new());
        }
        self.asked().map(|()| Vec::new())
    }
    async fn delete_repo_entities(&self, _repo_id: &str) -> Result<DeleteOutcome> {
        self.change();
        Ok(DeleteOutcome::default())
    }
    async fn delete_entities_by_file(&self, _: &str, _: &str) -> Result<DeleteOutcome> {
        self.purge();
        Ok(DeleteOutcome::default())
    }
    async fn index_summaries(&self, summaries: &[VectorCodeSummary]) -> Result<usize> {
        self.change();
        Ok(summaries.len())
    }
    async fn delete_repo_summaries(&self, _repo_id: &str) -> Result<DeleteOutcome> {
        self.change();
        Ok(DeleteOutcome::default())
    }
    async fn delete_summaries_by_file(&self, _: &str, _: &str) -> Result<DeleteOutcome> {
        self.purge();
        Ok(DeleteOutcome::default())
    }
    async fn entity_count(&self, _repo_id: &str) -> Result<usize> {
        self.call();
        Ok(0)
    }
    fn tracks_completeness(&self) -> bool {
        self.call();
        self.tracked
    }
    async fn identity(&self, _afresh: bool) -> Result<String> {
        self.asked().map(|()| RECORDED.to_string())
    }
}
