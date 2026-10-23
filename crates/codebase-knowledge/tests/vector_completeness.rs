//! A repo's vectors in a store that keeps track of them: filled from the
//! graph a page at a time, resumably, with nothing deleted first; put
//! right file by file when an index run's write fails; and relied on only
//! once they are complete in the collection the store has. With a vector
//! store kept in memory that fails, and changes the graph under a page,
//! as told.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use eyre::{eyre, Result};
use workforce_codebase_knowledge::graph::entities::{EntityId, EntityKind};
use workforce_codebase_knowledge::graph::vectors::FillStage;
use workforce_codebase_knowledge::graph::CodeGraph;
use workforce_codebase_knowledge::indexer::crawler::CrawlerConfig;
use workforce_codebase_knowledge::indexer::fill::{VectorFiller, Verified, Verifying};
use workforce_codebase_knowledge::indexer::summary_generator::{SummaryGenerator, SummaryLlm};
use workforce_codebase_knowledge::indexer::{IndexingConfig, IndexingPipeline, RunOptions};
use workforce_codebase_knowledge::query::natural_language::QueryPlanner;
use workforce_codebase_knowledge::query::{QueryEngine, VectorWait};
use workforce_codebase_knowledge::vector::{
    health::InsertErrorSink, CodeVectorStore, DeleteOutcome, Differing, VectorCodeEntity,
    VectorCodeSummary, VectorSearchResult,
};

const REPO: &str = "repo";
const QUESTION: &str = "how is `parse_config` used";

/// What a [`Held`] store does to the graph, as an index run would, as a
/// write of entities comes in and before it lands.
enum Meanwhile {
    /// A file's entities leave the graph, and its vectors the store.
    FileDeleted(&'static str),
    /// The collection is lost with all it holds.
    Lost,
}

/// Vectors kept in memory, each under what a real store makes its id from.
/// It counts what it is asked to embed and to purge, fails what is
/// switched on, and does a [`Meanwhile`] once.
#[derive(Default)]
struct Held {
    entities: Mutex<BTreeMap<String, VectorCodeEntity>>,
    summaries: Mutex<BTreeMap<String, VectorCodeSummary>>,
    /// How many times the collection was made.
    made: AtomicUsize,
    /// Entities and summaries embedded, in all.
    embedded: AtomicUsize,
    /// Whole-repo purges asked for.
    repo_purges: AtomicUsize,
    fail_index: AtomicBool,
    fail_file_purge: AtomicBool,
    /// A search is accepted and never answered.
    hang_search: AtomicBool,
    /// Keeps no track of what it holds of a repo, as Weaviate's store.
    untracked: AtomicBool,
    /// What a search finds.
    found: Mutex<Vec<VectorSearchResult>>,
    /// Pages of what is held that were listed.
    listed: AtomicUsize,
    /// Writes of entities that succeed before every later one fails.
    writes_left: Mutex<Option<usize>>,
    meanwhile: Mutex<Option<Meanwhile>>,
    /// Entities that come into the graph as the next write of entities
    /// comes in: an index run's, between a page's two reads.
    crowd: Mutex<Vec<workforce_codebase_knowledge::graph::entities::CodeEntity>>,
    graph: Mutex<Option<Arc<CodeGraph>>>,
    /// Tell which collection is held from what was last looked at afresh,
    /// as a real store does between its looks.
    remembering: AtomicBool,
    remembered: AtomicUsize,
    /// A write of entities waits here for leave to go on, if set: one
    /// permit lets one write through.
    gate: Mutex<Option<Arc<tokio::sync::Semaphore>>>,
    /// As many writes of entities as this has permits are with the store
    /// at once, if set, as with an embedder that takes so many requests:
    /// the others wait their turn before they come to the gate.
    room: Mutex<Option<Arc<tokio::sync::Semaphore>>>,
    /// Writes of entities that have had their turn.
    entered: AtomicUsize,
    /// Writes of entities come in and not yet gone, however they go:
    /// stored, failed, or dropped where they wait.
    writing: Arc<AtomicUsize>,
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

fn entity_key(entity: &VectorCodeEntity) -> String {
    format!(
        "{}|{}|{}|{}|{}",
        entity.repo_id,
        entity.entity_id,
        entity.qualified_name,
        entity.signature,
        entity.start_line
    )
}

fn summary_key(summary: &VectorCodeSummary) -> String {
    format!(
        "{}|{}|{}",
        summary.repo_id, summary.granularity, summary.entity_id
    )
}

impl Held {
    /// The graph ids of the entities held.
    fn ids(&self) -> Vec<String> {
        let held = self.entities.lock().unwrap();
        let mut ids: Vec<String> = held.values().map(|e| e.entity_id.clone()).collect();
        ids.sort();
        ids
    }

    fn files(&self) -> Vec<String> {
        let held = self.entities.lock().unwrap();
        let mut files: Vec<String> = held.values().map(|e| e.file_path.clone()).collect();
        files.sort();
        files.dedup();
        files
    }

    fn generations(&self) -> Vec<String> {
        let held = self.entities.lock().unwrap();
        let mut generations: Vec<String> = held.values().map(|e| e.generation.clone()).collect();
        generations.sort();
        generations.dedup();
        generations
    }

    fn embedded(&self) -> usize {
        self.embedded.load(Ordering::SeqCst)
    }

