//! A repo's vectors in a store that keeps track of them: filled from the
//! graph a page at a time, resumably, with nothing deleted first; put
//! right file by file when an index run's write fails; and relied on only
//! once they are complete in the collection the store has. With a vector
//! store kept in memory that fails, and changes the graph under a page,
//! as told.

use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use eyre::Result;
use workforce_codebase_knowledge::graph::entities::{EntityId, EntityKind};
use workforce_codebase_knowledge::graph::vectors::FillStage;
use workforce_codebase_knowledge::graph::{CodeGraph, VectorKeys};
use workforce_codebase_knowledge::indexer::crawler::CrawlerConfig;
use workforce_codebase_knowledge::indexer::fill::{VectorFiller, Verified, Verifying};
use workforce_codebase_knowledge::indexer::summary_generator::{SummaryGenerator, SummaryLlm};
use workforce_codebase_knowledge::indexer::{IndexingConfig, IndexingPipeline, RunOptions};
use workforce_codebase_knowledge::query::natural_language::QueryPlanner;
use workforce_codebase_knowledge::query::{QueryEngine, VectorWait};
use workforce_codebase_knowledge::testing::{
    entity_key, summary_key, Counting, Failing, Held, Meanwhile, RECORDED, REPO,
};
use workforce_codebase_knowledge::vector::{
    health::InsertErrorSink, CodeVectorStore, DeleteOutcome, VectorCodeEntity, VectorCodeSummary,
    VectorSearchResult,
};
use workforce_vellev::testing::{logged, NotServing};
const QUESTION: &str = "how is `parse_config` used";

/// A store that keeps no track, as Weaviate's: another store than `Held`.
struct Untracked;

#[async_trait]
impl CodeVectorStore for Untracked {
    fn backend(&self) -> &'static str {
        VectorKeys::WEAVIATE
    }
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
    let filler = repo.filler();
    // Filled again at an operator's word: a store holding the repo is
    // otherwise adopted.
    filler.begin(REPO, false).await.unwrap();
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

/// A graph over the same database as `repo`'s, keeping its vector state
/// as `keys` say.
fn keyed(repo: &Repo, active: &str, standby: Option<&str>) -> Arc<CodeGraph> {
    Arc::new(
        CodeGraph::from_backend(Arc::clone(repo.graph.backend()))
            .keyed(VectorKeys::new(active, standby).unwrap()),
    )
}

/// An index run that writes to another store, or to none, with the
/// tracked store (vellev's) configured as the standby: as a hub moved back
/// to Weaviate does, or one still on Weaviate while vellev is seeded. The
/// files it changes are noted for the standby, whose vectors stay complete
/// but for them, and are put right there file by file. A full index there
/// rebuilds the graph: the standby is told of every file the repo had and
/// has, and so stays complete but for them, its fill as it was.
#[tokio::test]
async fn an_index_run_on_another_store_notes_its_files_for_the_standby() {
    for elsewhere in [None, Some(Arc::new(Untracked) as Arc<dyn CodeVectorStore>)] {
        let repo = Repo::filled().await;
        let embedded = repo.held.embedded();
        let on_weaviate = keyed(&repo, "weaviate", Some("vellev"));
        let other = |repo: &Repo| match &elsewhere {
            Some(store) => IndexingPipeline::with_vector_store(
                repo.config(),
                Arc::clone(&on_weaviate),
                Arc::clone(store),
            ),
            None => IndexingPipeline::new(repo.config(), Arc::clone(&on_weaviate)),
        };
        repo.change_mail();
        repo.incremental(&other(&repo), 1).await;
        std::fs::remove_file(repo.dir.path().join("src/config.rs")).unwrap();
        commit(repo.dir.path());
        repo.incremental(&other(&repo), 0).await;
        assert_eq!(repo.pending().await, ["src/config.rs", "src/mail.rs"]);
        assert_eq!(
            on_weaviate.count_vectors_pending(REPO).await.unwrap(),
            0,
            "nothing noted for the store in use, which keeps no track"
        );
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

        // A full index there: every file the repo had, and has, noted. One
        // it had is gone by then, never indexed as gone, so only what the
        // repo had says the standby holds it.
        write(repo.dir.path(), "post.rs", MAIL_RS);
        std::fs::remove_file(repo.dir.path().join("src/many.rs")).unwrap();
        commit(repo.dir.path());
        let had = repo.graph.get_file_paths(REPO).await.unwrap();
        assert!(had.contains(&"src/many.rs".to_string()), "{had:?}");
        assert!(!repo.pending().await.contains(&"src/many.rs".to_string()));
        let complete_in = repo.graph.vectors_complete_in(REPO).await.unwrap();
        let fill = repo.graph.vector_fill(REPO).await.unwrap();
        assert!(complete_in.is_some() && fill.is_some());
        other(&repo)
            .run_full_index(RunOptions::default())
            .await
            .unwrap();
        assert_eq!(
            repo.graph.vectors_complete_in(REPO).await.unwrap(),
            complete_in,
            "a full index elsewhere"
        );
        assert_eq!(repo.graph.vector_fill(REPO).await.unwrap(), fill);
        let mut expected: Vec<String> = had;
        expected.push("src/post.rs".to_string());
        expected.sort();
        expected.dedup();
        assert_eq!(repo.pending().await, expected);
        assert!(repo.complete().await, "but for the files noted");
        assert_eq!(repo.filler().settle_pending(REPO).await.unwrap(), 0);
        assert!(
            repo.agrees().await,
            "what it held of the files gone is gone"
        );
    }
}

