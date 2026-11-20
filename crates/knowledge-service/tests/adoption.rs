//! Adopting a tenant of code vectors built elsewhere from a copy of the
//! graph: what the store already holds as the graph has it is kept, and
//! only what changed since the copy is embedded.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use metrics_util::debugging::{DebugValue, DebuggingRecorder};
use vellev_client::api::Page;
use vellev_core::CollectionName;
use vellev_core::Embedding;
use workforce_codebase_knowledge::graph::entities::{
    CodeEntity, CodeVisibility, EntityId, EntityKind, Language,
};
use workforce_codebase_knowledge::graph::vectors::FillStage;
use workforce_codebase_knowledge::graph::CodeGraph;
use workforce_codebase_knowledge::indexer::fill::{VectorFiller, Verified, Verifying};
use workforce_codebase_knowledge::lease::{AcquireOutcome, IndexPhase};
use workforce_codebase_knowledge::testing::{
    as_held, collection_keeping_text, counted, stepped, Counting, Failing, Held,
};
use workforce_codebase_knowledge::vector::{
    CodeVectorStore, VectorCodeEntity, VectorCodeSummary, VellevCodeVectorStore,
};
use workforce_common::settings::{CodegraphConfig, KnowledgeConfig};
use workforce_knowledge_service::{KnowledgeService, VectorBackend};
use workforce_vellev::testing::{logged, Counted, Node, SMALL};
use workforce_vellev::{embedding_of, LOCAL_QUERY_PREFIX};

const QUESTION: &str = "what does thing 3 do";

/// What the text of a record changed after the copy ends with.
const SINCE: &str = ", as changed after the copy";

/// A service on the graph kept in `dir`, its vectors in `store`, which
/// fills at no pace and compares each repo again after `reconcile_secs`.
fn service_on(
    dir: &Path,
    store: Arc<dyn CodeVectorStore>,
    reconcile_secs: u64,
) -> KnowledgeService {
    let config = KnowledgeConfig {
        vector_backfill_per_sec: 0,
        vector_reconcile_secs: reconcile_secs,
        ..KnowledgeConfig::default()
    };
    KnowledgeService::new_with_backend(
        dir.join("repo"),
        dir.join("data"),
        &config,
        VectorBackend::Given(store),
        &CodegraphConfig::default(),
        None,
        None,
    )
    .unwrap()
}

/// Whether the store holds the repo as the graph has it: as many records
/// as the graph has to embed, and a comparison of the two to the end
/// finding nothing differing or held besides.
async fn agrees(graph: &Arc<CodeGraph>, store: &Arc<dyn CodeVectorStore>, repo: &str) -> bool {
    let filler = VectorFiller::new(Arc::clone(graph), Arc::clone(store));
    let mut at = Verifying::default();
    let ended = loop {
        match filler.verify_step(repo, &mut at).await.unwrap() {
            Verified::Going => {}
            ended => break ended,
        }
    };
    filler.confirm(repo).await.unwrap().agrees()
        && matches!(
            ended,
            Verified::Done {
                differing: 0,
                strays: 0,
                ..
            }
        )
}

/// Whether the repo's vectors are recorded complete in the collection the
/// store has.
async fn complete(service: &KnowledgeService, repo: &str) -> bool {
    let store = service.vector_store().clone().unwrap();
    service.graph().vectors_complete_in(repo).await.unwrap()
        == Some(store.identity(false).await.unwrap())
}

/// Step the repo's fill until it is done, asking a question of it after
/// each step and saying why each answer was made without vectors.
async fn stepped_asking(
    service: &KnowledgeService,
    filler: &VectorFiller,
    repo: &str,
) -> Vec<Option<String>> {
    let skipped = std::sync::Mutex::new(Vec::new());
    stepped(filler, repo, || async {
        let answer = service
            .query_engine()
            .ask(repo, QUESTION, None, false)
            .await
            .unwrap();
        skipped.lock().unwrap().push(answer.vectors_skipped);
    })
    .await;
    skipped.into_inner().unwrap()
}

/// A repo of the fixture: its entities, in how many files, and how many
/// of them are summarised; and the files changed after the copy, by the
/// entities each holds, with how many summaries are among them.
struct Shape {
    repo: &'static str,
    entities: usize,
    files: usize,
    summaries: usize,
    changed: &'static [usize],
    changed_summaries: usize,
}

const ALPHA: Shape = Shape {
    repo: "alpha",
    entities: 2_000,
    files: 80,
    summaries: 100,
    changed: &[4; 15],
    changed_summaries: 4,
};

/// The three repos of plan N3.3: 5,000 entities and 200 summaries, of
/// which 140 entities and 10 summaries are in the 40 files changed after
/// the copy.
const SHAPES: [Shape; 3] = [
    ALPHA,
    Shape {
        repo: "beta",
        entities: 2_000,
        files: 80,
        summaries: 60,
        changed: &[3; 15],
        changed_summaries: 3,
    },
    Shape {
        repo: "gamma",
        entities: 1_000,
        files: 40,
        summaries: 40,
        changed: &[4, 4, 4, 4, 4, 3, 3, 3, 3, 3],
        changed_summaries: 3,
    },
];