    /// Lose the collection with all it holds, as a restored backup that
    /// left it out does: made again, it is another.
    fn lose(&self) {
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

    /// Hold a vector of an entity the graph does not have.
    fn hold_stale(&self, id: &str) {
        let stale = VectorCodeEntity {
            entity_id: id.to_string(),
            repo_id: REPO.to_string(),
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
        self.entities
            .lock()
            .unwrap()
            .insert(entity_key(&stale), stale);
    }
}

#[async_trait]
impl CodeVectorStore for Held {
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
        let crowd = std::mem::take(&mut *self.crowd.lock().unwrap());
        if !crowd.is_empty() {
            let graph = self.graph.lock().unwrap().clone().expect("a graph");
            graph.upsert_entities(&crowd).await?;
        }
        let meanwhile = self.meanwhile.lock().unwrap().take();
        match meanwhile {
            Some(Meanwhile::FileDeleted(file)) => {
                let graph = self.graph.lock().unwrap().clone().expect("a graph");
                graph.delete_file_entities(REPO, file).await?;
                let mut held = self.entities.lock().unwrap();
                held.retain(|_, entity| entity.file_path != file);
            }
            Some(Meanwhile::Lost) => self.lose(),
            None => {}
        }
        let mut held = self.entities.lock().unwrap();
        for entity in entities {
            held.insert(entity_key(entity), entity.clone());
        }
        self.embedded.fetch_add(entities.len(), Ordering::SeqCst);
        Ok(entities.len())
    }

    async fn search_entities(
        &self,
        _query: &str,
        _repo_id: Option<&str>,
        _kind: Option<EntityKind>,
        _limit: usize,
    ) -> Result<Vec<VectorSearchResult>> {
        if self.hang_search.load(Ordering::SeqCst) {
            std::future::pending::<()>().await;
        }
        Ok(self.found.lock().unwrap().clone())
    }

    async fn differing_entities(
        &self,
        _repo_id: &str,
        entities: &[VectorCodeEntity],
    ) -> Result<Differing> {
        let held = self.entities.lock().unwrap();
        let of = |wanted: &VectorCodeEntity| -> Vec<&VectorCodeEntity> {
            let all = held.values();
            all.filter(|held| held.entity_id == wanted.entity_id)
                .collect()
        };
        let differs = |wanted: &VectorCodeEntity| {
            let of_it = of(wanted);
            of_it.len() != 1 || of_it[0].record_hash() != wanted.record_hash()
        };
        let all = entities.iter().enumerate();
        Ok(Differing {
            places: all.filter(|(_, e)| differs(e)).map(|(at, _)| at).collect(),
            held: entities.iter().map(|wanted| of(wanted).len()).sum(),
        })
    }

    async fn differing_summaries(
        &self,
        _repo_id: &str,
        summaries: &[VectorCodeSummary],
    ) -> Result<Differing> {
        let held = self.summaries.lock().unwrap();
        let of = |wanted: &VectorCodeSummary| -> Vec<&VectorCodeSummary> {
            let all = held.values();
            all.filter(|held| held.key() == wanted.key()).collect()
        };
        let differs = |wanted: &VectorCodeSummary| {
            let of_it = of(wanted);
            of_it.len() != 1 || of_it[0].record_hash() != wanted.record_hash()
        };
        let all = summaries.iter().enumerate();
        Ok(Differing {
            places: all.filter(|(_, s)| differs(s)).map(|(at, _)| at).collect(),
            held: summaries.iter().map(|wanted| of(wanted).len()).sum(),
        })
    }

    async fn held_after(
        &self,
        _repo_id: &str,
        summaries: bool,
        after: Option<String>,
        limit: usize,
    ) -> Result<(Vec<(String, String)>, Option<String>)> {
        self.listed.fetch_add(1, Ordering::SeqCst);
        let all: Vec<(String, (String, String))> = if summaries {
            let held = self.summaries.lock().unwrap();
            held.iter()
                .map(|(at, s)| (at.clone(), (s.key(), s.file_path.clone())))
                .collect()
        } else {
            let held = self.entities.lock().unwrap();
            held.iter()
                .map(|(at, e)| (at.clone(), (e.entity_id.clone(), e.file_path.clone())))
                .collect()
        };
        let rest: Vec<_> = all
            .into_iter()
            .filter(|(at, _)| after.as_ref().is_none_or(|after| at > after))
            .collect();
        let next = (rest.len() > limit).then(|| rest[limit - 1].0.clone());
        let page = rest.into_iter().take(limit).map(|(_, held)| held).collect();
        Ok((page, next))
    }

    async fn delete_repo_entities(&self, _repo_id: &str) -> Result<DeleteOutcome> {
        self.repo_purges.fetch_add(1, Ordering::SeqCst);
        self.entities.lock().unwrap().clear();
        Ok(DeleteOutcome::default())
    }

    async fn delete_entities_by_file(
        &self,
        _repo_id: &str,
        file_path: &str,
    ) -> Result<DeleteOutcome> {
        self.refuse(&self.fail_file_purge, "purge the file")?;
        let mut held = self.entities.lock().unwrap();
        held.retain(|_, entity| entity.file_path != file_path);
        Ok(DeleteOutcome::default())
    }

    async fn index_summaries(&self, summaries: &[VectorCodeSummary]) -> Result<usize> {
        self.refuse(&self.fail_index, "index")?;
        let mut held = self.summaries.lock().unwrap();
        for summary in summaries {
            held.insert(summary_key(summary), summary.clone());
        }
        self.embedded.fetch_add(summaries.len(), Ordering::SeqCst);
        Ok(summaries.len())
    }

    async fn delete_repo_summaries(&self, _repo_id: &str) -> Result<DeleteOutcome> {
        self.summaries.lock().unwrap().clear();
        Ok(DeleteOutcome::default())
    }

    async fn delete_summaries_by_file(
        &self,
        _repo_id: &str,
        file_path: &str,
    ) -> Result<DeleteOutcome> {
        self.refuse(&self.fail_file_purge, "purge the file")?;
        let mut held = self.summaries.lock().unwrap();
        held.retain(|_, summary| summary.file_path != file_path);
        Ok(DeleteOutcome::default())
    }

    async fn entity_count(&self, _repo_id: &str) -> Result<usize> {
        Ok(self.entities.lock().unwrap().len())
    }

    async fn summary_count(&self, _repo_id: &str) -> Result<usize> {
        Ok(self.summaries.lock().unwrap().len())
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

    async fn sweep(&self, _repo_id: &str, generation: &str) -> Result<usize> {
        let mut entities = self.entities.lock().unwrap();
        let mut summaries = self.summaries.lock().unwrap();
        let before = entities.len() + summaries.len();
        entities.retain(|_, entity| entity.generation == generation);
        summaries.retain(|_, summary| summary.generation == generation);
        Ok(before - entities.len() - summaries.len())
    }
}

/// A store that keeps no track, as Weaviate's: another store than `Held`.
struct Untracked;

#[async_trait]
impl CodeVectorStore for Untracked {
    async fn ensure_collections(&self) -> Result<()> {
        Ok(())
    }
    async fn health_check(&self) -> Result<bool> {
        Ok(true)
    }
    fn base_url(&self) -> &str {
        "http://weaviate"
    }
    fn set_insert_error_sink(&self, _sink: Arc<dyn InsertErrorSink>) {}
    async fn index_entities(&self, entities: &[VectorCodeEntity]) -> Result<usize> {
        Ok(entities.len())
    }
    async fn search_entities(
        &self,
        _query: &str,
        _repo_id: Option<&str>,
        _kind: Option<EntityKind>,
        _limit: usize,
    ) -> Result<Vec<VectorSearchResult>> {
        Ok(Vec::new())
    }
    async fn delete_repo_entities(&self, _repo_id: &str) -> Result<DeleteOutcome> {
        Ok(DeleteOutcome::default())
    }
    async fn delete_entities_by_file(&self, _: &str, _: &str) -> Result<DeleteOutcome> {
        Ok(DeleteOutcome::default())
    }
    async fn index_summaries(&self, summaries: &[VectorCodeSummary]) -> Result<usize> {
        Ok(summaries.len())
    }
    async fn delete_repo_summaries(&self, _repo_id: &str) -> Result<DeleteOutcome> {
        Ok(DeleteOutcome::default())
    }
    async fn delete_summaries_by_file(&self, _: &str, _: &str) -> Result<DeleteOutcome> {
        Ok(DeleteOutcome::default())
    }
    async fn entity_count(&self, _repo_id: &str) -> Result<usize> {
        Ok(0)
    }
}

fn write(root: &Path, file: &str, source: &str) {
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src").join(file), source).unwrap();
}

fn git(root: &Path, args: &[&str]) {
    let done = std::process::Command::new("git")
        .args(["-c", "user.name=test", "-c", "user.email=test@example.com"])
        .args(args)
        .current_dir(root)
        .output()
        .unwrap();
    assert!(done.status.success(), "git {args:?}: {done:?}");
}

/// Commit the work tree: an incremental index goes by what changed between
/// the commit last indexed and `HEAD`.
fn commit(root: &Path) {
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "change"]);
}

const CONFIG_RS: &str =
    "/// Reads the configuration.\npub fn parse_config(path: &str) -> String {\n    path.to_string()\n}\n";
const MAIL_RS: &str =
    "/// Sends a message.\npub fn send_email(to: &str) -> bool {\n    !to.is_empty()\n}\n";

/// A source file of `functions` functions.
fn many(functions: usize) -> String {
    (0..functions)
        .map(|n| format!("/// Does thing {n}.\npub fn thing_{n}() -> usize {{\n    {n}\n}}\n\n"))
        .collect()
}

/// A model that summarises anything in a word.
struct Terse;

#[async_trait]
impl SummaryLlm for Terse {
    async fn generate_text(&self, _prompt: &str, _max_tokens: usize) -> Result<String> {
        Ok("Does what it says.".to_string())
    }

    fn model_name(&self) -> &str {
        "terse"
    }
}

/// Step the fill of the repo until it is at `stage`, and say in how many
/// steps: within more than any fill here takes, so that one that never
/// gets there fails and does not go on for ever.
async fn stepped_to(filler: &VectorFiller, stage: FillStage) -> usize {
    for steps in 1..=1000 {
        if filler.step(REPO).await.unwrap().stage == stage {
            return steps;
        }
    }
    panic!("the fill did not come to {stage:?}");
}

struct Repo {
    dir: tempfile::TempDir,
    graph: Arc<CodeGraph>,
    held: Arc<Held>,
}

impl Repo {
    fn new() -> Self {
        Self::of(12)
    }

    /// A repo of two small files and one of `functions` functions.
    fn of(functions: usize) -> Self {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "config.rs", CONFIG_RS);
        write(dir.path(), "mail.rs", MAIL_RS);
        write(dir.path(), "many.rs", &many(functions));
        git(dir.path(), &["init", "-q"]);
        commit(dir.path());
        let graph = Arc::new(CodeGraph::open_in_memory().unwrap());
        let held = Arc::new(Held::default());
        *held.graph.lock().unwrap() = Some(Arc::clone(&graph));
        Self { dir, graph, held }
    }

    /// A repo indexed in full, its vectors filled.
    async fn filled() -> Self {
        let repo = Self::new();
        repo.index().await;
        repo.fill().await;
        assert!(repo.complete().await && repo.agrees().await);
        repo
    }