/// A full index carries no completeness for vellev that nothing kept: not
/// with vellev in use but no store of it built, whose run notes nothing
/// for it, nor with Weaviate in use and no standby.
#[tokio::test]
async fn a_full_index_forgets_completeness_nothing_kept() {
    for (active, standby, store) in [
        ("vellev", Some("weaviate"), None),
        (
            "weaviate",
            None,
            Some(Arc::new(Untracked) as Arc<dyn CodeVectorStore>),
        ),
    ] {
        let repo = Repo::filled().await;
        let graph = keyed(&repo, active, standby);
        let pipeline = match store {
            Some(store) => IndexingPipeline::with_vector_store(repo.config(), graph, store),
            None => IndexingPipeline::new(repo.config(), graph),
        };
        pipeline
            .run_full_index(RunOptions::default())
            .await
            .unwrap();
        assert_eq!(
            repo.graph.vectors_complete_in(REPO).await.unwrap(),
            None,
            "{active}"
        );
    }
}

/// A full index that fails once the repo is purged leaves what the
/// standby's vectors were complete in for the next one to carry; with no
/// standby, nothing is left.
#[tokio::test]
async fn a_full_index_that_fails_leaves_the_standbys_completeness() {
    for standby in [Some("vellev"), None] {
        let repo = Repo::filled().await;
        let complete_in = repo.graph.vectors_complete_in(REPO).await.unwrap();
        assert!(complete_in.is_some());
        let expected = complete_in.filter(|_| standby.is_some());
        let graph = keyed(&repo, "weaviate", standby);
        let pipeline = |config| {
            IndexingPipeline::with_vector_store(config, Arc::clone(&graph), Arc::new(Untracked))
        };
        let mut gone = repo.config();
        gone.crawler.root = repo.dir.path().join("gone");
        assert!(pipeline(gone)
            .run_full_index(RunOptions::default())
            .await
            .is_err());
        assert!(repo.graph.get_index_state(REPO).await.unwrap().is_none());
        assert_eq!(
            repo.graph.vectors_complete_in(REPO).await.unwrap(),
            expected,
            "{standby:?}"
        );
        pipeline(repo.config())
            .run_full_index(RunOptions::default())
            .await
            .unwrap();
        assert_eq!(
            repo.graph.vectors_complete_in(REPO).await.unwrap(),
            expected,
            "{standby:?}"
        );
    }
}

/// The files whose summaries a run makes are noted for the standby, with
/// the store in use keeping track or not: those of the files the run
/// changed, and those of files it did not change at all, as the summaries
/// a resumed summary phase makes across the repo are. For a store that
/// keeps track, before a fill of it has begun too, when it leaves the
/// summaries to the fill.
#[tokio::test]
async fn the_files_whose_summaries_a_run_makes_are_noted_for_the_standby() {
    for in_use in ["weaviate", "vellev"] {
        let repo = Repo::new();
        let (store, standby): (Arc<dyn CodeVectorStore>, &str) = match in_use {
            "weaviate" => (Arc::new(Untracked), "vellev"),
            _ => (repo.store(), "weaviate"),
        };
        let graph = keyed(&repo, in_use, Some(standby));
        let pipeline = || {
            IndexingPipeline::with_vector_store(
                repo.config(),
                Arc::clone(&graph),
                Arc::clone(&store),
            )
        };
        let summarising =
            || pipeline().with_summary_generator(SummaryGenerator::new(Box::new(Terse), 64));
        let noted = || async {
            let noted = graph
                .backend()
                .vectors_pending(standby, REPO, 100)
                .await
                .unwrap();
            noted.into_iter().map(|(file, _)| file).collect::<Vec<_>>()
        };
        let forget = || async {
            for (file, seq) in graph
                .backend()
                .vectors_pending(standby, REPO, 100)
                .await
                .unwrap()
            {
                graph
                    .backend()
                    .clear_vectors_pending(standby, REPO, std::slice::from_ref(&file), seq)
                    .await
                    .unwrap();
            }
        };
        pipeline()
            .run_full_index(RunOptions::default())
            .await
            .unwrap();
        forget().await;

        // A changed file, summarised as it is indexed: its own summaries.
        repo.change_mail();
        let made = summarising()
            .run_incremental_index(RunOptions::default())
            .await
            .unwrap();
        assert!(made.summaries_generated > 0, "{in_use}: {made:?}");
        assert_eq!(noted().await, ["src/mail.rs"], "{in_use}");
        forget().await;

        // Summaries of the files no run changed, made as the summary phase
        // is resumed: noted, file by file. `mail.rs`'s, fresh already and
        // noted as they were made, are not noted again.
        let made = summarising()
            .run_summaries_only(RunOptions::default())
            .await
            .unwrap();
        assert!(made.summaries_generated > 0, "{in_use}: {made:?}");
        assert_eq!(
            noted().await,
            ["src/config.rs", "src/many.rs"],
            "{in_use}: the files outside any change"
        );
    }
}