/// One small repo, for what its size does not matter to.
const SMALL_REPO: [Shape; 1] = [Shape {
    repo: "small",
    entities: 60,
    files: 6,
    summaries: 10,
    changed: &[3, 2],
    changed_summaries: 2,
}];

impl Shape {
    fn file(&self, at: usize) -> String {
        format!("src/{}_{at:02}.rs", self.repo)
    }

    fn changed_files(&self) -> Vec<String> {
        (0..self.changed.len()).map(|at| self.file(at)).collect()
    }

    fn changed_entities(&self) -> usize {
        self.changed.iter().sum()
    }

    /// What changed after the copy: every entity and summary of the
    /// changed files.
    fn changed_records(&self) -> usize {
        self.changed_entities() + self.changed_summaries
    }

    /// The entities, those of the changed files first, each file's at
    /// lines of its own; `after` for their text after the copy.
    fn entities(&self, after: bool) -> Vec<CodeEntity> {
        let changed = self.changed.len();
        let rest = self.files - changed;
        let files = self
            .changed
            .iter()
            .enumerate()
            .flat_map(|(file, &held)| std::iter::repeat_n(file, held))
            .chain((0..self.entities - self.changed_entities()).map(|n| changed + n % rest));
        let mut lines: BTreeMap<usize, usize> = BTreeMap::new();
        files
            .enumerate()
            .map(|(n, file)| {
                let line = lines.entry(file).or_default();
                *line += 5;
                let name = format!("{}_thing_{n:04}", self.repo);
                let file_path = self.file(file);
                let since = if after && file < changed { SINCE } else { "" };
                CodeEntity {
                    id: EntityId(format!("{}::{file_path}::{name}", self.repo)),
                    repo_id: self.repo.to_string(),
                    kind: EntityKind::Function,
                    qualified_name: format!("{}::{name}", self.repo),
                    signature: Some(format!("pub fn {name}() -> usize")),
                    visibility: CodeVisibility::Public,
                    parent_id: None,
                    doc_comment: Some(format!("Does {} thing {n}{since}.", self.repo)),
                    content_hash: format!("{n}{since}"),
                    language: Language::Rust,
                    start_line: *line,
                    end_line: *line + 3,
                    file_path,
                    name,
                }
            })
            .collect()
    }

    /// The entities summarised: as many of the changed files' as are
    /// summarised, then of the others.
    fn summarised(&self, entities: &[CodeEntity]) -> Vec<CodeEntity> {
        let (changed, unchanged) = entities.split_at(self.changed_entities());
        changed[..self.changed_summaries]
            .iter()
            .chain(&unchanged[..self.summaries - self.changed_summaries])
            .cloned()
            .collect()
    }

    /// Write the repo into `graph` as it was at the copy, or `after` it,
    /// registered with its index state.
    async fn write(&self, graph: &CodeGraph, after: bool) {
        let entities = self.entities(after);
        graph
            .register_repo(self.repo, self.repo, &format!("/{}", self.repo))
            .await
            .unwrap();
        graph.upsert_entities(&entities).await.unwrap();
        let changed = self.changed_files();
        for entity in self.summarised(&entities) {
            let since = if after && changed.contains(&entity.file_path) {
                SINCE
            } else {
                ""
            };
            let text = format!("Summarises {}{since}.", entity.name);
            graph
                .upsert_summary(&entity.id, "entity", &text, &entity.content_hash, "terse")
                .await
                .unwrap();
        }
        graph
            .update_index_state_full(self.repo, "t0", entities.len(), 0)
            .await
            .unwrap();
    }
}

/// A store on `node`'s tenant whose vectors are made from each text's
/// hash, counted, embedding as `local` where it is said.
fn counted_as(node: &Node, local: Option<&Embedding>) -> (VellevCodeVectorStore, Arc<Counted>) {
    counted(&node.url, &node.key, &node.tenant, local)
}

/// Every object of the tenant's code collections, by id: its text's hash
/// and its fill.
async fn objects(node: &Node) -> BTreeMap<String, (String, String)> {
    let tenant = node.tenant();
    let mut held = BTreeMap::new();
    for name in ["CodeEntity", "CodeSummary"] {
        let records = tenant.collection(&CollectionName::new(name).unwrap());
        let mut page = Page::new(1_000);
        loop {
            let listed = records.list(&page).await.unwrap();
            for object in listed.objects {
                let properties = &object.properties;
                held.insert(
                    object.id.0.to_string(),
                    (
                        properties["textHash"].to_string(),
                        properties["fill"].to_string(),
                    ),
                );
            }
            match listed.next {
                Some(next) => page = Page::new(1_000).after(next),
                None => break,
            }
        }
    }
    held
}