    fn config(&self) -> IndexingConfig {
        IndexingConfig {
            root: self.dir.path().to_path_buf(),
            repo_id: REPO.to_string(),
            repo_name: REPO.to_string(),
            crawler: CrawlerConfig {
                root: self.dir.path().to_path_buf(),
                ..CrawlerConfig::default()
            },
            parse_concurrency: 2,
            summary_concurrency: 2,
            generate_summaries: false,
        }
    }

    fn store(&self) -> Arc<dyn CodeVectorStore> {
        Arc::clone(&self.held) as Arc<dyn CodeVectorStore>
    }

    /// The pipeline writing vectors to the held store.
    fn pipeline(&self) -> IndexingPipeline {
        IndexingPipeline::with_vector_store(self.config(), Arc::clone(&self.graph), self.store())
    }

    /// The pipeline, summarising what it indexes.
    fn summarising(&self) -> IndexingPipeline {
        self.pipeline()
            .with_summary_generator(SummaryGenerator::new(Box::new(Terse), 64))
    }

    /// Compare the store with the graph to the end.
    async fn compare(&self) -> Verified {
        let filler = self.filler();
        let mut at = Verifying::default();
        for _ in 0..1000 {
            match filler.verify_step(REPO, &mut at).await.unwrap() {
                Verified::Going => {}
                ended => return ended,
            }
        }
        panic!("the comparison did not end");
    }

    /// Whether the store holds every summary the graph has, as it has it.
    async fn summaries_agree(&self) -> bool {
        let confirmed = self.filler().confirm(REPO).await.unwrap();
        confirmed.summaries_expected > 0 && confirmed.agrees()
    }

    /// A filler of the held store, three to a page.
    fn filler(&self) -> VectorFiller {
        VectorFiller::new(Arc::clone(&self.graph), self.store()).paged(3)
    }

    async fn index(&self) {
        self.pipeline()
            .run_full_index(RunOptions::default())
            .await
            .unwrap();
    }

    /// Fill to the end; returns how many pages it took.
    async fn fill(&self) -> usize {
        let filler = self.filler();
        for pages in 1..1000 {
            if filler.step(REPO).await.unwrap().stage == FillStage::Done {
                return pages;
            }
        }
        panic!("the fill did not end");
    }

    async fn complete(&self) -> bool {
        self.filler().complete(REPO, true).await.unwrap()
    }

    async fn graph_ids(&self) -> Vec<String> {
        let entities = self.graph.all_entities(REPO).await.unwrap();
        let mut ids: Vec<String> = entities
            .iter()
            .map(|entity| entity.id.as_str().to_string())
            .collect();
        ids.sort();
        ids
    }

    /// Whether the store holds exactly the graph's entities.
    async fn agrees(&self) -> bool {
        self.held.ids() == self.graph_ids().await
    }

    async fn pending(&self) -> Vec<String> {
        let pending = self.graph.vectors_pending(REPO, 1000).await.unwrap();
        pending.into_iter().map(|(file, _)| file).collect()
    }

    /// Add a function to `mail.rs`, committed.
    fn change_mail(&self) {
        write(
            self.dir.path(),
            "mail.rs",
            &format!("{MAIL_RS}\npub fn bounce() {{}}\n"),
        );
        commit(self.dir.path());
    }

    /// Run an incremental index that is one: it indexed `files` files, not
    /// the whole repo again as it does when it cannot tell what changed.
    async fn incremental(&self, pipeline: &IndexingPipeline, files: usize) {
        let indexed = pipeline
            .run_incremental_index(RunOptions::default())
            .await
            .unwrap();
        assert_eq!(indexed.files_indexed, files, "{indexed:?}");
    }