/// With no standby configured, a run on another store notes nothing for
/// the tracked one: the standby is how one is kept in step, and a hub
/// moved off vellev without naming it as the standby leaves it to fall
/// behind (to be found by the next comparison).
#[tokio::test]
async fn an_index_run_on_another_store_notes_nothing_without_a_standby() {
    let repo = Repo::filled().await;
    let on_weaviate = keyed(&repo, "weaviate", None);
    repo.change_mail();
    let other =
        IndexingPipeline::with_vector_store(repo.config(), on_weaviate, Arc::new(Untracked));
    repo.incremental(&other, 1).await;
    assert!(repo.pending().await.is_empty());
}

/// A run on the tracked store, with Weaviate as the standby (vellev in
/// use, Weaviate kept for a rollback): the run takes off its own notes of
/// the files it stored, and leaves the standby's, so that Weaviate is put
/// right from them if it is switched back to.
#[tokio::test]
async fn a_run_on_the_tracked_store_leaves_the_standbys_notes() {
    let repo = Repo::filled().await;
    let with_standby = keyed(&repo, "vellev", Some("weaviate"));
    let pipeline =
        IndexingPipeline::with_vector_store(repo.config(), Arc::clone(&with_standby), repo.store());
    repo.change_mail();
    repo.incremental(&pipeline, 1).await;
    assert!(repo.pending().await.is_empty(), "its own taken off");
    assert!(repo.complete().await && repo.agrees().await);
    let standby = repo
        .graph
        .backend()
        .vectors_pending("weaviate", REPO, 10)
        .await
        .unwrap();
    let standby: Vec<&str> = standby.iter().map(|(file, _)| file.as_str()).collect();
    assert_eq!(standby, ["src/mail.rs"]);
}

/// A run on the tracked store with a standby notes every file it changes
/// in both under one mark, and takes off its own by that mark once its
/// store has them: none is left pending in the store in use.
#[tokio::test]
async fn a_clean_run_takes_off_its_own_notes_by_their_mark() {
    let repo = Repo::filled().await;
    let with_standby = keyed(&repo, "vellev", Some("weaviate"));
    let pipeline =
        IndexingPipeline::with_vector_store(repo.config(), Arc::clone(&with_standby), repo.store());
    repo.change_mail();
    write(repo.dir.path(), "post.rs", MAIL_RS);
    std::fs::remove_file(repo.dir.path().join("src/config.rs")).unwrap();
    commit(repo.dir.path());
    repo.incremental(&pipeline, 2).await;
    assert!(repo.pending().await.is_empty());
    assert!(repo.complete().await && repo.agrees().await);
    let standby = with_standby
        .backend()
        .vectors_pending("weaviate", REPO, 10)
        .await
        .unwrap();
    let files: Vec<&str> = standby.iter().map(|(file, _)| file.as_str()).collect();
    assert_eq!(files, ["src/config.rs", "src/mail.rs", "src/post.rs"]);
    assert!(
        standby.iter().all(|(_, mark)| *mark == standby[0].1),
        "one mark: {standby:?}"
    );
}

/// With no vector store at all, the files of the summaries a run makes
/// are noted for the standby still.
#[tokio::test]
async fn the_files_summarised_with_no_vector_store_are_noted_for_the_standby() {
    let repo = Repo::new();
    let graph = keyed(&repo, "weaviate", Some("vellev"));
    IndexingPipeline::new(repo.config(), Arc::clone(&graph))
        .run_full_index(RunOptions::default())
        .await
        .unwrap();
    let noted = || async {
        let noted = graph.backend().vectors_pending("vellev", REPO, 100).await;
        noted.unwrap()
    };
    for (file, mark) in noted().await {
        graph
            .backend()
            .clear_vectors_pending("vellev", REPO, &[file], mark)
            .await
            .unwrap();
    }
    let made = IndexingPipeline::new(repo.config(), Arc::clone(&graph))
        .with_summary_generator(SummaryGenerator::new(Box::new(Terse), 64))
        .run_summaries_only(RunOptions::default())
        .await
        .unwrap();
    assert!(made.summaries_generated > 0, "{made:?}");
    let files: Vec<String> = noted().await.into_iter().map(|(file, _)| file).collect();
    assert_eq!(files, ["src/config.rs", "src/mail.rs", "src/many.rs"]);
}