/// A graph whose repos were copied, a tenant built from that copy, and
/// the graph changed since.
struct Fixture {
    dir: tempfile::TempDir,
    node: Node,
    shapes: &'static [Shape],
    /// How its stores embed, where not as the node's model does.
    local: Option<Embedding>,
}

impl Fixture {
    /// The graph written as at the copy and the tenant built from it, by a
    /// store claimed for the graph, or for `claimed_by`; then the graph
    /// changed, its changed files noted, and nothing recorded of the
    /// build in it.
    async fn built(shapes: &'static [Shape], claimed_by: Option<&str>) -> Option<Self> {
        let node = Node::embedding().await?;
        Some(Self::built_on(node, shapes, claimed_by, None).await)
    }

    async fn built_on(
        node: Node,
        shapes: &'static [Shape],
        claimed_by: Option<&str>,
        local: Option<Embedding>,
    ) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let (store, counted) = counted_as(&node, local.as_ref());
        let store = match claimed_by {
            Some(graph) => store.for_graph(graph),
            None => store,
        };
        let build = service_on(dir.path(), Arc::new(store), 21_600);
        let graph = build.graph();
        for shape in shapes {
            shape.write(graph, false).await;
        }
        let pass = build.backfill_vectors().await.unwrap();
        assert!(
            pass.failed.is_empty(),
            "{:?}",
            build.vector_status().await.unwrap()
        );
        let records: usize = shapes
            .iter()
            .map(|shape| shape.entities + shape.summaries)
            .sum();
        assert_eq!(counted.asked(), records, "the build embeds everything");
        for shape in shapes {
            graph.clear_vector_fill(shape.repo).await.unwrap();
            graph
                .set_vectors_complete_in(shape.repo, None)
                .await
                .unwrap();
            shape.write(graph, true).await;
            graph
                .note_vectors_pending(shape.repo, &shape.changed_files())
                .await
                .unwrap();
        }
        Self {
            dir,
            node,
            shapes,
            local,
        }
    }

    /// The service switched to the tenant, its store seeded by a counter
    /// of its own.
    fn switched(&self) -> (KnowledgeService, Arc<Counted>) {
        let (store, counted) = counted_as(&self.node, self.local.as_ref());
        (
            service_on(self.dir.path(), Arc::new(store), 21_600),
            counted,
        )
    }

    fn changed(&self) -> usize {
        self.shapes.iter().map(Shape::changed_records).sum()
    }
}

/// The switch embeds what changed after the copy and nothing else, and
/// every repo ends complete, nothing pending, as the graph has it.
#[tokio::test]
async fn the_switch_embeds_exactly_what_changed_after_the_copy() {
    let Some(fixture) = Fixture::built(&SHAPES, None).await else {
        return;
    };
    let (service, counted) = fixture.switched();
    let pass = service.backfill_vectors().await.unwrap();
    assert!(
        pass.failed.is_empty(),
        "{:?}",
        service.vector_status().await.unwrap()
    );
    assert_eq!(counted.asked(), fixture.changed());
    let store = service.vector_store().clone().unwrap();
    for shape in fixture.shapes {
        assert!(complete(&service, shape.repo).await, "{}", shape.repo);
        assert_eq!(
            service
                .graph()
                .count_vectors_pending(shape.repo)
                .await
                .unwrap(),
            0
        );
        assert!(
            agrees(service.graph(), &store, shape.repo).await,
            "{}",
            shape.repo
        );
    }
}

/// A tenant claimed by another graph is refused before anything is read or
/// written: nothing embedded, nothing of it changed, no fill begun.
#[tokio::test]
async fn a_tenant_claimed_by_another_graph_is_refused_untouched() {
    let Some(fixture) = Fixture::built(&SHAPES, Some("another-graph")).await else {
        return;
    };
    let before = objects(&fixture.node).await;
    let (service, counted) = fixture.switched();
    let pass = service.backfill_vectors().await.unwrap();
    let repos: Vec<&str> = fixture.shapes.iter().map(|shape| shape.repo).collect();
    assert_eq!(pass.failed, repos);
    for status in service.vector_status().await.unwrap() {
        let why = status.last_error.unwrap_or_default();
        assert!(why.contains("another code graph"), "{why}");
        assert_eq!(
            service.graph().vector_fill(&status.repo_id).await.unwrap(),
            None
        );
    }
    assert_eq!(counted.asked(), 0);
    assert_eq!(objects(&fixture.node).await, before);
}