    /// Whether a question of the repo is scored with its vectors.
    async fn scored_by_vectors(&self) -> bool {
        let engine = QueryEngine::with_vector_store(Arc::clone(&self.graph), self.store());
        let plan = QueryPlanner::plan(QUESTION, false);
        engine
            .resolve_seed_entities_multi(&[REPO.to_string()], QUESTION, &plan)
            .await
            .unwrap()
            .vector_configured
    }
}

#[tokio::test]
async fn a_repo_never_indexed_has_no_complete_vectors() {
    let repo = Repo::new();
    assert!(!repo.complete().await);
    assert!(!repo.scored_by_vectors().await);
}

/// A full index stores no vectors in a store that keeps track: the fill
/// does, after it, a page at a time, and only then are they relied on.
#[tokio::test]
async fn a_full_index_leaves_the_vectors_to_a_fill_from_the_graph() {
    let repo = Repo::new();
    repo.index().await;
    assert_eq!(repo.held.embedded(), 0, "nothing embedded by the index run");
    assert!(!repo.complete().await && !repo.scored_by_vectors().await);

    let entities = repo.graph_ids().await.len();
    let pages = repo.fill().await;
    assert!(
        pages >= entities / 3,
        "{pages} pages for {entities} entities"
    );
    assert!(repo.complete().await && repo.scored_by_vectors().await);
    assert!(repo.agrees().await);
    assert_eq!(repo.held.embedded(), entities, "each embedded once");
    let fill = repo.graph.vector_fill(REPO).await.unwrap().unwrap();
    assert_eq!(fill.stage, FillStage::Done);
    assert_eq!((fill.done, fill.total), (entities as u64, entities as u64));
    assert_eq!(repo.held.generations(), [fill.generation]);
    let confirmed = repo.filler().confirm(REPO).await.unwrap();
    assert!(confirmed.agrees(), "{confirmed:?}");
    assert_eq!(confirmed.entities_stored, entities);
}

/// A fill that fails part of the way keeps its place: gone on with, it
/// stores what is left, and nothing already stored is embedded again or
/// deleted.
#[tokio::test]
async fn a_fill_stopped_part_of_the_way_goes_on_from_where_it_stopped() {
    let repo = Repo::new();
    repo.index().await;
    let entities = repo.graph_ids().await.len();
    let purged = repo.held.repo_purges.load(Ordering::SeqCst);
    *repo.held.writes_left.lock().unwrap() = Some(2);
    let filler = repo.filler();
    filler.step(REPO).await.unwrap();
    filler.step(REPO).await.unwrap();
    let stopped = filler.step(REPO).await.unwrap_err().to_string();
    assert!(stopped.contains("went away"), "{stopped}");
    let kept = repo.graph.vector_fill(REPO).await.unwrap().unwrap();
    assert_eq!((kept.stage, kept.done), (FillStage::Entities, 6));
    assert_eq!(repo.held.embedded(), 6);
    assert!(!repo.complete().await);

    // Another filler, as after a restart.
    *repo.held.writes_left.lock().unwrap() = None;
    repo.fill().await;
    assert!(repo.complete().await && repo.agrees().await);
    assert_eq!(repo.held.embedded(), entities, "none embedded twice");
    assert_eq!(
        repo.held.repo_purges.load(Ordering::SeqCst),
        purged,
        "nothing deleted to fill"
    );
    let ended = repo.graph.vector_fill(REPO).await.unwrap().unwrap();
    assert_eq!(ended.generation, kept.generation, "the same fill");
}

/// What the store held of the repo before a fill stays until the fill
/// ends, and then what the graph no longer has is removed.
#[tokio::test]
async fn a_fill_replaces_what_is_held_and_removes_what_is_stale_at_the_end() {
    let repo = Repo::filled().await;
    repo.held.hold_stale("gone::long_ago");
    repo.graph
        .set_vectors_complete_in(REPO, None)
        .await
        .unwrap();
    let filler = repo.filler();
    filler.step(REPO).await.unwrap();
    assert!(
        repo.held.ids().contains(&"gone::long_ago".to_string()),
        "still held while the fill runs"
    );
    assert!(repo.held.ids().len() > repo.graph_ids().await.len());
    repo.fill().await;
    assert!(repo.agrees().await, "the stale one removed at the end");
    assert_eq!(repo.held.generations().len(), 1);
}

/// A page whose entities leave the graph while it is being stored is put
/// right before the fill moves on: the index run changed the graph first,
/// so the page's second look at the graph sees it.
#[tokio::test]
async fn a_page_changed_under_the_fill_ends_as_the_graph_has_it() {
    let repo = Repo::new();
    repo.index().await;
    *repo.held.meanwhile.lock().unwrap() = Some(Meanwhile::FileDeleted("src/many.rs"));
    repo.fill().await;
    assert!(repo.complete().await);
    assert!(repo.agrees().await, "{:?}", repo.held.files());
    assert!(!repo.held.files().contains(&"src/many.rs".to_string()));
}

/// An incremental index whose vector write fails leaves the files it
/// changed noted as pending, and the repo's vectors complete: they are put
/// right file by file, with no fill of the repo.
#[tokio::test]
async fn a_failed_incremental_write_leaves_its_files_pending_not_the_repo_to_fill() {
    let repo = Repo::filled().await;
    let embedded = repo.held.embedded();
    let fill = repo.graph.vector_fill(REPO).await.unwrap().unwrap();

    repo.held.fail_index.store(true, Ordering::SeqCst);
    repo.change_mail();
    repo.incremental(&repo.pipeline(), 1).await;
    assert_eq!(repo.pending().await, ["src/mail.rs"]);
    assert!(repo.complete().await, "the rest is still relied on");
    assert!(!repo.agrees().await);

    // Still failing: the file stays pending.
    assert!(repo.filler().settle_pending(REPO).await.is_err());
    assert_eq!(repo.pending().await, ["src/mail.rs"]);

    repo.held.fail_index.store(false, Ordering::SeqCst);
    assert_eq!(repo.filler().settle_pending(REPO).await.unwrap(), 0);
    assert!(repo.agrees().await && repo.pending().await.is_empty());
    let in_mail = repo
        .graph
        .find_entities_in_file(REPO, "src/mail.rs")
        .await
        .unwrap()
        .len();
    assert_eq!(
        repo.held.embedded() - embedded,
        in_mail,
        "only the file's entities embedded"
    );
    assert_eq!(
        repo.graph.vector_fill(REPO).await.unwrap().unwrap(),
        fill,
        "no fill begun"
    );
    assert_eq!(
        repo.held.generations(),
        [fill.generation],
        "under the fill's generation"
    );
}

/// A file whose purge fails stays pending too, and a file noted again
/// while it was being put right stays noted.
#[tokio::test]
async fn a_file_that_could_not_be_purged_stays_pending_and_a_note_renewed_survives() {
    let repo = Repo::filled().await;
    repo.held.fail_file_purge.store(true, Ordering::SeqCst);
    std::fs::remove_file(repo.dir.path().join("src/mail.rs")).unwrap();
    commit(repo.dir.path());
    repo.incremental(&repo.pipeline(), 0).await;
    assert_eq!(repo.pending().await, ["src/mail.rs"]);
    assert!(repo.held.files().contains(&"src/mail.rs".to_string()));

    repo.held.fail_file_purge.store(false, Ordering::SeqCst);
    let (_, seq) = repo.graph.vectors_pending(REPO, 10).await.unwrap()[0].clone();
    repo.graph
        .note_vectors_pending(REPO, &["src/mail.rs".to_string()])
        .await
        .unwrap();
    repo.graph
        .clear_vectors_pending(REPO, "src/mail.rs", seq)
        .await
        .unwrap();
    assert_eq!(repo.pending().await, ["src/mail.rs"], "noted again since");

    assert_eq!(repo.filler().settle_pending(REPO).await.unwrap(), 0);
    assert!(repo.agrees().await);
}

/// An incremental index whose writes all land leaves nothing pending, and
/// what it writes carries the repo's generation, so the next fill's end
/// spares it.
#[tokio::test]
async fn an_incremental_index_that_stores_its_vectors_leaves_nothing_pending() {
    let repo = Repo::filled().await;
    repo.change_mail();
    repo.incremental(&repo.pipeline(), 1).await;
    assert!(repo.pending().await.is_empty());
    assert!(repo.complete().await && repo.agrees().await);
    assert_eq!(repo.held.generations().len(), 1);
}

/// A renamed file's vectors are dropped at its old path and stored again at
/// its new one.
#[tokio::test]
async fn a_renamed_files_vectors_follow_it() {
    let repo = Repo::filled().await;
    git(repo.dir.path(), &["mv", "src/mail.rs", "src/post.rs"]);
    commit(repo.dir.path());
    repo.incremental(&repo.pipeline(), 0).await;
    assert!(repo.complete().await && repo.agrees().await);
    assert!(repo.pending().await.is_empty());
    let files = repo.held.files();
    assert!(
        files.contains(&"src/post.rs".to_string()) && !files.contains(&"src/mail.rs".to_string()),
        "{files:?}"
    );
}

/// An incremental index that writes to another store, or to none, as a
/// hub moved back to another vector store does, or one still on the old
/// store while the new is seeded: the files it changes are noted for the
/// tracked store, whose vectors stay complete but for them, and are put
/// right there file by file. A full index elsewhere rebuilds the graph,
/// and the tracked store is then to be filled again.
#[tokio::test]
async fn an_index_run_on_another_store_notes_its_files_for_the_tracked_one() {
    for elsewhere in [None, Some(Arc::new(Untracked) as Arc<dyn CodeVectorStore>)] {
        let repo = Repo::filled().await;
        let embedded = repo.held.embedded();
        let other = |repo: &Repo| match &elsewhere {
            Some(store) => IndexingPipeline::with_vector_store(
                repo.config(),
                Arc::clone(&repo.graph),
                Arc::clone(store),
            ),
            None => IndexingPipeline::new(repo.config(), Arc::clone(&repo.graph)),
        };
        repo.change_mail();
        repo.incremental(&other(&repo), 1).await;
        std::fs::remove_file(repo.dir.path().join("src/config.rs")).unwrap();
        commit(repo.dir.path());
        repo.incremental(&other(&repo), 0).await;
        assert_eq!(repo.pending().await, ["src/config.rs", "src/mail.rs"]);
        assert!(repo.complete().await, "but for the files noted");
        assert!(!repo.agrees().await);
        assert_eq!(
            repo.held.embedded(),
            embedded,
            "nothing written to the tracked store"
        );

        // Back on the tracked store: only those files are put right.
        assert_eq!(repo.filler().settle_pending(REPO).await.unwrap(), 0);
        assert!(repo.agrees().await);
        let in_mail = repo
            .graph
            .find_entities_in_file(REPO, "src/mail.rs")
            .await
            .unwrap()
            .len();
        assert_eq!(repo.held.embedded() - embedded, in_mail);

        other(&repo)
            .run_full_index(RunOptions::default())
            .await
            .unwrap();
        assert!(!repo.complete().await, "a full index elsewhere");
        assert_eq!(repo.graph.vector_fill(REPO).await.unwrap(), None);
    }
}

/// A store that keeps no track of a repo no store keeps track of notes
/// nothing: there is nobody to put it right.
#[tokio::test]
async fn nothing_is_noted_where_no_store_keeps_track() {
    let repo = Repo::new();
    let untracked = || {
        IndexingPipeline::with_vector_store(
            repo.config(),
            Arc::clone(&repo.graph),
            Arc::new(Untracked),
        )
    };
    untracked()
        .run_full_index(RunOptions::default())
        .await
        .unwrap();
    repo.change_mail();
    repo.incremental(&untracked(), 1).await;
    assert!(repo.pending().await.is_empty());
}

/// The collection lost behind the indexer's back, as one a restored backup
/// left out is: what the graph records was complete in a collection that
/// is no more. The repo is not scored by vectors until a fill has filled
/// the new one.
#[tokio::test]
async fn vectors_complete_in_a_collection_since_lost_are_not_relied_on() {
    let repo = Repo::filled().await;
    let recorded = repo.graph.vectors_complete_in(REPO).await.unwrap();
    assert!(recorded.is_some() && repo.scored_by_vectors().await);

    repo.held.lose();
    assert_eq!(
        repo.graph.vectors_complete_in(REPO).await.unwrap(),
        recorded
    );
    assert!(!repo.complete().await);
    assert!(!repo.scored_by_vectors().await);

    repo.fill().await;
    assert!(repo.complete().await && repo.scored_by_vectors().await);
    assert!(repo.agrees().await);
}

/// A collection lost while a fill was writing to it holds only what was
/// written after. The fill finds its collection another, at the latest
/// when it ends, where it looks afresh; it records nothing complete of the
/// part, and starts again.
#[tokio::test]
async fn a_collection_lost_during_a_fill_is_not_recorded_complete() {
    for seen_at_once in [true, false] {
        let repo = Repo::new();
        repo.index().await;
        // A real store reads which collection it has only now and then.
        repo.held.remembering.store(!seen_at_once, Ordering::SeqCst);
        let filler = repo.filler();
        let first = filler.step(REPO).await.unwrap();
        *repo.held.meanwhile.lock().unwrap() = Some(Meanwhile::Lost);
        let mut found_out = None;
        for _ in 0..100 {
            match filler.step(REPO).await {
                Ok(fill) if fill.generation != first.generation => {
                    found_out = Some("begun again".to_string());
                    break;
                }
                Ok(fill) => assert_ne!(fill.stage, FillStage::Done, "not on part of the repo"),
                Err(e) => {
                    found_out = Some(e.to_string());
                    break;
                }
            }
        }
        let found_out = found_out.expect("found out");
        if seen_at_once {
            assert_eq!(found_out, "begun again");
        } else {
            assert!(found_out.contains("made again"), "{found_out}");
            assert_eq!(repo.graph.vector_fill(REPO).await.unwrap(), None);
        }
        assert!(!repo.complete().await);

        repo.fill().await;
        assert!(repo.complete().await && repo.agrees().await);
    }
}

/// Summaries are filled with the entities, counted, and put right with
/// their file.
#[tokio::test]
async fn summaries_are_filled_and_put_right_with_the_entities() {
    let repo = Repo::new();
    repo.index().await;
    let in_mail = repo
        .graph
        .find_entities_in_file(REPO, "src/mail.rs")
        .await
        .unwrap();
    let summarised: &EntityId = &in_mail[0].id;
    for entity in repo.graph.all_entities(REPO).await.unwrap().iter().take(5) {
        repo.graph
            .upsert_summary(&entity.id, "function", "Does a thing.", "hash", "model")
            .await
            .unwrap();
    }
    repo.graph
        .upsert_summary(summarised, "file", "Sends mail.", "hash", "model")
        .await
        .unwrap();
    let summaries = repo.graph.count_summaries_present(REPO).await.unwrap() as usize;
    repo.fill().await;
    assert_eq!(repo.held.summaries.lock().unwrap().len(), summaries);
    let confirmed = repo.filler().confirm(REPO).await.unwrap();
    assert!(confirmed.agrees(), "{confirmed:?}");
    assert_eq!(confirmed.summaries_stored, summaries);
    let fill = repo.graph.vector_fill(REPO).await.unwrap().unwrap();
    assert_eq!(
        fill.total,
        (repo.graph_ids().await.len() + summaries) as u64
    );

    // A summary changed, its file noted: the file's summaries are stored
    // again as the graph has them.
    repo.graph
        .upsert_summary(
            summarised,
            "file",
            "Sends mail, and bounces.",
            "hash2",
            "model",
        )
        .await
        .unwrap();
    repo.graph
        .note_vectors_pending(REPO, &["src/mail.rs".to_string()])
        .await
        .unwrap();
    repo.filler().settle_pending(REPO).await.unwrap();
    let held = repo.held.summaries.lock().unwrap();
    assert!(held
        .values()
        .any(|summary| summary.summary_text == "Sends mail, and bounces."));
    assert_eq!(held.len(), summaries);
    // Written as the fill's own, entities and summaries alike: a fill
    // that swept at its end would otherwise take them for stale.
    assert!(held.values().all(|s| s.generation == fill.generation));
    let entities = repo.held.entities.lock().unwrap();
    assert!(entities.values().all(|e| e.generation == fill.generation));
}

/// Two workers filling one repo each move the recorded place only from
/// where they found it: the one that lost the race takes up from where
/// the other got to, and the repo is counted and recorded once.
#[tokio::test]
async fn two_workers_filling_one_repo_do_not_undo_each_other() {
    let repo = Repo::new();
    repo.index().await;
    let entities = repo.graph_ids().await.len();
    let worker = || async {
        let filler = repo.filler();
        for pages in 1..1000 {
            // Give way, as a worker waiting on a real store does.
            tokio::task::yield_now().await;
            if filler.step(REPO).await.unwrap().stage == FillStage::Done {
                return pages;
            }
        }
        panic!("the fill did not end");
    };
    let (one, other) = tokio::join!(worker(), worker());
    assert!(
        one > 1 && other > 1,
        "both took part: {one} and {other} pages"
    );
    assert!(repo.complete().await && repo.agrees().await);
    assert_eq!(
        repo.filler().step(REPO).await.unwrap().stage,
        FillStage::Done,
        "a worker that comes after begins no fill of its own"
    );
    let fill = repo.graph.vector_fill(REPO).await.unwrap().unwrap();
    assert_eq!(fill.done, entities as u64, "counted once");
    assert_eq!(repo.held.generations(), [fill.generation]);
    // Each may store a page the other was also on, since a page is stored
    // before the place is moved; neither stores what the other recorded.
    let embedded = repo.held.embedded();
    assert!(
        (entities..=2 * entities).contains(&embedded),
        "{embedded} embedded, of {entities}"
    );
}

/// While a repo's vectors are incomplete an answer is scored as with no
/// vector store, not as with a store that found nothing.
#[tokio::test]
async fn an_answer_is_scored_as_with_no_store_while_the_vectors_are_incomplete() {
    let repo = Repo::filled().await;
    let with_store = QueryEngine::with_vector_store(Arc::clone(&repo.graph), repo.store());
    let storeless = QueryEngine::new(Arc::clone(&repo.graph));
    let graph_only = storeless.ask(REPO, QUESTION, None, false).await.unwrap();
    let complete = with_store.ask(REPO, QUESTION, None, false).await.unwrap();
    assert!(
        complete.confidence < graph_only.confidence,
        "a complete store that finds nothing counts against the answer"
    );

    repo.graph
        .set_vectors_complete_in(REPO, None)
        .await
        .unwrap();
    let incomplete = with_store.ask(REPO, QUESTION, None, false).await.unwrap();
    assert_eq!(incomplete.confidence, graph_only.confidence);
}

/// Summaries made while a fill is under way reach the store, though the
/// fill has passed where they are in the graph: the repo is not recorded
/// complete without them.
#[tokio::test]
async fn summaries_made_while_a_fill_is_under_way_are_stored() {
    let repo = Repo::new();
    repo.index().await;
    let filler = repo.filler();
    // Past the summaries, of which there were none: only the sweep is left.
    stepped_to(&filler, FillStage::Sweep).await;
    assert!(!repo.complete().await);

    let made = repo
        .summarising()
        .run_summaries_only(RunOptions::default())
        .await
        .unwrap();
    assert!(made.summaries_generated > 0, "{made:?}");
    repo.fill().await;
    repo.filler().settle_pending(REPO).await.unwrap();
    assert!(repo.complete().await);
    assert!(
        repo.summaries_agree().await,
        "{:?}",
        repo.filler().confirm(REPO).await.unwrap()
    );
    // Of the fill's generation, so that its sweep left them.
    let fill = repo.graph.vector_fill(REPO).await.unwrap().unwrap();
    let held = repo.held.summaries.lock().unwrap();
    assert!(held.values().all(|s| s.generation == fill.generation));
}

/// Before any fill has begun, summaries wait for it: it reads the graph
/// after they are in it, and stores each once.
#[tokio::test]
async fn summaries_made_before_any_fill_are_left_to_it() {
    let repo = Repo::new();
    repo.index().await;
    let made = repo
        .summarising()
        .run_summaries_only(RunOptions::default())
        .await
        .unwrap();
    assert_eq!(repo.held.embedded(), 0, "nothing embedded by the index run");
    assert!(repo.pending().await.is_empty());
    repo.fill().await;
    assert!(repo.summaries_agree().await);
    assert_eq!(
        repo.held.summaries.lock().unwrap().len(),
        made.summaries_generated
    );
}

/// Summaries the store refuses leave their files pending, whether the
/// repo's vectors are complete or being filled: put right, the store has
/// them.
#[tokio::test]
async fn summaries_the_store_refuses_leave_their_files_pending() {
    for filling in [false, true] {
        let repo = Repo::new();
        repo.index().await;
        match filling {
            true => {
                stepped_to(&repo.filler(), FillStage::Sweep).await;
            }
            false => {
                repo.fill().await;
            }
        }
        repo.held.fail_index.store(true, Ordering::SeqCst);
        let made = repo
            .summarising()
            .run_summaries_only(RunOptions::default())
            .await
            .unwrap();
        assert!(made.summaries_generated > 0);
        assert!(repo.held.summaries.lock().unwrap().is_empty());
        let pending = repo.pending().await;
        assert!(
            pending.contains(&"src/mail.rs".to_string()) && pending.len() == 3,
            "filling: {filling}, pending: {pending:?}"
        );

        repo.held.fail_index.store(false, Ordering::SeqCst);
        repo.fill().await;
        assert_eq!(repo.filler().settle_pending(REPO).await.unwrap(), 0);
        assert!(repo.complete().await && repo.summaries_agree().await);
    }
}

/// A full index of a repo whose vectors are whole purges none of them:
/// they go on being searched, as the repo was, while a fill begun by the
/// index run writes over them and removes at its end what is no longer
/// in the graph.
#[tokio::test]
async fn a_full_index_keeps_the_vectors_it_found_until_the_fill_replaces_them() {
    let repo = Repo::filled().await;
    let before = repo.held.ids();
    let first = repo.graph.vector_fill(REPO).await.unwrap().unwrap();
    std::fs::remove_file(repo.dir.path().join("src/config.rs")).unwrap();
    repo.change_mail();

    repo.index().await;
    assert_eq!(repo.held.repo_purges.load(Ordering::SeqCst), 0);
    assert_eq!(repo.held.ids(), before, "held as before the index run");
    assert!(repo.complete().await, "and relied on still");
    assert!(repo.scored_by_vectors().await);
    let begun = repo.graph.vector_fill(REPO).await.unwrap().unwrap();
    assert_eq!(begun.stage, FillStage::Entities);
    assert_ne!(begun.generation, first.generation, "a fill of its own");

    // Relied on all the way through the fill, which ends with the store
    // as the graph is.
    let filler = repo.filler();
    for step in 0.. {
        assert!(step < 1000, "the fill did not end");
        if filler.step(REPO).await.unwrap().stage == FillStage::Done {
            break;
        }
        assert!(repo.complete().await);
    }
    assert!(repo.agrees().await);
    assert!(!repo.held.files().contains(&"src/config.rs".to_string()));
    assert_eq!(repo.held.generations(), [begun.generation]);

    // A repo whose vectors were not whole is not made out to be.
    let partial = Repo::new();
    partial.index().await;
    partial.filler().step(REPO).await.unwrap();
    partial.index().await;
    assert!(!partial.complete().await && !partial.scored_by_vectors().await);
    assert!(partial.graph.vector_fill(REPO).await.unwrap().is_none());
}

/// A store that holds a repo as the graph has it is found so without what
/// it holds being listed, and nothing is written. One that has lost a
/// vector, holds another of an earlier text, and holds one the graph has
/// not is found out, and only the files concerned are stored again.
#[tokio::test]
async fn a_store_that_has_drifted_from_the_graph_is_put_right_where_it_differs() {
    let repo = Repo::filled().await;
    let entities = repo.graph_ids().await.len();
    let embedded = repo.held.embedded();
    assert_eq!(
        repo.compare().await,
        Verified::Done {
            compared: entities,
            differing: 0,
            strays: 0
        }
    );
    assert_eq!(repo.held.listed.load(Ordering::SeqCst), 0, "nothing listed");
    assert!(repo.pending().await.is_empty());
    assert_eq!(repo.held.embedded(), embedded, "nothing written");

    {
        let mut held = repo.held.entities.lock().unwrap();
        let lost = held
            .iter()
            .find(|(_, e)| e.file_path == "src/config.rs" && e.name == "parse_config")
            .map(|(at, _)| at.clone())
            .expect("parse_config");
        held.remove(&lost);
        let earlier = held
            .values_mut()
            .find(|e| e.name == "send_email")
            .expect("send_email");
        earlier.searchable_content = "as it was before the restore".into();
    }
    // What differs and nothing held besides: found, and still nothing of
    // what the store holds is gone through.
    assert_eq!(
        repo.compare().await,
        Verified::Done {
            compared: entities,
            differing: 2,
            strays: 0
        }
    );
    assert_eq!(repo.held.listed.load(Ordering::SeqCst), 0, "nothing listed");
    // One held that the graph has not: as many held as the graph has, one
    // of the graph's lacking. Now it is gone through, and the one found.
    repo.held.hold_stale("gone::long_ago");
    assert_eq!(
        repo.compare().await,
        Verified::Done {
            compared: entities,
            differing: 2,
            strays: 1
        }
    );
    assert!(repo.held.listed.load(Ordering::SeqCst) > 0);
    assert!(repo.complete().await, "still relied on: little differed");
    assert_eq!(
        repo.pending().await,
        ["src/config.rs", "src/gone.rs", "src/mail.rs"]
    );
    let fill = repo.graph.vector_fill(REPO).await.unwrap().unwrap();
    assert_eq!(fill.stage, FillStage::Done, "no fill begun");

    assert_eq!(repo.filler().settle_pending(REPO).await.unwrap(), 0);
    assert!(repo.agrees().await);
    let in_files = repo.held.entities.lock().unwrap();
    let rewritten = in_files
        .values()
        .filter(|e| e.file_path != "src/many.rs")
        .count();
    assert_eq!(
        repo.held.embedded(),
        embedded + rewritten,
        "only the files that differed were stored again"
    );
    assert!(in_files
        .values()
        .all(|e| e.searchable_content != "as it was before the restore"));
}

/// Summaries are compared as entities are, and one the store holds that
/// the graph has not is found.
#[tokio::test]
async fn summaries_that_have_drifted_are_found_too() {
    let repo = Repo::new();
    repo.index().await;
    repo.summarising()
        .run_summaries_only(RunOptions::default())
        .await
        .unwrap();
    repo.fill().await;
    assert!(repo.summaries_agree().await);
    let compared = match repo.compare().await {
        Verified::Done {
            compared,
            differing: 0,
            strays: 0,
        } => compared,
        other => panic!("{other:?}"),
    };
    assert!(compared > repo.graph_ids().await.len(), "summaries too");

    {
        let mut held = repo.held.summaries.lock().unwrap();
        let mut stray = held.values().next().expect("a summary").clone();
        let changed = held.values_mut().next().expect("a summary");
        changed.summary_text = "As it was.".into();
        stray.entity_id = "gone::long_ago".into();
        stray.file_path = "src/gone.rs".into();
        held.insert(summary_key(&stray), stray);
    }
    assert_eq!(
        repo.compare().await,
        Verified::Done {
            compared,
            differing: 1,
            strays: 1
        }
    );
    assert_eq!(repo.filler().settle_pending(REPO).await.unwrap(), 0);
    assert!(repo.summaries_agree().await);
    let held = repo.held.summaries.lock().unwrap();
    assert!(held.values().all(|s| s.summary_text != "As it was."));
}

/// A store that holds little of a repo as the graph has it, as one
/// restored to long ago, is not patched file by file: the repo's vectors
/// are recorded incomplete and filled again.
#[tokio::test]
async fn a_store_that_differs_in_much_is_filled_again() {
    let repo = Repo::of(150);
    repo.index().await;
    repo.fill().await;
    {
        let mut held = repo.held.entities.lock().unwrap();
        let kept: Vec<String> = held.keys().take(60).cloned().collect();
        held.retain(|at, _| kept.contains(at));
    }
    assert_eq!(repo.compare().await, Verified::Refilling);
    assert!(!repo.complete().await);
    let fill = repo.graph.vector_fill(REPO).await.unwrap().unwrap();
    assert_eq!((fill.stage, fill.done), (FillStage::Entities, 0));
    repo.fill().await;
    repo.filler().settle_pending(REPO).await.unwrap();
    assert!(repo.complete().await && repo.agrees().await);
}

/// A question given a deadline does not wait on a store that accepts a
/// search and never answers: it is answered from the graph once the
/// deadline passes, saying why. One slow answer costs the question that
/// waited; after three in a row the store is left alone, and questions do
/// not wait at all. Its rest over, one question tries it, and when it
/// answers it is used again.
#[tokio::test]
async fn a_question_does_not_wait_on_a_store_that_never_answers() {
    let repo = Repo::filled().await;
    repo.held.hang_search.store(true, Ordering::SeqCst);
    let wait = VectorWait {
        deadline: std::time::Duration::from_millis(150),
        rest: std::time::Duration::from_millis(600),
    };
    let engine =
        QueryEngine::with_vector_store(Arc::clone(&repo.graph), repo.store()).waiting(wait);
    let storeless = QueryEngine::new(Arc::clone(&repo.graph));
    let graph_only = storeless.ask(REPO, QUESTION, None, false).await.unwrap();
    assert_eq!(graph_only.vectors_skipped, None, "no store to skip");
    let ask = || async {
        let began = std::time::Instant::now();
        let answered = engine.ask(REPO, QUESTION, None, false);
        let answer = tokio::time::timeout(std::time::Duration::from_secs(10), answered)
            .await
            .expect("answered, though the store never is")
            .unwrap();
        (answer, began.elapsed())
    };
    assert_eq!(engine.vector_breaker(), Some("closed"));
    for nth in 1..=3 {
        let (answer, waited) = ask().await;
        assert!(
            waited >= wait.deadline && waited < wait.deadline * 3,
            "{waited:?}"
        );
        assert_eq!(answer.confidence, graph_only.confidence, "as with no store");
        let why = answer
            .vectors_skipped
            .expect("it says the vectors were skipped");
        assert!(why.contains("did not answer within"), "{why}");
        let told = if nth == 3 { "open" } else { "closed" };
        assert_eq!(engine.vector_breaker(), Some(told), "after {nth}");
    }
    let (answer, waited) = ask().await;
    assert!(waited < wait.deadline, "left alone: no wait at all");
    let why = answer.vectors_skipped.unwrap();
    assert!(why.contains("left alone"), "{why}");

    // Its rest over, one question tries it; still not answering, it is
    // left alone for another rest.
    tokio::time::sleep(wait.rest).await;
    assert_eq!(engine.vector_breaker(), Some("half_open"));
    let (_, waited) = ask().await;
    assert!(waited >= wait.deadline, "the one that tries it waits");
    assert_eq!(engine.vector_breaker(), Some("open"));
    assert!(ask().await.1 < wait.deadline);

    // Answering again, it is used again once tried.
    repo.held.hang_search.store(false, Ordering::SeqCst);
    tokio::time::sleep(wait.rest).await;
    let (answer, _) = ask().await;
    assert!(
        answer.confidence < graph_only.confidence,
        "scored by vectors"
    );
    assert_eq!(answer.vectors_skipped, None);
    assert_eq!(engine.vector_breaker(), Some("closed"));
}

/// A store that keeps no track of what it holds, as Weaviate's, is asked
/// as it always was: with no deadline of the engine's, whatever deadline
/// the engine was given. An engine given none has none for any store.
#[tokio::test]
async fn a_store_that_had_no_deadline_before_has_none_now() {
    let repo = Repo::filled().await;
    repo.held.untracked.store(true, Ordering::SeqCst);
    repo.held.hang_search.store(true, Ordering::SeqCst);
    let wait = VectorWait {
        deadline: std::time::Duration::from_millis(100),
        rest: std::time::Duration::from_secs(30),
    };
    let engine =
        QueryEngine::with_vector_store(Arc::clone(&repo.graph), repo.store()).waiting(wait);
    let asked = engine.ask(REPO, QUESTION, None, false);
    let waited = std::time::Duration::from_millis(1000);
    assert!(
        tokio::time::timeout(waited, asked).await.is_err(),
        "still waiting on its store"
    );
    repo.held.untracked.store(false, Ordering::SeqCst);
    let patient = QueryEngine::with_vector_store(Arc::clone(&repo.graph), repo.store());
    assert_eq!(patient.vector_breaker(), None);
    let asked = patient.ask(REPO, QUESTION, None, false);
    assert!(tokio::time::timeout(waited, asked).await.is_err());
}

/// So with the real store and a server that accepts connections and says
/// nothing: whether the repo's vectors are complete cannot be told in
/// time, and the question goes on without them, saying so.
#[tokio::test]
async fn a_question_does_not_wait_on_a_vellev_node_that_never_answers() {
    use workforce_codebase_knowledge::vector::VellevCodeVectorStore;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((socket, _)) = listener.accept().await {
            held.push(socket);
        }
    });
    let repo = Repo::filled().await;
    let silent: Arc<dyn CodeVectorStore> =
        Arc::new(VellevCodeVectorStore::new(&url, "key", "tenant").unwrap());
    let wait = VectorWait {
        deadline: std::time::Duration::from_millis(300),
        rest: std::time::Duration::from_secs(30),
    };
    let engine = QueryEngine::with_vector_store(Arc::clone(&repo.graph), silent).waiting(wait);
    for _ in 0..3 {
        let began = std::time::Instant::now();
        let answer = engine.ask(REPO, QUESTION, None, false).await.unwrap();
        let waited = began.elapsed();
        assert!(
            waited >= wait.deadline && waited < std::time::Duration::from_secs(3),
            "{waited:?}"
        );
        assert!(!answer.answer.is_empty());
        let why = answer.vectors_skipped.unwrap();
        assert!(why.contains("did not answer within"), "{why}");
    }
    let began = std::time::Instant::now();
    let answer = engine.ask(REPO, QUESTION, None, false).await.unwrap();
    assert!(began.elapsed() < wait.deadline, "left alone");
    assert!(answer.vectors_skipped.unwrap().contains("left alone"));
}