/// A run whose summaries cannot be noted for the standby fails, and writes
/// none: a summary written unnoted would never reach the standby.
#[tokio::test]
async fn a_run_whose_summaries_cannot_be_noted_fails() {
    let repo = Repo::new();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("codegraph.db");
    let graph = Arc::new(
        CodeGraph::open(&path)
            .unwrap()
            .keyed(VectorKeys::new("weaviate", Some("vellev")).unwrap()),
    );
    IndexingPipeline::new(repo.config(), Arc::clone(&graph))
        .run_full_index(RunOptions::default())
        .await
        .unwrap();
    rusqlite::Connection::open(&path)
        .unwrap()
        .execute_batch(
            "CREATE TRIGGER refused BEFORE UPDATE ON vector_note_mark
             BEGIN SELECT RAISE(ABORT, 'refused'); END;",
        )
        .unwrap();
    let run = IndexingPipeline::new(repo.config(), Arc::clone(&graph))
        .with_summary_generator(SummaryGenerator::new(Box::new(Terse), 64))
        .run_summaries_only(RunOptions::default())
        .await;
    let error = format!("{:#}", run.expect_err("the note failed"));
    assert!(error.contains("refused"), "{error}");
    assert_eq!(graph.count_summaries_present(REPO).await.unwrap(), 0);
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

/// A repo indexed, its vectors recorded complete in [`RECORDED`].
async fn recorded() -> Repo {
    let repo = Repo::new();
    repo.index().await;
    repo.graph
        .set_vectors_complete_in(REPO, Some(RECORDED))
        .await
        .unwrap();
    repo
}

/// With code search off a question is answered from the graph alone,
/// saying so, and asks the store nothing: nothing is wanted of whoever
/// fills the vectors, and nothing is suspected of them.
#[tokio::test]
async fn with_code_search_off_a_question_asks_no_store_and_says_why() {
    use workforce_codebase_knowledge::vector::CodeSearchUnavailable;
    let repo = recorded().await;
    for tracked in [true, false] {
        let store = Counting::new(tracked, Failing::Not);
        let engine = QueryEngine::with_vector_store(Arc::clone(&repo.graph), store.clone())
            .waiting(VectorWait::default())
            .code_search(false);
        let wanted = engine.vectors_wanted();
        let told = tokio::spawn(async move { wanted.notified().await });
        tokio::task::yield_now().await;
        let answer = engine.ask(REPO, QUESTION, None, false).await.unwrap();
        assert_eq!(
            answer.vectors_skipped,
            Some(CodeSearchUnavailable::Off.to_string()),
            "tracked: {tracked}"
        );
        assert_eq!(store.called(), 0, "tracked: {tracked}");
        let waited = tokio::time::timeout(std::time::Duration::from_millis(100), told).await;
        assert!(waited.is_err(), "nothing is wanted of the filler");
        assert!(engine.vectors_suspected().lock().unwrap().is_empty());
    }
}

/// A store whose node does not serve its tenant leaves code search
/// unavailable, saying so in the store's own words, whether its search or
/// its readiness after a search finds that; any other failure is said as
/// it was.
#[tokio::test]
async fn a_misrouted_store_leaves_code_search_unavailable_saying_so() {
    let repo = recorded().await;
    let skipped = |store: Arc<Counting>| {
        let engine = QueryEngine::with_vector_store(Arc::clone(&repo.graph), store);
        async move {
            let answer = engine.ask(REPO, QUESTION, None, false).await.unwrap();
            answer.vectors_skipped.expect("the vectors were skipped")
        }
    };
    let searched = "the search of the vector store failed: the node went away";
    let readied = "the vector store failed: the node went away";
    for (tracked, spared, failed) in [
        (true, 0, searched),
        (false, 0, searched),
        (true, 1, readied),
    ] {
        let misrouted = skipped(Counting::sparing(tracked, Failing::Misrouted, spared)).await;
        assert!(
            misrouted.starts_with("code search unavailable: misrouted: ")
                && misrouted.contains("http://code-node:18080")
                && misrouted.contains("'code'"),
            "tracked: {tracked}, spared: {spared}: {misrouted}"
        );
        let otherwise = skipped(Counting::sparing(tracked, Failing::Otherwise, spared)).await;
        assert_eq!(otherwise, failed, "tracked: {tracked}, spared: {spared}");
    }
}

/// An insert-error sink that keeps what it hears.
#[derive(Default)]
struct Heard(Mutex<Vec<String>>);

impl InsertErrorSink for Heard {
    fn record(&self, message: String) {
        self.0.lock().unwrap().push(message);
    }
}

fn is_misrouted(e: &eyre::Report, node: &NotServing) -> bool {
    e.downcast_ref() == Some(&node.misrouted("code"))
}

/// A code store whose node does not serve its tenant asks once, from
/// every way in, and says where it asked: so does a failed write, as the
/// insert-error sink and the indexer's warning hear of it.
#[tokio::test]
async fn a_misrouted_code_store_asks_once_and_says_where() {
    use workforce_codebase_knowledge::vector::VellevCodeVectorStore;
    let node = NotServing::up().await;
    let fresh = || {
        VellevCodeVectorStore::new(&node.url, "key", "code")
            .unwrap()
            .for_graph("g")
    };
    let once = |before: usize| node.asked() - before;

    let before = node.asked();
    let searched = fresh().search_entities(QUESTION, Some(REPO), None, 5).await;
    assert!(is_misrouted(&searched.unwrap_err(), &node));
    assert_eq!(once(before), 1, "search");

    let before = node.asked();
    let identified = fresh().identity(false).await;
    assert!(is_misrouted(&identified.unwrap_err(), &node));
    assert_eq!(once(before), 1, "identity");

    let before = node.asked();
    let deleted = fresh().delete_repo_entities(REPO).await;
    assert!(is_misrouted(&deleted.unwrap_err(), &node));
    assert_eq!(once(before), 1, "delete");

    let before = node.asked();
    let store = fresh();
    let heard = Arc::new(Heard::default());
    store.set_insert_error_sink(Arc::clone(&heard) as Arc<dyn InsertErrorSink>);
    let entity = VectorCodeEntity {
        entity_id: "e".into(),
        repo_id: REPO.into(),
        qualified_name: "crate::e".into(),
        name: "e".into(),
        kind: "Function".into(),
        file_path: "src/e.rs".into(),
        language: "Rust".into(),
        signature: String::new(),
        doc_comment: String::new(),
        summary: String::new(),
        searchable_content: "parses the configuration".into(),
        start_line: 1,
        end_line: 2,
        generation: String::new(),
    };
    let indexed = store.index_entities(&[entity]).await;
    assert!(is_misrouted(&indexed.unwrap_err(), &node));
    assert_eq!(once(before), 1, "index");
    let heard = heard.0.lock().unwrap().clone();
    assert!(
        heard.len() == 1 && heard[0].contains("misrouted") && heard[0].contains(&node.url),
        "{heard:?}"
    );

    let repo = Repo::new();
    repo.index().await;
    repo.change_mail();
    let misrouted: Arc<dyn CodeVectorStore> = Arc::new(fresh());
    let pipeline =
        IndexingPipeline::with_vector_store(repo.config(), Arc::clone(&repo.graph), misrouted);
    logged::kept();
    repo.incremental(&pipeline, 1).await;
    let warned = logged::at("WARN");
    assert!(
        warned
            .iter()
            .any(|line| line.contains("Vector indexing failed") && line.contains("misrouted")),
        "{warned:?}"
    );
}

/// Two questions of a store whose node does not serve its tenant, within
/// the recheck, reach the node once between them, and both say why code
/// search is unavailable.
#[tokio::test]
async fn two_questions_of_a_misrouted_store_within_the_recheck_cost_one_request() {
    use workforce_codebase_knowledge::vector::VellevCodeVectorStore;
    let node = NotServing::up().await;
    let repo = recorded().await;
    let store = VellevCodeVectorStore::new(&node.url, "key", "code")
        .unwrap()
        .for_graph("g");
    let engine = QueryEngine::with_vector_store(Arc::clone(&repo.graph), Arc::new(store))
        .waiting(VectorWait::default());
    for _ in 0..2 {
        let answer = engine.ask(REPO, QUESTION, None, false).await.unwrap();
        let why = answer.vectors_skipped.expect("the vectors were skipped");
        assert!(
            why.starts_with("code search unavailable: misrouted:"),
            "{why}"
        );
    }
    assert_eq!(node.asked(), 1);
}

impl Repo {
    /// What the graph has of the repo, as a store holds it.
    async fn as_held(&self) -> (Vec<VectorCodeEntity>, Vec<VectorCodeSummary>) {
        let entities = self.graph.all_entities(REPO).await.unwrap();
        let summaries = self
            .graph
            .summaries_page(REPO, "", None, 100_000)
            .await
            .unwrap();
        (
            entities
                .iter()
                .map(|entity| VectorCodeEntity::from_code_entity(entity, REPO, None))
                .collect(),
            summaries
                .iter()
                .map(|summary| VectorCodeSummary::of(summary, REPO, ""))
                .collect(),
        )
    }

    /// A repo of `functions` functions indexed in full, the store holding
    /// it as the graph has it though it is recorded complete nowhere: as a
    /// tenant built elsewhere and adopted whole.
    async fn prebuilt(functions: usize) -> Self {
        let repo = Self::of(functions);
        repo.index().await;
        let (entities, summaries) = repo.as_held().await;
        repo.held.hold(&entities);
        repo.held.hold_summaries(&summaries);
        repo
    }

    /// Whether the store holds the repo's entities and summaries exactly as
    /// the graph has them.
    async fn held_as_graph(&self) -> bool {
        let (entities, summaries) = self.as_held().await;
        let mut wanted: Vec<String> = entities.iter().map(entity_key).collect();
        wanted.extend(summaries.iter().map(summary_key));
        let mut held: Vec<String> = self.held.entities.lock().unwrap().keys().cloned().collect();
        held.extend(self.held.summaries.lock().unwrap().keys().cloned());
        wanted.sort();
        held.sort();
        let filler = self.filler();
        let confirmed = filler.confirm(REPO).await.unwrap();
        let differing = self
            .store()
            .differing_entities(REPO, &entities)
            .await
            .unwrap();
        wanted == held && confirmed.agrees() && differing.places.is_empty()
    }

    /// Change the entities named `names` in the store, as a store holding
    /// an earlier text of them does: one more line each.
    fn held_otherwise(&self, names: &[&str]) {
        let mut held = self.held.entities.lock().unwrap();
        for entity in held
            .values_mut()
            .filter(|e| names.contains(&e.name.as_str()))
        {
            entity.end_line += 1;
        }
    }
}

/// Step the adoption of the repo until it is done, saying every stage it
/// was at.
async fn adopted(filler: &VectorFiller) -> Vec<FillStage> {
    let mut stages = Vec::new();
    for _ in 0..1000 {
        let fill = filler.step(REPO).await.unwrap();
        stages.push(fill.stage);
        if fill.stage == FillStage::Done {
            return stages;
        }
    }
    panic!("the adoption did not end");
}

/// A repo being adopted is served from its vectors, and found empty it is
/// not suspected: it is being compared already. One recorded complete and
/// found empty is.
#[tokio::test]
async fn an_adopting_repo_found_empty_is_not_suspected_but_a_complete_one_is() {
    let repo = Repo::prebuilt(12).await;
    let fill = repo.filler().step(REPO).await.unwrap();
    assert_eq!(fill.stage, FillStage::AdoptEntities);
    let engine = QueryEngine::with_vector_store(Arc::clone(&repo.graph), repo.store());
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
    let answer = engine.ask(REPO, QUESTION, None, false).await.unwrap();
    assert_eq!(answer.vectors_skipped, None, "served while it is adopted");
    repo.held.found.lock().unwrap().clear();
    let answer = engine.ask(REPO, QUESTION, None, false).await.unwrap();
    assert_eq!(answer.vectors_skipped, None);
    assert!(engine.vectors_suspected().lock().unwrap().is_empty());

    adopted(&repo.filler()).await;
    engine.ask(REPO, QUESTION, None, false).await.unwrap();
    assert!(engine.vectors_suspected().lock().unwrap().contains(REPO));
}

/// A page mended writes only what the store lacks as it is: a record held
/// right with an earlier version beside it costs no embed, and a record
/// that moved is searchable throughout, written before its old place goes.
#[tokio::test]
async fn a_page_with_an_earlier_version_beside_a_right_record_embeds_nothing() {
    let repo = Repo::prebuilt(12).await;
    let (entities, _) = repo.as_held().await;
    let named = |name: &str| entities.iter().find(|e| e.name == name).unwrap().clone();
    let beside = VectorCodeEntity {
        start_line: named("thing_0").start_line + 40,
        ..named("thing_0")
    };
    let moved = named("thing_2");
    repo.held.hold(&[beside]);
    repo.held_otherwise(&["thing_1"]);
    repo.held
        .entities
        .lock()
        .unwrap()
        .remove(&entity_key(&moved));
    repo.held.hold(&[VectorCodeEntity {
        start_line: moved.start_line + 5,
        ..moved.clone()
    }]);
    repo.held.hold_stale("gone::long_ago");
    *repo.held.kept.lock().unwrap() = Some(moved.entity_id.clone());

    adopted(&repo.filler()).await;
    assert!(repo.held_as_graph().await);
    assert_eq!(repo.held.embedded(), 2, "the changed one and the moved one");
    assert!(
        !repo.held.unkept.load(Ordering::SeqCst),
        "the moved one was held at every write"
    );
    assert!(repo.complete().await);
}

/// An adoption stopped between a page's write and its record goes on from
/// where it was recorded: the page written is found agreeing, and nothing
/// is embedded twice or compared again.
#[tokio::test]
async fn an_adoption_stopped_mid_way_goes_on_without_embedding_twice() {
    let repo = Repo::prebuilt(400).await;
    let mut ids = repo.graph_ids().await;
    ids.sort();
    let (entities, _) = repo.as_held().await;
    let changed: Vec<&str> = ids
        .chunks(100)
        .map(|page| {
            let entity = entities.iter().find(|e| e.entity_id == page[0]).unwrap();
            entity.name.as_str()
        })
        .collect();
    repo.held_otherwise(&changed);
    *repo.held.park_after.lock().unwrap() = Some(3);
    let filler = repo.filler();
    let stopped = async {
        loop {
            filler.step(REPO).await.unwrap();
        }
    };
    let stopped = tokio::time::timeout(std::time::Duration::from_secs(2), stopped).await;
    assert!(stopped.is_err(), "parked on the third page's write");
    assert_eq!(repo.held.embedded(), 3);

    adopted(&repo.filler()).await;
    assert!(repo.held_as_graph().await);
    assert_eq!(repo.held.embedded(), changed.len(), "each once");
    let compared = repo.held.compared_from.lock().unwrap().clone();
    for page in ids.chunks(100) {
        let times = compared.iter().filter(|first| **first == page[0]).count();
        assert_eq!(times, 2, "compared, then read again or gone on with");
    }
}

/// A page whose graph changes while it is mended is mended again until it
/// agrees; one that never stops changing fails, keeping its place.
#[tokio::test]
async fn a_page_changed_while_it_is_mended_ends_as_the_graph_has_it() {
    let repo = Repo::prebuilt(12).await;
    repo.held_otherwise(&["thing_3"]);
    let entity = repo.graph.all_entities(REPO).await.unwrap();
    let thing = entity.iter().find(|e| e.name == "thing_5").unwrap();
    let changed = workforce_codebase_knowledge::graph::entities::CodeEntity {
        doc_comment: Some("Does thing 5, and more.".into()),
        ..thing.clone()
    };
    *repo.held.crowd.lock().unwrap() = vec![changed];
    adopted(&repo.filler()).await;
    assert!(repo.held_as_graph().await);

    let repo = Repo::prebuilt(12).await;
    repo.held_otherwise(&["thing_3"]);
    let entity = repo.graph.all_entities(REPO).await.unwrap();
    *repo.held.restless.lock().unwrap() = entity.iter().find(|e| e.name == "thing_5").cloned();
    let filler = repo.filler();
    let begun = filler.step(REPO).await.unwrap_err().to_string();
    assert!(
        begun.contains("kept changing while they were adopted"),
        "{begun}"
    );
    let fill = repo.graph.vector_fill(REPO).await.unwrap().unwrap();
    assert_eq!(
        (fill.stage, fill.cursor.as_str()),
        (FillStage::AdoptEntities, "")
    );
}

/// A store holding nothing of the repo is filled as it always was.
#[tokio::test]
async fn an_empty_store_is_filled_as_before() {
    let repo = Repo::new();
    repo.index().await;
    let stages = adopted(&repo.filler()).await;
    assert!(stages.iter().all(|stage| !stage.adopting()), "{stages:?}");
    assert!(stages.contains(&FillStage::Sweep));
    assert_eq!(repo.held.embedded(), repo.graph_ids().await.len());
    assert!(repo.complete().await && repo.agrees().await);
}

/// A full index during an adoption drops it; the next one finds what was
/// mended agreeing, and embeds only what still differed.
#[tokio::test]
async fn a_full_index_during_an_adoption_begins_it_again_and_embeds_nothing_twice() {
    let repo = Repo::prebuilt(250).await;
    let mut ids = repo.graph_ids().await;
    ids.sort();
    let (entities, _) = repo.as_held().await;
    let changed: Vec<&str> = ids
        .chunks(100)
        .map(|page| {
            let entity = entities.iter().find(|e| e.entity_id == page[0]).unwrap();
            entity.name.as_str()
        })
        .collect();
    repo.held_otherwise(&changed);
    let filler = repo.filler();
    filler.step(REPO).await.unwrap();
    filler.step(REPO).await.unwrap();
    repo.index().await;
    assert_eq!(repo.graph.vector_fill(REPO).await.unwrap(), None);
    let stages = adopted(&filler).await;
    assert_eq!(stages[0], FillStage::AdoptEntities, "adopted again");
    assert!(repo.held_as_graph().await);
    assert_eq!(repo.held.embedded(), changed.len());
}

/// An operator's refill during an adoption fills, and sweeps.
#[tokio::test]
async fn forcing_a_refill_during_an_adoption_fills() {
    let repo = Repo::prebuilt(12).await;
    repo.held.hold_stale("gone::long_ago");
    let filler = repo.filler();
    assert!(filler.step(REPO).await.unwrap().stage.adopting());
    let forced = filler.begin(REPO, false).await.unwrap();
    assert_eq!(forced.stage, FillStage::Entities);
    let stages = adopted(&filler).await;
    assert!(stages.contains(&FillStage::Sweep), "{stages:?}");
    assert!(stages.iter().all(|stage| !stage.adopting()));
    assert!(repo.agrees().await);
    assert_eq!(repo.held.generations(), [forced.generation]);
}

/// Records an index run deletes after their page was compared leave the
/// store holding fewer than were compared: that too has what it holds
/// gone through, and a stray found.
#[tokio::test]
async fn the_stray_rule_walks_when_a_run_deletes_what_was_compared() {
    let repo = Repo::prebuilt(12).await;
    repo.held.hold_stale("gone::long_ago");
    let filler = repo.filler();
    filler.step(REPO).await.unwrap();
    for file in ["src/config.rs", "src/mail.rs"] {
        repo.graph.delete_file_entities(REPO, file).await.unwrap();
        repo.store()
            .delete_entities_by_file(REPO, file)
            .await
            .unwrap();
    }
    let stages = adopted(&filler).await;
    assert!(
        stages.contains(&FillStage::AdoptStrayEntities),
        "{stages:?}"
    );
    assert!(repo.held_as_graph().await, "the stray removed");
}

/// A record an index run writes back while it is removed as a stray is
/// noted, and written back by the settle after the adoption.
#[tokio::test]
async fn a_key_added_while_strays_are_removed_is_written_back() {
    let repo = Repo::prebuilt(12).await;
    let mail = repo
        .graph
        .find_entities_in_file(REPO, "src/mail.rs")
        .await
        .unwrap();
    repo.graph
        .delete_file_entities(REPO, "src/mail.rs")
        .await
        .unwrap();
    *repo.held.on_remove.lock().unwrap() = mail.clone();
    adopted(&repo.filler()).await;
    assert_eq!(repo.pending().await, ["src/mail.rs"]);
    assert_eq!(repo.filler().settle_pending(REPO).await.unwrap(), 0);
    assert!(repo.held.ids().contains(&mail[0].id.as_str().to_string()));
    assert!(repo.held_as_graph().await);
}

/// A pending file the store holds exactly as the graph has it is taken
/// off with nothing deleted or written; one it lacks a record of, holds
/// another hash of, or holds a record besides of, is written again.
#[tokio::test]
async fn a_pending_file_held_exactly_is_settled_without_a_write() {
    let repo = Repo::filled().await;
    let writes = || {
        (
            repo.held.file_purges.load(Ordering::SeqCst),
            repo.held.writes.load(Ordering::SeqCst),
        )
    };
    repo.graph
        .note_vectors_pending(REPO, &["src/many.rs".to_string()])
        .await
        .unwrap();
    let before = writes();
    assert_eq!(repo.filler().settle_pending(REPO).await.unwrap(), 0);
    assert_eq!(writes(), before, "nothing deleted or written");

    let (entities, _) = repo.as_held().await;
    let config = entities.iter().find(|e| e.name == "parse_config").unwrap();
    repo.held
        .entities
        .lock()
        .unwrap()
        .remove(&entity_key(config));
    repo.held_otherwise(&["send_email"]);
    let thing = entities.iter().find(|e| e.name == "thing_0").unwrap();
    repo.held.hold(&[VectorCodeEntity {
        entity_id: "besides".into(),
        ..thing.clone()
    }]);
    let files = ["src/config.rs", "src/mail.rs", "src/many.rs"].map(str::to_string);
    repo.graph.note_vectors_pending(REPO, &files).await.unwrap();
    let before = writes();
    assert_eq!(repo.filler().settle_pending(REPO).await.unwrap(), 0);
    assert_eq!(writes().0, before.0 + 6, "each file's purged");
    assert!(repo.held_as_graph().await, "the one besides is gone");
}

/// A store that keeps no track says it holds nothing of a file, so a file
/// the graph no longer has is purged from it, as any changed file is.
#[tokio::test]
async fn a_file_deleted_from_the_graph_is_purged_from_a_store_that_keeps_no_track() {
    let repo = Repo::new();
    repo.index().await;
    let store = Counting::new(false, Failing::Not);
    let filler = VectorFiller::new(Arc::clone(&repo.graph), store.clone());
    for file in ["src/gone.rs", "src/mail.rs"] {
        let before = store.purged();
        repo.graph
            .note_vectors_pending(REPO, &[file.to_string()])
            .await
            .unwrap();
        assert_eq!(filler.settle_pending(REPO).await.unwrap(), 0);
        assert_eq!(store.purged(), before + 2, "{file}");
    }
}

/// A file changed in the graph after its page was compared, and noted with
/// nothing written, as by a run whose lease was taken from it, is settled
/// after the adoption: written again, and only it.
#[tokio::test]
async fn a_file_changed_after_its_page_was_compared_is_settled_after_the_finish() {
    let repo = Repo::prebuilt(12).await;
    let filler = repo.filler();
    filler.step(REPO).await.unwrap();
    let mail = repo
        .graph
        .find_entities_in_file(REPO, "src/mail.rs")
        .await
        .unwrap();
    let changed: Vec<_> = mail
        .into_iter()
        .map(
            |entity| workforce_codebase_knowledge::graph::entities::CodeEntity {
                doc_comment: Some("Sends a message, and bounces.".into()),
                ..entity
            },
        )
        .collect();
    repo.graph.upsert_entities(&changed).await.unwrap();
    repo.graph
        .note_vectors_pending(REPO, &["src/mail.rs".to_string()])
        .await
        .unwrap();
    adopted(&filler).await;
    assert_eq!(
        repo.held.embedded(),
        0,
        "the adoption found its page agreeing"
    );
    assert_eq!(filler.settle_pending(REPO).await.unwrap(), 0);
    assert_eq!(repo.held.embedded(), changed.len());
    assert!(repo.held_as_graph().await);
}

/// During a fill that sweeps, a pending file held right but under an
/// earlier generation is written again, under the fill's: skipped, the
/// sweep would remove it.
#[tokio::test]
async fn a_file_settled_during_a_fill_is_rewritten_not_skipped() {
    let repo = Repo::filled().await;
    let before = repo.held.generations();
    let filler = repo.filler();
    let fill = filler.begin(REPO, true).await.unwrap();
    stepped_to(&filler, FillStage::Summaries).await;
    {
        let mut held = repo.held.entities.lock().unwrap();
        let mail = held.values_mut().find(|e| e.name == "send_email").unwrap();
        mail.generation = before[0].clone();
    }
    repo.graph
        .note_vectors_pending(REPO, &["src/mail.rs".to_string()])
        .await
        .unwrap();
    assert!(repo.complete().await, "kept complete, so settled meanwhile");
    assert_eq!(filler.settle_pending(REPO).await.unwrap(), 0);
    stepped_to(&filler, FillStage::Done).await;
    assert!(repo.agrees().await, "nothing swept that the graph has");
    assert_eq!(repo.held.generations(), [fill.generation]);
}