/// A repo of which three tenths differ in the tenant has those three
/// tenths embedded, with what changed after the copy, and is served all
/// the while.
#[tokio::test]
async fn a_repo_three_tenths_changed_reembeds_three_tenths() {
    let Some(fixture) = Fixture::built(&SHAPES, None).await else {
        return;
    };
    let entities = ALPHA.entities(true);
    let unchanged = &entities[ALPHA.changed_entities()..];
    let rewritten: Vec<VectorCodeEntity> = unchanged[..600]
        .iter()
        .map(|entity| VectorCodeEntity {
            searchable_content: format!("{} as it was before", entity.name),
            ..VectorCodeEntity::from_code_entity(entity, ALPHA.repo, None)
        })
        .collect();
    let summaries = ALPHA.summarised(&entities)[ALPHA.changed_summaries..][..30]
        .iter()
        .map(|entity| VectorCodeSummary {
            entity_id: entity.id.as_str().to_string(),
            repo_id: ALPHA.repo.to_string(),
            granularity: "entity".into(),
            file_path: entity.file_path.clone(),
            summary_text: format!("Summarised {} otherwise.", entity.name),
            is_stale: false,
            generation: String::new(),
        })
        .collect::<Vec<_>>();
    let (behind, _) = counted_as(&fixture.node, None);
    behind.index_entities(&rewritten).await.unwrap();
    behind.index_summaries(&summaries).await.unwrap();

    let (service, counted) = fixture.switched();
    let store = service.vector_store().clone().unwrap();
    let filler = VectorFiller::new(Arc::clone(service.graph()), Arc::clone(&store));
    let skipped = stepped_asking(&service, &filler, ALPHA.repo).await;
    assert!(skipped.iter().all(Option::is_none), "{skipped:?}");
    assert_eq!(
        counted.asked(),
        rewritten.len() + summaries.len() + ALPHA.changed_records()
    );
    assert!(agrees(service.graph(), &store, ALPHA.repo).await);
}

/// A repo being adopted is searched by vectors from its first page on.
#[tokio::test]
async fn a_repo_being_adopted_is_searched() {
    let Some(fixture) = Fixture::built(&SHAPES, None).await else {
        return;
    };
    let (service, _) = fixture.switched();
    let store = service.vector_store().clone().unwrap();
    let filler = VectorFiller::new(Arc::clone(service.graph()), store);
    filler.step(ALPHA.repo).await.unwrap();
    let fill = filler.step(ALPHA.repo).await.unwrap();
    assert!(fill.stage.adopting() && fill.done > 0, "{fill:?}");
    let answer = service
        .query_engine()
        .ask(ALPHA.repo, QUESTION, None, false)
        .await
        .unwrap();
    assert_eq!(answer.vectors_skipped, None);
    assert!(!answer.sources.is_empty());
}

/// Both code collections dropped and made again at another model's size
/// under a store that had read them: its adoption is refused naming
/// both models, with nothing embedded, written or begun, and the pass
/// tells the operator.
#[tokio::test]
async fn an_adopted_collection_of_another_size_is_refused_with_nothing_written() {
    let Some(node) = Node::embedding().await else {
        return;
    };
    let small = embedding_of(SMALL, 384, Some(LOCAL_QUERY_PREFIX), &[]);
    let fixture = Fixture::built_on(node, &SMALL_REPO, None, Some(small)).await;
    let (service, counted) = fixture.switched();
    let store = service.vector_store().clone().unwrap();
    store.identity(true).await.unwrap();

    let tenant = fixture.node.tenant();
    for name in ["CodeEntity", "CodeSummary"] {
        tenant
            .collection(&CollectionName::new(name).unwrap())
            .drop()
            .await
            .unwrap();
    }
    let at_base =
        VellevCodeVectorStore::new(&fixture.node.url, &fixture.node.key, &fixture.node.tenant)
            .unwrap();
    let (entities, summaries) = as_held(service.graph(), "small").await;
    at_base.index_entities(&entities[..5]).await.unwrap();
    at_base.index_summaries(&summaries[..2]).await.unwrap();
    let before = objects(&fixture.node).await;

    let filler = VectorFiller::new(Arc::clone(service.graph()), Arc::clone(&store));
    let refused = format!("{:#}", filler.step("small").await.unwrap_err());
    for said in [
        "'CodeEntity'",
        "BAAI/bge-base-en-v1.5",
        SMALL,
        "768 dimensions, expected 384",
    ] {
        assert!(refused.contains(said), "{said}: {refused}");
    }
    assert_eq!(counted.asked(), 0);
    assert_eq!(objects(&fixture.node).await, before);
    assert_eq!(service.graph().vector_fill("small").await.unwrap(), None);

    let pass = service.backfill_vectors().await.unwrap();
    assert_eq!(pass.failed, ["small"]);
    let why = service.vector_status().await.unwrap()[0]
        .last_error
        .clone()
        .unwrap_or_default();
    assert!(why.contains("768 dimensions, expected 384"), "{why}");
}