/// A repo recorded complete of which a search finds nothing is suspected:
/// whoever keeps the vectors is told, to compare them with the graph.
#[tokio::test]
async fn a_search_that_finds_nothing_of_a_complete_repo_has_it_compared() {
    let repo = Repo::filled().await;
    let engine = QueryEngine::with_vector_store(Arc::clone(&repo.graph), repo.store());
    let wanted = engine.vectors_wanted();
    let told = tokio::spawn(async move { wanted.notified().await });
    tokio::task::yield_now().await;
    engine.ask(REPO, QUESTION, None, false).await.unwrap();
    let suspected = engine.vectors_suspected();
    assert!(suspected.lock().unwrap().contains(REPO));
    tokio::time::timeout(std::time::Duration::from_secs(2), told)
        .await
        .expect("whoever keeps the vectors is woken")
        .unwrap();

    // One that finds something is not.
    suspected.lock().unwrap().clear();
    let found = VectorSearchResult {
        entity_id: repo.graph_ids().await[0].clone(),
        repo_id: REPO.to_string(),
        qualified_name: String::new(),
        kind: "Function".to_string(),
        file_path: String::new(),
        start_line: 1,
        end_line: 2,
        score: 0.9,
    };
    *repo.held.found.lock().unwrap() = vec![found];
    engine.ask(REPO, QUESTION, None, false).await.unwrap();
    assert!(suspected.lock().unwrap().is_empty());
}

/// A page is read again from the graph after it is stored, to see what
/// changed under it. If more came into the page meanwhile than is read
/// again, the fill does not go past what it has not seen: the page fails,
/// to be tried again, and nothing is recorded complete without it.
#[tokio::test]
async fn a_page_longer_than_it_is_read_again_is_not_passed_over() {
    let repo = Repo::new();
    repo.index().await;
    let entities = repo.graph_ids().await.len();
    // Pages of one, read again as up to two: a page that has grown to two
    // or more between its two reads cannot be told from one cut short.
    let filler = VectorFiller::new(Arc::clone(&repo.graph), repo.store())
        .paged(1)
        .checking(1);
    let fill = filler.step(REPO).await.unwrap();
    assert_eq!(fill.done, 1);

    // An index run adds entities that sort before the page's last, after
    // the page was read and before it is read again.
    let first = repo
        .graph
        .entities_page(REPO, &fill.cursor, None, 1)
        .await
        .unwrap();
    let mut crowd = Vec::new();
    for n in 0..3 {
        let mut added = first[0].clone();
        let earlier = format!("{}{n}", fill.cursor);
        assert!(earlier.as_str() < first[0].id.as_str());
        added.id = EntityId(earlier);
        added.name = format!("crowd_{n}");
        crowd.push(added);
    }
    *repo.held.crowd.lock().unwrap() = crowd;
    let refused = filler.step(REPO).await.unwrap_err().to_string();
    assert!(refused.contains("came into a page"), "{refused}");
    let kept = repo.graph.vector_fill(REPO).await.unwrap().unwrap();
    assert_eq!(kept.cursor, fill.cursor, "the fill keeps its place");

    // Read again as more, the page is stored whole, and the fill ends
    // with every entity, the added ones among them.
    let roomy = VectorFiller::new(Arc::clone(&repo.graph), repo.store()).paged(1);
    stepped_to(&roomy, FillStage::Done).await;
    assert!(repo.complete().await && repo.agrees().await);
    assert_eq!(repo.held.ids().len(), entities + 3);
}