/// The summaries' collection made again, empty, while the entities are
/// adopted, by a store whose looks the adopting store does not see: the
/// finish looks again, so the repo is never recorded complete in what
/// it was; it is adopted again in the new collection, and ends complete.
#[tokio::test]
async fn a_collection_replaced_during_an_adoption_is_never_recorded_complete() {
    let Some(fixture) = Fixture::built(&SMALL_REPO, None).await else {
        return;
    };
    let (service, _) = fixture.switched();
    let store = service.vector_store().clone().unwrap();
    let was = store.identity(true).await.unwrap();
    let filler = VectorFiller::new(Arc::clone(service.graph()), Arc::clone(&store));
    let mut fill = filler.step("small").await.unwrap();
    while matches!(
        fill.stage,
        FillStage::AdoptEntities | FillStage::AdoptStrayEntities
    ) {
        fill = filler.step("small").await.unwrap();
    }
    let summaries = CollectionName::new("CodeSummary").unwrap();
    fixture
        .node
        .tenant()
        .collection(&summaries)
        .drop()
        .await
        .unwrap();
    let (other, _) = counted_as(&fixture.node, None);
    let made = VectorCodeSummary {
        entity_id: "elsewhere".into(),
        repo_id: "elsewhere".into(),
        granularity: "entity".into(),
        file_path: "src/elsewhere.rs".into(),
        summary_text: "Makes the collection again.".into(),
        is_stale: false,
        generation: String::new(),
    };
    other.index_summaries(&[made]).await.unwrap();
    other.delete_repo_summaries("elsewhere").await.unwrap();

    let finished = loop {
        match filler.step("small").await {
            Ok(fill) => assert_ne!(
                fill.stage,
                FillStage::Done,
                "finished in the old collection"
            ),
            Err(e) => break format!("{e:#}"),
        }
    };
    assert!(finished.contains("made again meanwhile"), "{finished}");
    assert_eq!(service.graph().vector_fill("small").await.unwrap(), None);
    assert_ne!(
        service.graph().vectors_complete_in("small").await.unwrap(),
        Some(was.clone())
    );

    let mut stages = Vec::new();
    loop {
        let fill = filler.step("small").await.unwrap();
        stages.push(fill.stage);
        if fill.stage == FillStage::Done {
            break;
        }
    }
    assert!(
        stages
            .iter()
            .all(|stage| *stage == FillStage::Done || stage.adopting()),
        "{stages:?}"
    );
    let now = store.identity(true).await.unwrap();
    assert_ne!(now, was);
    assert_eq!(
        service.graph().vectors_complete_in("small").await.unwrap(),
        Some(now)
    );
    assert!(agrees(service.graph(), &store, "small").await);
}

/// A tenant built into the shape that keeps the entities' text is adopted
/// as any other, and its begin says so, though the store had looked at it
/// already.
#[tokio::test]
async fn an_adopted_collection_that_keeps_its_text_is_adopted_and_said_at_begin() {
    let Some(node) = Node::embedding().await else {
        return;
    };
    let earlier = collection_keeping_text();
    node.tenant()
        .collection(&earlier.name)
        .create(&earlier)
        .await
        .unwrap();
    let fixture = Fixture::built_on(node, &SMALL_REPO, None, None).await;
    let (service, counted) = fixture.switched();
    let store = service.vector_store().clone().unwrap();
    store.identity(true).await.unwrap();
    let filler = VectorFiller::new(Arc::clone(service.graph()), Arc::clone(&store));
    let logged = logged::during(async {
        let fill = filler.step("small").await.unwrap();
        assert!(fill.stage.adopting(), "{fill:?}");
    })
    .await;
    let warned: Vec<&str> = logged
        .lines()
        .filter(|line| line.contains("WARN") && line.contains("collection keeps text"))
        .collect();
    assert!(
        warned.len() == 1
            && warned[0].contains("CodeEntity")
            && warned[0].contains("searchableContent"),
        "{logged}"
    );
    service.backfill_vectors().await.unwrap();
    assert_eq!(counted.asked(), fixture.changed());
    assert!(complete(&service, "small").await);
    assert!(agrees(service.graph(), &store, "small").await);
}

/// What `snapshotter` has counted of the code vectors embedded, by why.
fn embedded_by_why(snapshotter: &metrics_util::debugging::Snapshotter) -> BTreeMap<String, u64> {
    snapshotter
        .snapshot()
        .into_vec()
        .into_iter()
        .filter(|(key, ..)| key.key().name() == "workforce.code_vectors.embedded_total")
        .filter_map(|(key, _, _, value)| {
            let labels: BTreeMap<String, String> = key
                .key()
                .labels()
                .map(|label| (label.key().to_string(), label.value().to_string()))
                .collect();
            assert_eq!(labels.get("backend").map(String::as_str), Some("vellev"));
            match value {
                DebugValue::Counter(count) => Some((labels["reason"].clone(), count)),
                _ => None,
            }
        })
        .collect()
}