/// Pages are stored ahead of where a fill has got to, when it is told to,
/// and recorded in their order: the repo ends as the graph has it. If the
/// fill is begun again meanwhile, as a full index begins it, the pages
/// that were ahead are let go: nothing of the old fill is recorded as the
/// new one's, which would pass over what it has yet to store.
#[tokio::test]
async fn pages_stored_ahead_of_a_fill_begun_again_are_let_go() {
    let repo = Repo::new();
    repo.index().await;
    let entities = repo.graph_ids().await.len();
    let filler = VectorFiller::new(Arc::clone(&repo.graph), repo.store())
        .paged(2)
        .ahead(3);
    let first = filler.step(REPO).await.unwrap();
    assert_eq!(first.done, 2, "one page recorded");
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
    assert_eq!(repo.held.embedded(), 6, "and two more stored ahead of it");

    let again = filler.begin(REPO, false).await.unwrap();
    assert_ne!(again.generation, first.generation);
    let pages = stepped_to(&filler, FillStage::Done).await - 1;
    assert!(pages >= entities / 2, "{pages} pages");
    assert!(repo.complete().await && repo.agrees().await);
    assert_eq!(
        repo.held.generations(),
        std::slice::from_ref(&again.generation)
    );
    let fill = repo.graph.vector_fill(REPO).await.unwrap().unwrap();
    assert_eq!(fill.done, entities as u64, "each counted once");
}

/// Pages on their way to the store when a fill is told to let them go
/// are given up, and none is left running when it returns: nothing of
/// them reaches the store afterwards, as it would while an index run that
/// began meanwhile changes the graph. The fill keeps its place, and going
/// on from it stores each entity once.
#[tokio::test]
async fn pages_let_go_are_given_up_and_none_is_left_running() {
    let repo = Repo::new();
    repo.index().await;
    let entities = repo.graph_ids().await.len();
    // The store takes the first page and holds every later write; and it
    // has one write at a time, so the third page waits for the second's
    // place.
    let gate = Arc::new(tokio::sync::Semaphore::new(1));
    *repo.held.gate.lock().unwrap() = Some(Arc::clone(&gate));
    *repo.held.room.lock().unwrap() = Some(Arc::new(tokio::sync::Semaphore::new(1)));
    let entered = || repo.held.entered.load(Ordering::SeqCst);
    let filler = VectorFiller::new(Arc::clone(&repo.graph), repo.store())
        .paged(2)
        .ahead(3);
    let first = filler.step(REPO).await.unwrap();
    assert_eq!(first.done, 2, "one page recorded");
    let writing = || repo.held.writing.load(Ordering::SeqCst);
    for _ in 0..1000 {
        if writing() == 2 {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(writing(), 2, "two more with the store, on their way");
    assert_eq!(repo.held.embedded(), 2);
    for _ in 0..100 {
        tokio::task::yield_now().await;
    }
    assert_eq!(entered(), 2, "the first page, and the second in its place");

    filler.let_go(REPO).await;
    assert_eq!(writing(), 0, "none is running once they are let go");
    // All were given up before any was waited for: the third did not
    // take the place the second left as it went.
    assert_eq!(entered(), 2, "none took the place of one given up");
    // Nor does one go on when the store would take it.
    gate.add_permits(1000);
    for _ in 0..100 {
        tokio::task::yield_now().await;
    }
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert_eq!(repo.held.embedded(), 2, "nothing of them is stored after");
    assert_eq!((writing(), entered()), (0, 2));
    let kept = repo.graph.vector_fill(REPO).await.unwrap().unwrap();
    assert_eq!(
        (kept.done, &kept.cursor),
        (2, &first.cursor),
        "its place kept"
    );

    // Gone on with, the pages ahead are taken up as they are stored, and
    // not begun again: each entity is stored once.
    stepped_to(&filler, FillStage::Done).await;
    assert!(repo.complete().await && repo.agrees().await);
    assert_eq!(repo.held.embedded(), entities, "each stored once");
}

/// Two methods of one name in two impl blocks of one file are two
/// entities, each with an id of its own: the graph has both, the store
/// holds a vector for each, and the two agree after a fill and after
/// incremental runs, with nothing to put right. They used to share an id:
/// the graph kept the second alone while the store kept both, so every
/// comparison after an index run removed one and the next run put it back.
///
/// And the id of each is its own whatever becomes of the others, over
/// incremental runs that reorder them, add one and take one away: no id
/// changes until one alone is left of the name, which then has the id
/// the name alone gives.
#[tokio::test]
async fn methods_of_one_name_in_one_file_are_two_entities_and_stay_so() {
    let source = |impls: &[&str]| {
        let mut source = String::from("pub struct A;\npub struct B;\npub struct C;\n");
        for of in impls {
            source.push_str(&format!(
                "\nimpl {of} {{\n    /// Makes one of {of}.\n    pub fn new() -> Self {{\n        {of}\n    }}\n}}\n"
            ));
        }
        source
    };
    let repo = Repo::new();
    write(repo.dir.path(), "two.rs", &source(&["A", "B"]));
    commit(repo.dir.path());
    repo.index().await;
    // Each `new` of the file, by the type it makes.
    let news = || async {
        let in_file = repo.graph.find_entities_in_file(REPO, "src/two.rs").await;
        let news = in_file.unwrap().into_iter().filter(|e| e.name == "new");
        news.map(|e| (e.qualified_name.clone(), e.id))
            .collect::<std::collections::BTreeMap<_, _>>()
    };
    let id_of = |apart: &str| EntityId::from_components(REPO, "src/two.rs", "method", apart);
    let (a, b, c) = (id_of("new\u{1}A"), id_of("new\u{1}B"), id_of("new\u{1}C"));
    let named = |ids: &[(&str, &EntityId)]| {
        ids.iter()
            .map(|(name, id)| (name.to_string(), (*id).clone()))
            .collect::<std::collections::BTreeMap<_, _>>()
    };
    // Each is told apart by where it stands, the first as much as the
    // second: neither has the id that depends on which is written first.
    assert_eq!(news().await, named(&[("A::new", &a), ("B::new", &b)]));

    repo.fill().await;
    assert!(repo.agrees().await);
    let agreed = |compared| Verified::Done {
        compared,
        differing: 0,
        strays: 0,
    };
    let entities = repo.graph_ids().await.len();
    assert_eq!(repo.compare().await, agreed(entities));

    // Incremental runs of the file. After each the graph and the store
    // agree, with nothing left to compare away.
    let pipeline = repo.pipeline();
    let rewritten = |impls: &'static [&'static str], entities: usize| {
        let (repo, pipeline, source) = (&repo, &pipeline, &source);
        async move {
            write(repo.dir.path(), "two.rs", &source(impls));
            commit(repo.dir.path());
            repo.incremental(pipeline, 1).await;
            assert!(repo.pending().await.is_empty(), "{impls:?}");
            assert!(repo.agrees().await, "{impls:?}");
            assert_eq!(repo.compare().await, agreed(entities), "{impls:?}");
            assert!(
                repo.pending().await.is_empty(),
                "nothing put right: {impls:?}"
            );
        }
    };
    rewritten(&["B", "A"], entities).await;
    assert_eq!(
        news().await,
        named(&[("A::new", &a), ("B::new", &b)]),
        "reordered"
    );
    rewritten(&["C", "B", "A"], entities + 1).await;
    let three = named(&[("A::new", &a), ("B::new", &b), ("C::new", &c)]);
    assert_eq!(news().await, three, "one added before them");
    rewritten(&["B", "C"], entities).await;
    assert_eq!(
        news().await,
        named(&[("B::new", &b), ("C::new", &c)]),
        "one removed"
    );
    // One alone of the name: known by the name alone, its one change.
    rewritten(&["C"], entities - 1).await;
    assert_eq!(news().await, named(&[("C::new", &id_of("new"))]));
}