/// Every record embedded whole is counted under why it was: the switch's
/// under `adopt`, a settled file's under `pending`, an index run's under
/// `index` and a fill's under `refill`; a record patched is not.
#[test]
fn embeds_are_counted_by_why() {
    let recorder = DebuggingRecorder::new();
    let snapshotter = recorder.snapshotter();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    metrics::with_local_recorder(&recorder, || {
        runtime.block_on(async {
            let Some(node) = Node::embedding().await else {
                return;
            };
            let fixture = Fixture::built_on(node, &SHAPES, None, None).await;
            let before = embedded_by_why(&snapshotter);
            let counted_since = |reason: &str| {
                embedded_by_why(&snapshotter)
                    .get(reason)
                    .copied()
                    .unwrap_or(0)
                    - before.get(reason).copied().unwrap_or(0)
            };
            let (service, counted) = fixture.switched();
            assert!(service.backfill_vectors().await.unwrap().failed.is_empty());
            assert_eq!(counted_since("adopt"), fixture.changed() as u64);
            assert_eq!((counted_since("pending"), counted_since("refill")), (0, 0));

            let store = service.vector_store().clone().unwrap();
            let graph = service.graph();
            let filler = VectorFiller::new(Arc::clone(graph), Arc::clone(&store));
            let gamma = &SHAPES[2];
            let (changed, unchanged, inline) = (gamma.file(5), gamma.file(6), gamma.file(7));
            let mut entities = graph
                .find_entities_in_file(gamma.repo, &changed)
                .await
                .unwrap();
            for entity in &mut entities {
                entity.doc_comment = Some("Does it otherwise.".into());
                entity.content_hash.push_str(" otherwise");
            }
            graph.upsert_entities(&entities).await.unwrap();
            for (file, records) in [(&changed, entities.len()), (&unchanged, 0)] {
                let pending = counted_since("pending");
                graph
                    .note_vectors_pending(gamma.repo, std::slice::from_ref(file))
                    .await
                    .unwrap();
                filler.settle_pending(gamma.repo).await.unwrap();
                assert_eq!(counted_since("pending") - pending, records as u64, "{file}");
            }

            let inline = graph
                .find_entities_in_file(gamma.repo, &inline)
                .await
                .unwrap();
            let inline: Vec<VectorCodeEntity> = inline[..2]
                .iter()
                .map(|entity| VectorCodeEntity {
                    doc_comment: "Changed inline.".into(),
                    ..VectorCodeEntity::from_code_entity(entity, gamma.repo, None)
                })
                .collect();
            store.index_entities(&inline).await.unwrap();
            assert_eq!(counted_since("index"), 2);

            let asked = counted.asked();
            filler.begin(gamma.repo, true).await.unwrap();
            while filler.step(gamma.repo).await.unwrap().stage != FillStage::Done {}
            let refilled = counted_since("refill");
            assert_eq!(refilled, (counted.asked() - asked) as u64);
            let records = (gamma.entities + gamma.summaries) as u64;
            assert!(
                refilled > 0 && refilled < records,
                "{refilled} of {records}"
            );
        })
    });
}

/// A repo indexed from files, its vectors kept in `store`: the fakes'
/// hub.
struct Hub {
    dir: tempfile::TempDir,
    root: PathBuf,
    repo: String,
    held: Arc<Held>,
}

impl Hub {
    /// A repo of two small files and one of `functions` functions,
    /// indexed, the store holding it as the graph has it, as one built
    /// elsewhere does.
    async fn prebuilt(functions: usize) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join("src/config.rs"),
            "/// Reads the configuration.\npub fn parse_config(path: &str) -> String {\n    path.to_string()\n}\n",
        )
        .unwrap();
        std::fs::write(
            root.join("src/mail.rs"),
            "/// Sends a message.\npub fn send_email(to: &str) -> bool {\n    !to.is_empty()\n}\n",
        )
        .unwrap();
        let many: String = (0..functions)
            .map(|n| {
                format!("/// Does thing {n}.\npub fn thing_{n}() -> usize {{\n    {n}\n}}\n\n")
            })
            .collect();
        std::fs::write(root.join("src/many.rs"), many).unwrap();
        let held = Arc::new(Held::default());
        let hub = Self {
            repo: KnowledgeService::repo_id_for(&root),
            dir,
            root,
            held,
        };
        let service = hub.service();
        *hub.held.graph.lock().unwrap() = Some(Arc::clone(service.graph()));
        service.trigger_full_index_for(&hub.root).await.unwrap();
        hub.held.entities.lock().unwrap().clear();
        hub.held.hold_graph(service.graph(), &hub.repo).await;
        hub
    }

    /// A service of the hub's, as a hub started again is.
    fn service(&self) -> KnowledgeService {
        self.service_comparing_after(21_600)
    }

    fn service_comparing_after(&self, reconcile_secs: u64) -> KnowledgeService {
        service_on(self.dir.path(), self.store(), reconcile_secs)
    }

    fn store(&self) -> Arc<dyn CodeVectorStore> {
        Arc::clone(&self.held) as Arc<dyn CodeVectorStore>
    }

    fn graph(&self) -> Arc<CodeGraph> {
        self.held.graph.lock().unwrap().clone().unwrap()
    }

    /// Change the entities named `names` in the store, as one holding an
    /// earlier text of them does.
    fn held_otherwise(&self, names: &[&str]) -> usize {
        let mut held = self.held.entities.lock().unwrap();
        let changed: Vec<_> = held
            .values_mut()
            .filter(|e| names.contains(&e.name.as_str()))
            .collect();
        let count = changed.len();
        for entity in changed {
            entity.searchable_content.push_str(" as it was");
        }
        count
    }

    /// A record held of `repo`, of no file the graph has.
    fn hold_of(&self, repo: &str, id: &str) {
        self.held.hold(&[VectorCodeEntity {
            entity_id: id.to_string(),
            repo_id: repo.to_string(),
            qualified_name: id.to_string(),
            name: id.to_string(),
            kind: "Function".into(),
            file_path: "src/elsewhere.rs".into(),
            language: "Rust".into(),
            signature: String::new(),
            doc_comment: String::new(),
            summary: String::new(),
            searchable_content: id.to_string(),
            start_line: 1,
            end_line: 2,
            generation: String::new(),
        }]);
    }

    fn holds_of(&self, repo: &str) -> bool {
        self.held
            .entities
            .lock()
            .unwrap()
            .values()
            .any(|e| e.repo_id == repo)
    }

    async fn agrees(&self) -> bool {
        agrees(&self.graph(), &self.store(), &self.repo).await
    }
}

/// An index run holds the adoption where it is; once it is done the pass
/// finishes it, and what the run noted meanwhile is settled after.
#[tokio::test]
async fn an_index_run_holds_the_adoption_until_it_is_done() {
    let hub = Hub::prebuilt(12).await;
    let service = hub.service();
    let graph = hub.graph();
    let filler = VectorFiller::new(Arc::clone(&graph), hub.store());
    let begun = filler.step(&hub.repo).await.unwrap();
    assert!(begun.stage.adopting());
    let leased = graph
        .acquire_lease(&hub.repo, "another-hub", None, IndexPhase::Parsing)
        .await
        .unwrap();
    assert!(matches!(leased, AcquireOutcome::Acquired));
    let mut mail = graph
        .find_entities_in_file(&hub.repo, "src/mail.rs")
        .await
        .unwrap();
    mail[0].doc_comment = Some("Sends a message, and bounces.".into());
    graph.upsert_entities(&mail).await.unwrap();
    graph
        .note_vectors_pending(&hub.repo, &["src/mail.rs".to_string()])
        .await
        .unwrap();

    let pass = service.backfill_vectors().await.unwrap();
    assert_eq!(pass.failed, std::slice::from_ref(&hub.repo));
    let why = service.vector_status().await.unwrap()[0]
        .last_error
        .clone()
        .unwrap_or_default();
    assert!(why.contains("being indexed"), "{why}");
    assert_eq!(graph.vector_fill(&hub.repo).await.unwrap(), Some(begun));

    graph.release_lease(&hub.repo, "another-hub").await.unwrap();
    let pass = service.backfill_vectors().await.unwrap();
    assert_eq!(pass.filled, std::slice::from_ref(&hub.repo));
    assert!(complete(&service, &hub.repo).await);
    assert_eq!(graph.count_vectors_pending(&hub.repo).await.unwrap(), 0);
    assert!(hub.agrees().await);
}

/// A misrouted store is found so before anything is read or written: no
/// adoption begins.
#[tokio::test]
async fn a_misrouted_store_begins_no_adoption() {
    let hub = Hub::prebuilt(12).await;
    let store = Counting::new(true, Failing::Misrouted);
    let service = service_on(hub.dir.path(), store.clone(), 21_600);
    let pass = service.backfill_vectors().await.unwrap();
    assert_eq!(pass.failed, std::slice::from_ref(&hub.repo));
    let why = service.vector_status().await.unwrap()[0]
        .last_error
        .clone()
        .unwrap_or_default();
    assert!(why.contains("misrouted"), "{why}");
    assert_eq!(service.graph().vector_fill(&hub.repo).await.unwrap(), None);
    assert_eq!(store.changed(), 0, "nothing written or deleted");
}

/// Two workers adopting one repo at once end with it complete and as the
/// graph has it, and embed no more than a page twice.
#[tokio::test(flavor = "multi_thread")]
async fn two_workers_adopting_one_repo_end_agreeing() {
    let hub = Hub::prebuilt(250).await;
    let names: Vec<String> = (0..250).step_by(20).map(|n| format!("thing_{n}")).collect();
    let names: Vec<&str> = names.iter().map(String::as_str).collect();
    let differing = hub.held_otherwise(&names);
    let (one, two) = (hub.service(), hub.service());
    let (first, second) = tokio::join!(one.backfill_vectors(), two.backfill_vectors());
    let (first, second) = (first.unwrap(), second.unwrap());
    if !(first.failed.is_empty() && second.failed.is_empty()) {
        assert!(one.backfill_vectors().await.unwrap().failed.is_empty());
    }
    assert!(complete(&one, &hub.repo).await);
    assert!(hub.agrees().await);
    let embedded = hub.held.embedded();
    assert!(
        embedded >= differing && embedded <= differing + 100,
        "{embedded} of {differing}"
    );
}

/// What the store holds of a repo the graph no longer has is removed by
/// the first pass, and only then; one registered meanwhile is left while
/// the rest are swept, and a record of no repo is left too. A sweep that
/// fails leaves the pass to go on, and the next sweeps.
#[tokio::test]
async fn a_dropped_repos_vectors_are_swept_once_and_a_new_one_kept() {
    let hub = Hub::prebuilt(12).await;
    hub.hold_of("dropped", "dropped::one");
    hub.hold_of("", "of-no-repo");
    let service = hub.service();
    assert!(service.backfill_vectors().await.unwrap().failed.is_empty());
    assert!(!hub.holds_of("dropped"));
    assert!(hub.holds_of(""), "a record of no repo is never swept");
    assert!(complete(&service, &hub.repo).await);
    hub.hold_of("dropped", "dropped::again");
    service.backfill_vectors().await.unwrap();
    assert!(hub.holds_of("dropped"), "swept once for the collection");

    hub.held
        .entities
        .lock()
        .unwrap()
        .retain(|_, e| e.repo_id != "dropped");
    hub.hold_of("early", "early::one");
    hub.hold_of("late", "late::one");
    *hub.held.registered_meanwhile.lock().unwrap() = Some("early".into());
    hub.service().backfill_vectors().await.unwrap();
    assert!(hub.holds_of("early"), "registered meanwhile");
    assert!(!hub.holds_of("late"), "swept after the one registered");

    let hub = Hub::prebuilt(12).await;
    hub.hold_of("dropped", "dropped::one");
    hub.held
        .fail_held_outside
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let service = hub.service();
    let logged = logged::during(async {
        assert!(service.backfill_vectors().await.unwrap().failed.is_empty());
    })
    .await;
    assert!(
        logged
            .lines()
            .any(|line| line.contains("WARN") && line.contains("no longer has were not removed")),
        "{logged}"
    );
    assert!(hub.holds_of("dropped"));
    assert!(complete(&service, &hub.repo).await, "adopted all the same");
    service.backfill_vectors().await.unwrap();
    assert!(!hub.holds_of("dropped"));
}

/// A graph that lists no repo sweeps nothing, and is swept once it lists
/// its repos again.
#[tokio::test]
async fn a_graph_listing_no_repo_sweeps_nothing() {
    let hub = Hub::prebuilt(12).await;
    hub.hold_of("dropped", "dropped::one");
    hub.graph().drop_repo(&hub.repo).await.unwrap();
    let service = hub.service();
    service.backfill_vectors().await.unwrap();
    assert!(hub.holds_of("dropped") && hub.holds_of(&hub.repo));
    service.trigger_full_index_for(&hub.root).await.unwrap();
    service.backfill_vectors().await.unwrap();
    assert!(!hub.holds_of("dropped") && hub.holds_of(&hub.repo));
}

/// A repo of which the store holds half, as inline writes before its
/// first fill leave it, is adopted and served from what is there, and
/// ends with only the half it lacked embedded.
#[tokio::test]
async fn a_repo_held_in_part_is_adopted_and_served() {
    let hub = Hub::prebuilt(20).await;
    let lacked = {
        let mut held = hub.held.entities.lock().unwrap();
        let keys: Vec<String> = held.keys().step_by(2).cloned().collect();
        keys.iter().for_each(|key| {
            held.remove(key);
        });
        keys.len()
    };
    let service = hub.service();
    let filler = VectorFiller::new(hub.graph(), hub.store());
    let skipped = stepped_asking(&service, &filler, &hub.repo).await;
    assert!(skipped.iter().all(Option::is_none), "{skipped:?}");
    let fill = hub.graph().vector_fill(&hub.repo).await.unwrap().unwrap();
    assert_eq!(fill.done, fill.total, "n of m ends at m");
    assert!(complete(&service, &hub.repo).await);
    assert_eq!(hub.held.embedded(), lacked);
    assert!(hub.agrees().await);
}

/// An adoption is the comparison the switch is due: none follows it until
/// the repo is due one again.
#[tokio::test]
async fn an_adoption_needs_no_compare_after_it() {
    let hub = Hub::prebuilt(12).await;
    let service = hub.service_comparing_after(1);
    assert!(service.backfill_vectors().await.unwrap().failed.is_empty());
    let compared = || hub.held.compared_from.lock().unwrap().len();
    let adopted = compared();
    assert!(adopted > 0);
    assert!(service.backfill_vectors().await.unwrap().filled.is_empty());
    assert_eq!(compared(), adopted, "not compared again");
    tokio::time::sleep(Duration::from_millis(1_100)).await;
    service.backfill_vectors().await.unwrap();
    assert!(compared() > adopted, "compared in its time");
}
