use std::collections::BTreeMap;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use eyre::{eyre, Result};
use workforce_codebase_knowledge::graph::entities::{
    CodeEntity, CodeVisibility, EntityId, EntityKind, Language,
};
use workforce_codebase_knowledge::graph::CodeGraph;
use workforce_codebase_knowledge::testing::{entity_key, Held, Hook};
use workforce_codebase_knowledge::vector::{
    CodeVectorStore, Near, NearestTo, Scanned, SearchedByVector, VectorCodeEntity,
    VectorSearchResult, VellevCodeVectorStore,
};
use workforce_common::settings::{VellevCodeConfig, WorkforceConfig};
use workforce_vellev::seeding::DocumentEmbedder;
use workforce_vellev::testing::{Counted, Node, NotServing};

use super::stats::{self, Applying, Form};
use super::*;
use sha2::{Digest, Sha256};

const PREFIX: &str = "query: ";

fn entity(repo: &str, name: &str, kind: EntityKind, doc: Option<&str>) -> CodeEntity {
    CodeEntity {
        id: EntityId(format!("{repo}-{name}")),
        repo_id: repo.to_string(),
        kind,
        name: name.to_string(),
        qualified_name: format!("lib::{name}"),
        file_path: format!("src/{name}.rs"),
        start_line: 1,
        end_line: 3,
        signature: None,
        visibility: CodeVisibility::Public,
        parent_id: None,
        doc_comment: doc.map(str::to_string),
        content_hash: "hash".to_string(),
        language: Language::Rust,
    }
}

/// `count` functions of `repo`, each documented in words of its own.
fn functions(repo: &str, count: usize) -> Vec<CodeEntity> {
    (0..count)
        .map(|n| {
            let name = format!("f{n:05}");
            let doc = format!("Does the {name} work of {repo}. And more besides.");
            entity(repo, &name, EntityKind::Function, Some(&doc))
        })
        .collect()
}

async fn graph_of(entities: &[CodeEntity]) -> CodeGraph {
    let graph = CodeGraph::open_in_memory().unwrap();
    let repos: std::collections::BTreeSet<&str> = entities
        .iter()
        .map(|entity| entity.repo_id.as_str())
        .collect();
    for repo in repos {
        graph
            .register_repo(repo, repo, &format!("/{repo}"))
            .await
            .unwrap();
    }
    graph.upsert_entities(entities).await.unwrap();
    graph
}

/// Each question an entity is asked without a summary.
fn texts(entity: &CodeEntity) -> Vec<String> {
    std::iter::once(entity.name.clone())
        .chain(
            entity
                .doc_comment
                .as_deref()
                .and_then(stats::first_sentence),
        )
        .collect()
}

fn result(entity: &CodeEntity) -> VectorSearchResult {
    VectorSearchResult {
        entity_id: entity.id.0.clone(),
        repo_id: entity.repo_id.clone(),
        qualified_name: entity.qualified_name.clone(),
        kind: format!("{:?}", entity.kind),
        file_path: entity.file_path.clone(),
        start_line: entity.start_line as u32,
        end_line: entity.end_line as u32,
        score: 0.9,
    }
}

fn nears(found: &[&CodeEntity]) -> Vec<Near> {
    found
        .iter()
        .enumerate()
        .map(|(at, entity)| Near {
            object: format!("o-{}", entity.id.0),
            distance: 0.1 + at as f32 * 0.01,
            found: result(entity),
        })
        .collect()
}

/// What a store answers each question of `entities` with: what `found`
/// finds for its entity.
fn answering_each<'a, T: Clone>(
    entities: &'a [CodeEntity],
    found: impl Fn(&'a CodeEntity) -> Vec<&'a CodeEntity>,
    answer: impl Fn(&[&CodeEntity]) -> T,
) -> BTreeMap<(String, String), T> {
    entities
        .iter()
        .flat_map(|entity| {
            let answered = answer(&found(entity));
            texts(entity)
                .into_iter()
                .map(move |text| ((entity.repo_id.clone(), text), answered.clone()))
                .collect::<Vec<_>>()
        })
        .collect()
}

/// A Weaviate holding `entities` and answering `answers`.
fn holding(
    answers: BTreeMap<(String, String), Vec<VectorSearchResult>>,
    entities: &[CodeEntity],
) -> Held {
    let held = Held {
        answers: Mutex::new(answers),
        ..Held::default()
    };
    held.entities
        .lock()
        .unwrap()
        .extend(entities.iter().map(|entity| {
            let held = VectorCodeEntity::from_code_entity(entity, &entity.repo_id, None);
            (entity_key(&held), held)
        }));
    held
}

fn weaviate_of<'a>(
    entities: &'a [CodeEntity],
    found: impl Fn(&'a CodeEntity) -> Vec<&'a CodeEntity>,
) -> Held {
    let answers = answering_each(entities, found, |found| {
        found.iter().map(|entity| result(entity)).collect()
    });
    holding(answers, entities)
}

/// A fake of vellev's search: answers by text from a map, a vector by the
/// text it was made of (with the prefix, as that text; without, the same
/// entities as other objects), and an exact scan as its index answers.
#[derive(Default)]
struct Searched {
    prefix: Option<String>,
    by_text: BTreeMap<(String, String), Vec<Near>>,
    held: BTreeMap<String, usize>,
    made: Mutex<std::collections::HashMap<Vec<u32>, String>>,
    asked: Mutex<Vec<(&'static str, String, usize)>>,
    failing_vector: Option<usize>,
}

fn bits(vector: &[f32]) -> Vec<u32> {
    vector.iter().map(|x| x.to_bits()).collect()
}

impl Searched {
    fn of<'a>(
        entities: &'a [CodeEntity],
        found: impl Fn(&'a CodeEntity) -> Vec<&'a CodeEntity>,
    ) -> Self {
        let held = entities.iter().fold(BTreeMap::new(), |mut held, entity| {
            *held.entry(entity.repo_id.clone()).or_insert(0) += 1;
            held
        });
        Self {
            prefix: Some(PREFIX.to_string()),
            by_text: answering_each(entities, found, nears),
            held,
            ..Self::default()
        }
    }

    fn asked(&self) -> Vec<(&'static str, String, usize)> {
        self.asked.lock().unwrap().clone()
    }

    fn answer(&self, repo: &str, text: &str) -> Vec<Near> {
        self.by_text
            .get(&(repo.to_string(), text.to_string()))
            .cloned()
            .unwrap_or_default()
    }

    fn by_vector(&self, vector: &[f32], repo: &str) -> Result<Vec<Near>> {
        let written = self
            .made
            .lock()
            .unwrap()
            .get(&bits(vector))
            .cloned()
            .ok_or_else(|| eyre!("a vector the node never made"))?;
        Ok(match &self.prefix {
            None => self.answer(repo, &written),
            Some(prefix) => match written.strip_prefix(prefix.as_str()) {
                Some(text) => self.answer(repo, text),
                None => self
                    .answer(repo, &written)
                    .into_iter()
                    .map(|near| Near {
                        object: format!("{}-plain", near.object),
                        ..near
                    })
                    .collect(),
            },
        })
    }
}

#[async_trait]
impl SearchedByVector for Searched {
    fn query_prefix(&self) -> Option<&str> {
        self.prefix.as_deref()
    }

    async fn embedded(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        let vectors = Counted::new(8).documents(texts).await?;
        let mut made = self.made.lock().unwrap();
        for (text, vector) in texts.iter().zip(&vectors) {
            made.insert(bits(vector), text.clone());
        }
        Ok(vectors)
    }

    async fn nearest(&self, to: NearestTo<'_>, repo_id: &str, limit: usize) -> Result<Vec<Near>> {
        let how = match to {
            NearestTo::Text(_) => "text",
            NearestTo::Vector(_) => "vector",
        };
        let vectors_asked = {
            let mut asked = self.asked.lock().unwrap();
            asked.push((how, repo_id.to_string(), limit));
            asked.iter().filter(|(how, ..)| *how == "vector").count()
        };
        match to {
            NearestTo::Text(text) => Ok(self.answer(repo_id, text)),
            NearestTo::Vector(_) if self.failing_vector == Some(vectors_asked) => {
                Err(eyre!("the node went away"))
            }
            NearestTo::Vector(vector) => self.by_vector(vector, repo_id),
        }
    }

    async fn nearest_exactly(
        &self,
        asked: &[(&[f32], Option<&str>)],
        repo_id: &str,
        limit: usize,
    ) -> Result<Scanned> {
        let mut scanned = Scanned {
            held: self.held.get(repo_id).copied().unwrap_or_default(),
            ..Scanned::default()
        };
        for (vector, first) in asked {
            let found = self.by_vector(vector, repo_id)?;
            let at = found
                .iter()
                .find(|near| Some(near.object.as_str()) == *first);
            scanned.named.push(at.map(|near| near.distance));
            scanned
                .nearest
                .push(found.into_iter().take(limit).collect());
        }
        Ok(scanned)
    }

    async fn counted(&self, repo_id: &str) -> Result<usize> {
        Ok(self.held.get(repo_id).copied().unwrap_or_default())
    }
}

/// A node's work as a script gives it, by the call counted from 0.
struct Counters {
    calls: AtomicUsize,
    script: Box<dyn Fn(usize) -> Result<Scored> + Send + Sync>,
}

impl Counters {
    fn new(script: impl Fn(usize) -> Result<Scored> + Send + Sync + 'static) -> Self {
        Self {
            calls: AtomicUsize::new(0),
            script: Box::new(script),
        }
    }

    /// One search between each read and the next, each estimating rows.
    fn shortlisting() -> Self {
        Self::new(|call| {
            let searches = call.div_ceil(2) as u64;
            Ok(Scored {
                searches,
                exactly: 0,
                estimated: searches * 100,
            })
        })
    }
}

#[async_trait]
impl Worked for Counters {
    async fn scored(&self) -> Result<Scored> {
        (self.script)(self.calls.fetch_add(1, Ordering::SeqCst))
    }
}

fn options(out: &Path) -> CompareOptions {
    CompareOptions {
        seed: 7,
        entities: NonZeroUsize::new(40).unwrap(),
        recall_queries: 5,
        same_queries: 10,
        resamples: NonZeroUsize::new(200).unwrap(),
        weaviate_url: "http://localhost:8080".to_string(),
        node_metrics: "http://localhost:9090/metrics".to_string(),
        no_prefix: false,
        out: out.to_path_buf(),
    }
}

fn manifest(graph: &CodeGraph, options: &CompareOptions, measured: &Measured) -> Manifest {
    Manifest {
        version: "test".to_string(),
        started: "start".to_string(),
        ended: "end".to_string(),
        options: options.clone(),
        limit: LIMIT,
        depth: DEPTH,
        graph: GraphAbout {
            identity: graph.identity().to_string(),
            backend: "sqlite".to_string(),
            sha256: String::new(),
        },
        repos: measured.repos.clone(),
        vellev: VellevAbout {
            url: "http://localhost:8081".to_string(),
            tenant: "code".to_string(),
            claim: graph.identity().to_string(),
            identity_start: "e/s".to_string(),
            identity_end: "e/s".to_string(),
            model: "a model".to_string(),
            dimensions: 8,
            query_prefix: Some(PREFIX.to_string()),
            distance: Distance::Cosine,
        },
        written_alone: true,
    }
}

/// A run on fakes, recorded into `options.out`.
async fn ran(
    graph: &CodeGraph,
    weaviate: &dyn CodeVectorStore,
    vellev: &dyn SearchedByVector,
    work: &dyn Worked,
    options: &CompareOptions,
) -> Result<Run> {
    recorded(&options.out, async {
        let measured = measured(graph, weaviate, vellev, work, options, &|_| {}).await?;
        let manifest = manifest(graph, options, &measured);
        Ok((measured, manifest))
    })
    .await
}

fn partial(out: &Path) -> std::path::PathBuf {
    placed(out).unwrap().1
}

fn first(entity: &CodeEntity) -> Vec<&CodeEntity> {
    vec![entity]
}

fn read(dir: &Path, name: &str) -> String {
    std::fs::read_to_string(dir.join(name)).unwrap()
}

fn rows<T: serde::de::DeserializeOwned>(dir: &Path, name: &str) -> Vec<T> {
    read(dir, name)
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn gate<'a>(run: &'a Run, name: &str) -> &'a Gate {
    run.report
        .findings
        .gates
        .iter()
        .find(|gate| gate.name == name)
        .unwrap()
}

/// A seed gives the same questions each time and another seed others, of
/// the entities of the lowest keys.
#[tokio::test]
async fn the_sample_depends_on_the_seed_alone() {
    let entities = [functions("a", 30), functions("b", 30)].concat();
    let graph = graph_of(&entities).await;
    let questions = |sample: Sample| -> String {
        sample
            .targets
            .iter()
            .flat_map(|target| &target.queries)
            .map(|query| serde_json::to_string(query).unwrap())
            .collect::<Vec<_>>()
            .join("\n")
    };
    let one = questions(sampled(&graph, 1, 10).await.unwrap());
    assert_eq!(one, questions(sampled(&graph, 1, 10).await.unwrap()));
    assert_ne!(one, questions(sampled(&graph, 2, 10).await.unwrap()));
    assert_eq!(one.lines().count(), 20);

    let taken: Vec<(String, String)> = sampled(&graph, 1, 10)
        .await
        .unwrap()
        .targets
        .iter()
        .map(|target| (target.entity.repo_id.clone(), target.entity.id.0.clone()))
        .collect();
    let mut keyed: Vec<([u8; 32], (String, String))> = entities
        .iter()
        .map(|entity| {
            let key = stats::key(1, "sample", &[&entity.repo_id, &entity.id.0]);
            (key, (entity.repo_id.clone(), entity.id.0.clone()))
        })
        .collect();
    keyed.sort();
    let mut lowest: Vec<(String, String)> =
        keyed.into_iter().take(10).map(|(_, taken)| taken).collect();
    lowest.sort();
    assert_eq!(taken, lowest);
}

/// Entities with nothing to embed are left out and counted, a doc with
/// nothing to embed is never asked, a fresh summary is asked at twice the
/// weight, a stale one is not and is counted, and a struct's summary is
/// never asked.
#[tokio::test]
async fn only_vectorizable_texts_and_fresh_summaries_are_asked() {
    let blank = CodeEntity {
        qualified_name: "--".to_string(),
        ..entity("r", "--", EntityKind::Function, None)
    };
    let dashes = entity("r", "dashed", EntityKind::Function, Some("---"));
    let fresh = entity("r", "fresh", EntityKind::Method, None);
    let stale = entity("r", "stale", EntityKind::Method, None);
    let shape = entity("r", "Shape", EntityKind::Struct, None);
    let graph = graph_of(&[
        blank.clone(),
        dashes.clone(),
        fresh.clone(),
        stale.clone(),
        shape.clone(),
    ])
    .await;
    for (of, granularity) in [
        (&fresh, "function"),
        (&stale, "function"),
        (&shape, "function"),
        (&shape, "file"),
    ] {
        graph
            .upsert_summary(
                &of.id,
                granularity,
                "Summarises it. Then more.",
                "hash",
                "m",
            )
            .await
            .unwrap();
    }
    graph
        .mark_summaries_stale(std::slice::from_ref(&stale.id))
        .await
        .unwrap();

    let sample = sampled(&graph, 3, 100).await.unwrap();
    let population = &sample.population;
    assert_eq!(population.excluded.get(NOTHING_TO_EMBED), Some(&1));
    assert_eq!(population.summary_stale, 1);
    let asked: BTreeMap<String, Vec<(Form, String, f64)>> = sample
        .targets
        .iter()
        .map(|target| {
            let queries = target
                .queries
                .iter()
                .map(|query| (query.form, query.text.clone(), query.weight))
                .collect();
            (target.entity.name.clone(), queries)
        })
        .collect();
    assert!(!asked.contains_key("--"));
    assert_eq!(asked["dashed"], [(Form::Name, "dashed".to_string(), 1.0)]);
    assert_eq!(
        asked["fresh"],
        [
            (Form::Name, "fresh".to_string(), 1.0),
            (Form::Summary, "Summarises it.".to_string(), 2.0)
        ]
    );
    assert_eq!(asked["stale"], [(Form::Name, "stale".to_string(), 1.0)]);
    assert_eq!(asked["Shape"], [(Form::Name, "Shape".to_string(), 1.0)]);
}

/// The bootstrap draws entities, each with all its questions: one entity
/// whose three questions differ has every replicate at its own mean.
#[tokio::test]
async fn resampling_draws_entities_not_queries() {
    let target = entity("r", "parse", EntityKind::Function, Some("Reads a file."));
    let graph = graph_of(std::slice::from_ref(&target)).await;
    let summary = "Parses the configuration.";
    graph
        .upsert_summary(&target.id, "function", summary, "hash", "m")
        .await
        .unwrap();
    let text = |text: &str| ("r".to_string(), text.to_string());
    let weaviate = holding(
        BTreeMap::from([
            (text("Reads a file."), vec![result(&target)]),
            (text(summary), vec![result(&target)]),
        ]),
        std::slice::from_ref(&target),
    );
    let vellev = Searched {
        prefix: Some(PREFIX.to_string()),
        by_text: BTreeMap::from([
            (text("parse"), nears(&[&target])),
            (text("Reads a file."), nears(&[&target])),
        ]),
        held: BTreeMap::from([("r".to_string(), 1)]),
        ..Searched::default()
    };
    let dir = tempfile::tempdir().unwrap();
    let run = ran(
        &graph,
        &weaviate,
        &vellev,
        &Counters::shortlisting(),
        &options(&dir.path().join("run")),
    )
    .await
    .unwrap();
    let mean = (1.0 + 0.0 - 2.0) / (1.0 + 1.0 + 2.0);
    let overall = run.report.findings.check1.overall;
    assert_eq!(
        (overall.d, overall.lower, overall.upper),
        (mean, mean, mean)
    );
}

fn answer(entity: &CodeEntity) -> Answer {
    Answer::from(result(entity))
}

fn named(entity: &CodeEntity, qualified_name: &str) -> Answer {
    let mut answer = answer(entity);
    answer.found.qualified_name = qualified_name.to_string();
    answer
}

/// A target and its graph, for judging answers.
async fn judging() -> (CodeGraph, Target, Vec<CodeEntity>) {
    let target = CodeEntity {
        qualified_name: "lib::A::new".to_string(),
        ..entity("r", "new", EntityKind::Method, Some("Makes an A."))
    };
    let others = functions("r", 12);
    let graph = graph_of(&[vec![target.clone()], others.clone()].concat()).await;
    let target = Target {
        text: record_text(&target),
        entity: target,
        queries: Vec::new(),
    };
    (graph, target, others)
}

/// Both stores are judged on the entity the product shows: a sibling the
/// graph dropped is shown as its survivor, a moved line is the same
/// entity, an entity of the target's text is a hit, an unheld one is not,
/// and a repeat frees a judged slot but keeps its place as it came.
#[tokio::test]
async fn a_hit_is_judged_on_the_entity_the_product_shows() {
    let (graph, target, others) = judging().await;
    let mut judge = Judge {
        graph: &graph,
        held: HashMap::new(),
    };
    let sibling = named(&target.entity, "lib::B::new");

    let weaviate = judge
        .side(&target, vec![sibling.clone()], true)
        .await
        .unwrap();
    assert!(weaviate.hit);
    assert!(!weaviate.hit_raw);
    assert_eq!(weaviate.siblings, Some(vec![1]));
    assert_eq!(weaviate.moved, 0);

    let mut moved = answer(&target.entity);
    moved.found.start_line = 40;
    let vellev = judge.side(&target, vec![moved], false).await.unwrap();
    assert!(vellev.hit && vellev.hit_raw);
    assert_eq!(vellev.siblings, None);
    assert_eq!(vellev.moved, 1);

    let twin = CodeEntity {
        id: EntityId("r-twin".to_string()),
        ..target.entity.clone()
    };
    graph.upsert_entity(&twin).await.unwrap();
    judge.held.clear();
    let by_text = judge
        .side(&target, vec![answer(&twin)], false)
        .await
        .unwrap();
    assert!(by_text.hit && by_text.hit_raw);

    let unheld = entity("r", "gone", EntityKind::Function, None);
    let missed = judge
        .side(&target, vec![answer(&unheld)], false)
        .await
        .unwrap();
    assert!(!missed.hit && !missed.hit_raw);

    let repeated: Vec<Answer> = std::iter::repeat_n(answer(&others[0]), 2)
        .chain(others[1..9].iter().map(answer))
        .chain(std::iter::once(answer(&target.entity)))
        .collect();
    assert_eq!(repeated[10], answer(&target.entity));
    let freed = judge.side(&target, repeated, false).await.unwrap();
    assert!(freed.hit);
    assert!(!freed.hit_raw);

    let mut found = Vec::new();
    for answers in [vec![sibling], vec![answer(&target.entity)]] {
        found.push(judge.side(&target, answers, true).await.unwrap());
    }
    let share = found
        .iter()
        .filter(|side| side.siblings.as_ref().is_some_and(|at| !at.is_empty()))
        .count();
    assert_eq!(share, 1);
}

/// The judged ten are ten distinct entities the graph holds: a sibling
/// and its survivor are one, an unheld result is none.
#[tokio::test]
async fn the_judged_ten_are_distinct_entities_the_graph_holds() {
    let (graph, target, others) = judging().await;
    let mut judge = Judge {
        graph: &graph,
        held: HashMap::new(),
    };
    let survivor = &others[0];
    let sibling = named(survivor, "lib::B::f00000");
    let unheld = answer(&entity("r", "gone", EntityKind::Function, None));
    let results: Vec<Answer> = [sibling, answer(survivor), unheld]
        .into_iter()
        .chain(others[1..8].iter().map(answer))
        .chain(std::iter::once(answer(&target.entity)))
        .collect();
    assert_eq!(results.len(), 11);
    let side = judge.side(&target, results, true).await.unwrap();
    assert!(side.hit);
    assert!(!side.hit_raw);

    let eleventh: Vec<Answer> = others[..10]
        .iter()
        .map(answer)
        .chain(std::iter::once(answer(&target.entity)))
        .collect();
    assert!(!judge.side(&target, eleventh, false).await.unwrap().hit);
}

/// No line of the comparison asks code search the way the product's
/// answers are made.
#[test]
fn compare_never_asks_the_query_engine() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/code_compare");
    let needles = [
        concat!("Query", "Engine"),
        concat!("query", "_engine"),
        concat!("Knowledge", "Service"),
    ];
    let itself = concat!("fn compare_never_asks_the_", "query", "_engine()");
    let found: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .flat_map(|path| {
            std::fs::read_to_string(&path)
                .unwrap()
                .lines()
                .map(str::trim_start)
                .filter(|line| !line.starts_with("//") && !line.starts_with(itself))
                .filter(|line| needles.iter().any(|needle| line.contains(needle)))
                .map(|line| format!("{}: {line}", path.display()))
                .collect::<Vec<_>>()
        })
        .collect();
    assert!(found.is_empty(), "{found:#?}");
}

/// Every search is the product's: one repo, 20 asked, 10 judged.
#[tokio::test]
async fn every_search_is_the_products() {
    let entities = functions("r", 12);
    let graph = graph_of(&entities).await;
    let (weaviate, vellev) = (
        weaviate_of(&entities, first),
        Searched::of(&entities, first),
    );
    let dir = tempfile::tempdir().unwrap();
    let run = ran(
        &graph,
        &weaviate,
        &vellev,
        &Counters::shortlisting(),
        &options(&dir.path().join("run")),
    )
    .await
    .unwrap();
    let searches = weaviate.searches.lock().unwrap().clone();
    assert_eq!(searches.len(), 24);
    assert!(searches
        .iter()
        .all(|search| search.repo_id.as_deref() == Some("r") && search.limit == 20));
    let asked = vellev.asked();
    assert_eq!(asked.len(), 24 * 3);
    assert!(asked
        .iter()
        .all(|(_, repo, limit)| repo == "r" && *limit == 20));
    assert_eq!(DEPTH, 10);
    assert!(run.passed().is_ok(), "{:?}", run.report.findings.gates);
}

fn holds_no_result(out: &Path) {
    assert!(!out.exists());
    assert!(!partial(out).join(SUMS).exists());
}

/// A store's failure fails the run, naming the side and the question, with
/// nothing that could pass for a result, and nothing asked after it.
#[tokio::test]
async fn a_store_error_fails_the_run_with_no_result() {
    let entities = functions("r", 12);
    let graph = graph_of(&entities).await;
    let dir = tempfile::tempdir().unwrap();
    let ids: Vec<String> = sampled(&graph, 7, 40)
        .await
        .unwrap()
        .targets
        .iter()
        .flat_map(|target| target.queries.iter().map(|query| query.id.clone()))
        .collect();

    let out = dir.path().join("vector");
    let weaviate = weaviate_of(&entities, first);
    let vellev = Searched {
        failing_vector: Some(3),
        ..Searched::of(&entities, first)
    };
    let failed = ran(
        &graph,
        &weaviate,
        &vellev,
        &Counters::shortlisting(),
        &options(&out),
    )
    .await
    .unwrap_err();
    let failed = format!("{failed:#}");
    assert!(
        failed.contains(&format!("vellev vector failed on query {}", ids[1])),
        "{failed}"
    );
    assert_eq!(weaviate.searches.lock().unwrap().len(), 2);
    holds_no_result(&out);

    let out = dir.path().join("weaviate");
    let weaviate = weaviate_of(&entities, first);
    *weaviate.failing_search.lock().unwrap() = Some(2);
    let failed = ran(
        &graph,
        &weaviate,
        &Searched::of(&entities, first),
        &Counters::shortlisting(),
        &options(&out),
    )
    .await
    .unwrap_err();
    let failed = format!("{failed:#}");
    assert!(
        failed.contains(&format!("weaviate failed on query {}", ids[1])),
        "{failed}"
    );
    holds_no_result(&out);

    let big = functions("big", SHORTLIST_ABOVE + 1);
    let graph = graph_of(&big).await;
    for (name, counters) in [
        ("metrics", Counters::new(|_| Err(eyre!("no metrics")))),
        (
            "doubled",
            Counters::new(|call| {
                Ok(Scored {
                    searches: 2 * call as u64,
                    ..Scored::default()
                })
            }),
        ),
    ] {
        let out = dir.path().join(name);
        let failed = ran(
            &graph,
            &weaviate_of(&big, first),
            &Searched::of(&big, first),
            &counters,
            &options(&out),
        )
        .await
        .unwrap_err();
        let failed = format!("{failed:#}");
        assert!(failed.contains("node metrics failed on query"), "{failed}");
        holds_no_result(&out);
    }
}

/// A node that serves another tenant fails the run after one request,
/// with the misroute's own words, before anything is sampled.
#[tokio::test]
async fn a_misrouted_node_fails_before_the_sample() {
    let node = NotServing::up().await;
    let vellev = VellevCodeVectorStore::new(&node.url, "key", "code").unwrap();
    let entities = functions("r", 3);
    let opened = Opened {
        graph: graph_of(&entities).await,
        about: GraphAbout {
            identity: "graph".to_string(),
            backend: "sqlite".to_string(),
            sha256: String::new(),
        },
    };
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("run");
    let weaviate = weaviate_of(&entities, first);
    let failed = compared(
        &opened,
        &weaviate,
        &vellev,
        &Counters::shortlisting(),
        &options(&out),
        &|_| {},
    )
    .await
    .unwrap_err();
    assert!(
        format!("{failed:#}").contains(&node.misrouted("code").to_string()),
        "{failed:#}"
    );
    assert_eq!(node.asked(), 1);
    assert!(weaviate.searches.lock().unwrap().is_empty());
    assert!(!out.exists() && !partial(&out).exists());
}

/// An unhealthy Weaviate is refused before the node is asked anything.
#[tokio::test]
async fn an_unhealthy_weaviate_is_refused_before_the_node() {
    let node = NotServing::up().await;
    let vellev = VellevCodeVectorStore::new(&node.url, "key", "code").unwrap();
    let entities = functions("r", 3);
    let opened = Opened {
        graph: graph_of(&entities).await,
        about: GraphAbout {
            identity: "graph".to_string(),
            backend: "sqlite".to_string(),
            sha256: String::new(),
        },
    };
    let weaviate = weaviate_of(&entities, first);
    weaviate.unhealthy.store(true, Ordering::SeqCst);
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("run");
    let failed = compared(
        &opened,
        &weaviate,
        &vellev,
        &Counters::shortlisting(),
        &options(&out),
        &|_| {},
    )
    .await
    .unwrap_err()
    .to_string();
    assert_eq!(failed, "Weaviate at memory is not healthy");
    assert_eq!(node.asked(), 0);
    assert!(!out.exists() && !partial(&out).exists());
}

/// A repo one store holds nothing of misses on that side, and its count
/// in the manifest shows the store holds none of it.
#[tokio::test]
async fn an_empty_answer_is_a_miss_not_an_error() {
    let entities = [functions("a", 10), functions("b", 10)].concat();
    let graph = graph_of(&entities).await;
    let weaviate = weaviate_of(&entities[..10], first);
    let dir = tempfile::tempdir().unwrap();
    let run = ran(
        &graph,
        &weaviate,
        &Searched::of(&entities, first),
        &Counters::shortlisting(),
        &options(&dir.path().join("run")),
    )
    .await
    .unwrap();
    let answers: Vec<Answers> = rows(&run.dir, "answers.jsonl");
    let of_b: Vec<&Answers> = answers
        .iter()
        .filter(|one| one.id.starts_with("b/"))
        .collect();
    assert_eq!(of_b.len(), 20);
    assert!(of_b
        .iter()
        .all(|one| one.weaviate.results.is_empty() && !one.weaviate.hit && one.vellev.hit));
    let counts = run.report.manifest.repos["b"];
    assert_eq!((counts.graph, counts.weaviate, counts.vellev), (10, 0, 10));
    assert_eq!(run.report.findings.check1.weaviate, 0.5);
}

/// The sums are of the files as they lie on disk, in coreutils' format;
/// and an existing out, or one left half-made, is refused.
#[tokio::test]
async fn the_files_are_summed_as_they_lie_on_disk() {
    let entities = functions("r", 12);
    let graph = graph_of(&entities).await;
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("run");
    let run = || {
        let (weaviate, vellev) = (
            weaviate_of(&entities, first),
            Searched::of(&entities, first),
        );
        let (graph, out) = (&graph, out.clone());
        async move {
            ran(
                graph,
                &weaviate,
                &vellev,
                &Counters::shortlisting(),
                &options(&out),
            )
            .await
        }
    };
    run().await.unwrap();
    let listed = read(&out, SUMS);
    let recomputed: String = FILES
        .iter()
        .map(|name| {
            let bytes = std::fs::read(out.join(name)).unwrap();
            format!("{}  {name}\n", hex::encode(Sha256::digest(&bytes)))
        })
        .collect();
    assert_eq!(listed, recomputed);

    let before = std::fs::read(out.join(SUMS)).unwrap();
    let refused = format!("{:#}", run().await.unwrap_err());
    assert!(refused.contains("exists"), "{refused}");
    assert_eq!(std::fs::read(out.join(SUMS)).unwrap(), before);

    let half = dir.path().join("half");
    std::fs::create_dir(partial(&half)).unwrap();
    let refused = refused_if_there(&half).unwrap_err().to_string();
    assert!(refused.contains("half.partial exists"), "{refused}");
    assert!(partial(&half).exists() && !half.exists());
}

/// An out ending in a separator is moved into as the directory it names,
/// one with no name is refused, and one made while the run measured is
/// never written over.
#[tokio::test]
async fn the_run_is_moved_into_out_alone() {
    let entities = functions("r", 12);
    let graph = graph_of(&entities).await;
    let dir = tempfile::tempdir().unwrap();
    let run = |out: std::path::PathBuf, meanwhile: bool| {
        let (weaviate, vellev) = (
            weaviate_of(&entities, first),
            Searched::of(&entities, first),
        );
        let graph = &graph;
        async move {
            let options = options(&out);
            recorded(&out, async {
                let measured = measured(
                    graph,
                    &weaviate,
                    &vellev,
                    &Counters::shortlisting(),
                    &options,
                    &|_| {},
                )
                .await?;
                if meanwhile {
                    std::fs::create_dir(&out)?;
                }
                let manifest = manifest(graph, &options, &measured);
                Ok((measured, manifest))
            })
            .await
        }
    };

    let ran = run(dir.path().join("slashed").join(""), false)
        .await
        .unwrap();
    assert_eq!(ran.dir, dir.path().join("slashed"));
    assert!(ran.dir.join(SUMS).is_file());
    assert!(!dir.path().join("slashed.partial").exists());

    let refused = refused_if_there(Path::new("..")).unwrap_err().to_string();
    assert!(refused.contains("names no directory"), "{refused}");

    let out = dir.path().join("meanwhile");
    let failed = format!("{:#}", run(out.clone(), true).await.unwrap_err());
    assert!(failed.contains("never writes over"), "{failed}");
    assert_eq!(std::fs::read_dir(&out).unwrap().count(), 0);
}

/// A graph with nothing to ask fails the overall gate, saying so.
#[tokio::test]
async fn a_run_that_asks_nothing_fails() {
    let graph = graph_of(&[]).await;
    let dir = tempfile::tempdir().unwrap();
    let run = ran(
        &graph,
        &weaviate_of(&[], first),
        &Searched::of(&[], first),
        &Counters::shortlisting(),
        &options(&dir.path().join("run")),
    )
    .await
    .unwrap();
    let overall = gate(&run, "overall");
    assert!(!overall.passed);
    assert_eq!(overall.said, "no query was asked");
    assert_eq!(run.report.findings.verdict, "fail");
}

/// vellev missing a twentieth of the targets Weaviate finds fails the
/// overall gate, and the run is still whole and summed.
#[tokio::test]
async fn a_failed_gate_still_writes_a_complete_run() {
    let entities = functions("r", 40);
    let lost = 2;
    let graph = graph_of(&entities).await;
    let vellev = Searched::of(&entities, |entity| match entity.name.as_str() {
        "f00000" | "f00001" => Vec::new(),
        _ => vec![entity],
    });
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("run");
    let run = ran(
        &graph,
        &weaviate_of(&entities, first),
        &vellev,
        &Counters::shortlisting(),
        &options(&out),
    )
    .await
    .unwrap();
    let d = run.report.findings.check1.overall.d;
    assert!(
        (d - -(lost as f64 / entities.len() as f64)).abs() < 1e-12,
        "{d}"
    );
    assert_eq!(run.report.findings.verdict, "fail");
    assert!(!gate(&run, "overall").passed);
    assert_eq!(read(&out, SUMS).lines().count(), FILES.len());
    let failed = run.passed().unwrap_err().to_string();
    assert!(failed.contains("overall"), "{failed}");
}

/// The same seed on the same fakes gives the same files, byte for byte,
/// check 2's included.
#[tokio::test]
async fn a_run_is_reproduced_by_its_seed() {
    let small = ["s1", "s2", "s3", "s4", "s5", "s6", "s7", "s8"].map(|repo| functions(repo, 3));
    let entities = [vec![functions("big", SHORTLIST_ABOVE + 1)], small.to_vec()]
        .concat()
        .concat();
    let graph = graph_of(&entities).await;
    let dir = tempfile::tempdir().unwrap();
    let mut runs = Vec::new();
    let out = dir.path().join("run");
    for name in ["one", "two"] {
        let run = ran(
            &graph,
            &weaviate_of(&entities, first),
            &Searched::of(&entities, first),
            &Counters::shortlisting(),
            &options(&out),
        )
        .await
        .unwrap();
        let kept = dir.path().join(name);
        std::fs::rename(&out, &kept).unwrap();
        runs.push((kept, run));
    }
    for name in FILES {
        assert_eq!(read(&runs[0].0, name), read(&runs[1].0, name), "{name}");
    }
    let recall: Vec<Recalled> = rows(&runs[0].0, "recall.jsonl");
    assert_eq!(recall.len(), 5);
    assert!(recall
        .iter()
        .all(|row| row.shortlisted && row.recall == 1.0 && row.self_check));
    let check2 = &runs[0].1.report.findings.check2;
    assert_eq!(check2.shortlisted, 5);
    assert_eq!(runs[0].1.report.findings.sample.repos.len(), 9);
    assert!(gate(&runs[0].1, "recall").passed && gate(&runs[0].1, "applied").passed);
}

/// A graph whose write-ahead log holds pages is refused, naming it, and
/// left as it was; with the log empty, the manifest's hash is the file's,
/// which opening it to read leaves as it was.
#[tokio::test]
async fn a_graph_with_pending_wal_pages_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = WorkforceConfig::default();
    config.knowledge.data_dir = Some(dir.path().to_string_lossy().to_string());
    let path = workforce_knowledge_service::code_graph_path(dir.path());
    let graph = CodeGraph::open(&path).unwrap();
    graph.register_repo("r", "r", "/r").await.unwrap();
    let settled = rusqlite::Connection::open(&path).unwrap();
    let indexed = || -> bool {
        settled
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE name = 'idx_entities_repo_id'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap()
            == 1
    };
    for _ in 0..500 {
        if indexed() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(indexed(), "the graph's own index is built behind it");
    drop(graph);
    settled
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
        .unwrap();
    drop(settled);
    let wal = PathBuf::from(format!("{}-wal", path.display()));
    std::fs::write(&wal, b"pending").unwrap();
    let graph_bytes = std::fs::read(&path).unwrap();
    let refused = opened_graph(&config, None).err().unwrap().to_string();
    assert!(refused.contains(&wal.display().to_string()), "{refused}");
    assert_eq!(std::fs::read(&path).unwrap(), graph_bytes);
    assert_eq!(std::fs::read(&wal).unwrap(), b"pending");

    std::fs::write(&wal, b"").unwrap();
    let hash = hex::encode(Sha256::digest(&graph_bytes));
    let opened = opened_graph(&config, None).unwrap();
    assert_eq!(opened.about.sha256, hash);
    assert_eq!(opened.about.identity, opened.graph.identity());
    drop(opened);
    assert_eq!(std::fs::read(&path).unwrap(), graph_bytes, "read only");
}

/// A code graph on Postgres is refused before it is opened, with nothing
/// made for it on disk.
#[test]
fn a_graph_not_on_sqlite_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = WorkforceConfig::default();
    config.knowledge.data_dir = Some(dir.path().to_string_lossy().to_string());
    config.codegraph.backend = "postgres".into();
    let refused = opened_graph(&config, None).err().unwrap().to_string();
    assert!(
        refused
            .starts_with("[codegraph] backend = \"postgres\": a run measures a SQLite code graph"),
        "{refused}"
    );
    assert!(!dir.path().join(".workforce").exists(), "nothing made");
}

/// The config of a store on `node`'s tenant embedding with bge-small, and
/// `prefix` before its questions.
fn configured(node: &Node, prefix: &str) -> WorkforceConfig {
    let mut config = WorkforceConfig::default();
    config.knowledge.vector_backend = "vellev".into();
    config.vellev.url = node.url.clone();
    config.vellev.api_key = Some(node.key.clone());
    config.vellev.tenant = node.tenant.clone();
    *config.vellev.code = VellevCodeConfig {
        model: Some(workforce_vellev::testing::SMALL.into()),
        dimensions: Some(384),
        query_prefix: Some(prefix.into()),
        ..Default::default()
    };
    config
}

/// Code a node can be asked about in words.
fn documented() -> Vec<CodeEntity> {
    [
        (
            "parse_config",
            "Reads the TOML configuration file from disk.",
        ),
        ("send_email", "Sends a message to an address over SMTP."),
        ("resize_image", "Scales a picture to fit the given width."),
        ("hash_password", "Derives a salted hash of a secret."),
        ("open_socket", "Connects to a server on a TCP port."),
        ("sort_rows", "Orders the table's rows by a column."),
        ("render_page", "Turns a template into HTML for the browser."),
        ("retry_request", "Asks again after a failed network call."),
        ("compress_file", "Packs a file into a smaller gzip archive."),
        ("parse_date", "Reads a calendar date written as text."),
        ("count_words", "Tallies how often each word appears."),
        ("draw_chart", "Plots a series of numbers as a line."),
    ]
    .iter()
    .map(|(name, doc)| entity("repo", name, EntityKind::Function, Some(doc)))
    .collect()
}

/// `entities` in a graph, their vectors in `config`'s tenant kept for it
/// as a fill keeps them, and the graph as a run opens it.
async fn built(config: &WorkforceConfig, entities: &[CodeEntity]) -> Opened {
    let graph = graph_of(entities).await;
    let filler = VellevCodeVectorStore::configured(&config.vellev)
        .unwrap()
        .for_graph(graph.identity());
    filler.ensure_collections().await.unwrap();
    let vectors: Vec<VectorCodeEntity> = entities
        .iter()
        .map(|entity| VectorCodeEntity::from_code_entity(entity, &entity.repo_id, None))
        .collect();
    filler.index_entities(&vectors).await.unwrap();
    Opened {
        about: GraphAbout {
            identity: graph.identity().to_string(),
            backend: "sqlite".to_string(),
            sha256: String::new(),
        },
        graph,
    }
}

/// The names of `node`'s tenant's collections, with their schemas and counts.
async fn held(node: &Node) -> Vec<(String, serde_json::Value, u64)> {
    let tenant = node.tenant();
    let mut held = Vec::new();
    for collection in tenant.collections().await.unwrap() {
        let records = tenant.collection(&collection.name);
        held.push((
            collection.name.as_str().to_string(),
            serde_json::to_value(records.schema().await.unwrap()).unwrap(),
            records.count(None).await.unwrap(),
        ));
    }
    held.sort_by(|a, b| a.0.cmp(&b.0));
    held
}

/// A collection dropped from under a run fails it, naming vellev, and is
/// never made again.
#[tokio::test]
async fn a_collection_dropped_during_a_run_is_never_made_again() {
    let Some(node) = Node::embedding().await else {
        return;
    };
    let config = configured(&node, workforce_vellev::LOCAL_QUERY_PREFIX);
    let entities = documented();
    let opened = built(&config, &entities).await;
    let tenant = node.tenant();
    let dropping: Hook = Arc::new(move || {
        let tenant = tenant.clone();
        Box::pin(async move {
            for collection in tenant.collections().await.unwrap() {
                if collection.name.as_str() == "CodeEntity" {
                    tenant.collection(&collection.name).drop().await.unwrap();
                }
            }
        })
    });
    let weaviate = weaviate_of(&entities, first);
    *weaviate.after_search.lock().unwrap() = Some((2, dropping));
    let vellev = VellevCodeVectorStore::configured(&config.vellev).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let failed = compared(
        &opened,
        &weaviate,
        &vellev,
        &Counters::shortlisting(),
        &options(&dir.path().join("run")),
        &|_| {},
    )
    .await
    .unwrap_err();
    let failed = format!("{failed:#}");
    assert!(failed.contains("vellev failed on query"), "{failed}");
    assert!(held(&node)
        .await
        .iter()
        .all(|(name, ..)| name != "CodeEntity"));
}

/// A code collection replaced while a run measures fails it at the end,
/// naming both identities.
#[tokio::test]
async fn a_collection_replaced_during_a_run_fails_it() {
    let Some(node) = Node::embedding().await else {
        return;
    };
    let config = configured(&node, workforce_vellev::LOCAL_QUERY_PREFIX);
    let entities = documented();
    let opened = built(&config, &entities).await;
    let vellev = VellevCodeVectorStore::configured(&config.vellev).unwrap();
    let before = vellev.found_as_wanted().await.unwrap();
    let (tenant, wanted, graph) = (
        node.tenant(),
        config.vellev.clone(),
        opened.graph.identity().to_string(),
    );
    let replacing: Hook = Arc::new(move || {
        let (tenant, wanted, graph) = (tenant.clone(), wanted.clone(), graph.clone());
        Box::pin(async move {
            for collection in tenant.collections().await.unwrap() {
                if collection.name.as_str() == "CodeSummary" {
                    tenant.collection(&collection.name).drop().await.unwrap();
                }
            }
            VellevCodeVectorStore::configured(&wanted)
                .unwrap()
                .for_graph(&graph)
                .ensure_collections()
                .await
                .unwrap();
        })
    });
    let weaviate = weaviate_of(&entities, first);
    *weaviate.after_search.lock().unwrap() = Some((2, replacing));
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("run");
    let failed = compared(
        &opened,
        &weaviate,
        &vellev,
        &Counters::shortlisting(),
        &options(&out),
        &|_| {},
    )
    .await
    .unwrap_err()
    .to_string();
    let after = vellev.found_as_wanted().await.unwrap();
    assert_ne!(after, before);
    assert!(
        failed.contains("collection replaced during the run")
            && failed.contains(&before)
            && failed.contains(&after),
        "{failed}"
    );
    assert!(!out.exists() && !partial(&out).join(SUMS).exists());
}

/// A tenant claimed for another graph, or for none, is refused before the
/// sample, and left as it was.
#[tokio::test]
async fn a_tenant_claimed_for_another_graph_is_refused() {
    let Some(node) = Node::embedding().await else {
        return;
    };
    let config = configured(&node, workforce_vellev::LOCAL_QUERY_PREFIX);
    let entities = documented();
    let opened = built(&config, &entities).await;
    let other = Opened {
        graph: graph_of(&entities).await,
        about: opened.about.clone(),
    };
    let vellev = VellevCodeVectorStore::configured(&config.vellev).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let before = held(&node).await;
    let weaviate = weaviate_of(&entities, first);
    let refused = compared(
        &other,
        &weaviate,
        &vellev,
        &Counters::shortlisting(),
        &options(&dir.path().join("run")),
        &|_| {},
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(
        refused.contains(opened.graph.identity()) && refused.contains(other.graph.identity()),
        "{refused}"
    );
    assert!(weaviate.searches.lock().unwrap().is_empty());
    assert_eq!(held(&node).await, before);

    let Some(unclaimed) = Node::embedding().await else {
        return;
    };
    let config = configured(&unclaimed, workforce_vellev::LOCAL_QUERY_PREFIX);
    let vellev = VellevCodeVectorStore::configured(&config.vellev).unwrap();
    vellev.ensure_collections().await.unwrap();
    let before = held(&unclaimed).await;
    let refused = compared(
        &other,
        &weaviate,
        &vellev,
        &Counters::shortlisting(),
        &options(&dir.path().join("unclaimed")),
        &|_| {},
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(refused.contains("claimed for no code graph"), "{refused}");
    assert_eq!(held(&unclaimed).await, before);
}

/// A run leaves the collections it measures as they were, and drops the
/// one it made to embed with.
#[tokio::test]
async fn a_run_leaves_the_measured_collections_as_they_were() {
    let Some(node) = Node::embedding().await else {
        return;
    };
    let config = configured(&node, workforce_vellev::LOCAL_QUERY_PREFIX);
    let entities = documented();
    let opened = built(&config, &entities).await;
    let vellev = VellevCodeVectorStore::configured(&config.vellev).unwrap();
    let before = (held(&node).await, vellev.found_as_wanted().await.unwrap());
    let dir = tempfile::tempdir().unwrap();
    compared(
        &opened,
        &weaviate_of(&entities, first),
        &vellev,
        &Counters::shortlisting(),
        &options(&dir.path().join("run")),
        &|_| {},
    )
    .await
    .unwrap();
    let after = (held(&node).await, vellev.found_as_wanted().await.unwrap());
    assert_eq!(after, before);
    assert!(after
        .0
        .iter()
        .all(|(name, ..)| !name.starts_with("WorkforceEmbedProbe")));
}

/// The node applies its prefix to a question: its text answer is its
/// prefixed vector's and not its plain one's. A tenant with no prefix
/// fails the gate unless the operator says none is meant, and saying so
/// with a prefix configured is refused before anything is read.
#[tokio::test]
async fn the_prefix_is_applied_by_the_node_and_told_apart() {
    let Some(node) = Node::embedding().await else {
        return;
    };
    let config = configured(&node, workforce_vellev::LOCAL_QUERY_PREFIX);
    let entities = documented();
    let opened = built(&config, &entities).await;
    let vellev = VellevCodeVectorStore::configured(&config.vellev).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("prefixed");
    let run = compared(
        &opened,
        &weaviate_of(&entities, first),
        &vellev,
        &Counters::shortlisting(),
        &options(&out),
        &|_| {},
    )
    .await
    .unwrap();
    assert!(gate(&run, "agreement").passed && gate(&run, "applied").passed);
    let answers: Vec<Answers> = rows(&out, "answers.jsonl");
    for answered in answers.iter().take(10) {
        let (text, vector) = (&answered.vellev.results, &answered.vellev_vector.results);
        assert!(stats::same_top(text, vector, DEPTH, WITHIN));
        let distances = text[0].distance.zip(vector[0].distance).unwrap();
        assert!((distances.0 - distances.1).abs() <= stats::SAME_DISTANCE);
    }

    let Some(plain) = Node::embedding().await else {
        return;
    };
    let config = configured(&plain, "");
    let opened = built(&config, &entities).await;
    let vellev = VellevCodeVectorStore::configured(&config.vellev).unwrap();
    let mut said = Vec::new();
    for (name, no_prefix) in [("unsaid", false), ("said", true)] {
        let options = CompareOptions {
            no_prefix,
            ..options(&dir.path().join(name))
        };
        let weaviate = weaviate_of(&entities, first);
        let run = compared(
            &opened,
            &weaviate,
            &vellev,
            &Counters::shortlisting(),
            &options,
            &|_| {},
        )
        .await
        .unwrap();
        said.push((
            gate(&run, "applied").passed,
            run.report.findings.prefix.applied,
        ));
    }
    assert_eq!(
        said,
        [(false, Applying::NotApplied), (true, Applying::ByStatement)]
    );

    let config = configured(&node, workforce_vellev::LOCAL_QUERY_PREFIX);
    let refused = compare_code_search(
        &config,
        Some(&dir.path().join("no graph here")),
        CompareOptions {
            no_prefix: true,
            ..options(&dir.path().join("refused"))
        },
        &|_| {},
    )
    .await
    .err()
    .unwrap()
    .to_string();
    assert!(refused.contains("--no-prefix"), "{refused}");
}

fn recall_query(repo: &str) -> Query {
    Query {
        id: format!("{repo}/e/name"),
        repo_id: repo.to_string(),
        entity_id: "e".to_string(),
        qualified_name: "lib::e".to_string(),
        start_line: 1,
        kind: "Function".to_string(),
        form: Form::Name,
        text: "what is asked".to_string(),
        weight: 1.0,
        stratum_weight: 1.0,
    }
}

/// Whether a search shortlisted is what the node says it did: a small
/// repo is scored whole, a large one shortlisted, and the large one cut
/// to under the shortlist's floor is scored whole again, though the graph
/// still counts it large. CI runs it with the node to itself
/// (`vellev-node-tests.sh`), so no other search is counted beside its own.
#[tokio::test]
async fn shortlisted_queries_are_told_by_the_node() {
    let Some(node) = Node::embedding().await else {
        return;
    };
    let Some(metrics) = node.metrics() else {
        return;
    };
    let config = configured(&node, workforce_vellev::LOCAL_QUERY_PREFIX);
    let small =
        workforce_codebase_knowledge::vector::code_embedding(config.vellev.code_model().unwrap());
    let (store, _) = workforce_codebase_knowledge::testing::counted(
        &node.url,
        &node.key,
        &node.tenant,
        Some(&small),
    );
    store.ensure_collections().await.unwrap();
    let of = |repo: &str, count: usize| -> Vec<VectorCodeEntity> {
        functions(repo, count)
            .iter()
            .enumerate()
            .map(|(n, entity)| CodeEntity {
                file_path: format!("src/part{}.rs", n / 100),
                ..entity.clone()
            })
            .map(|entity| VectorCodeEntity::from_code_entity(&entity, &entity.repo_id, None))
            .collect()
    };
    for batch in of("big", 5_000)
        .chunks(500)
        .chain(of("small", 300).chunks(300))
    {
        store.index_entities(batch).await.unwrap();
    }
    let vector = Counted::new(384)
        .documents(&["what is asked".to_string()])
        .await
        .unwrap()
        .remove(0);
    let vellev = VellevCodeVectorStore::configured(&config.vellev).unwrap();
    let work = NodeWork {
        client: reqwest::Client::new(),
        url: metrics,
    };
    let queries = [recall_query("small"), recall_query("big")];
    let vectors: Vec<Vectors> = queries
        .iter()
        .map(|query| Vectors {
            id: query.id.clone(),
            prefixed: vector.clone(),
            plain: None,
        })
        .collect();
    let asked: Vec<(&Query, &Vectors)> = queries.iter().zip(&vectors).collect();
    let told = |rows: Vec<Recalled>| -> BTreeMap<String, bool> {
        assert!(rows.iter().all(|row| row.self_check), "{rows:?}");
        rows.into_iter()
            .map(|row| (row.repo_id, row.shortlisted))
            .collect()
    };
    let rows = recalled(&asked, &vellev, &work, &|_| {}).await.unwrap();
    assert_eq!(
        told(rows),
        BTreeMap::from([("big".to_string(), true), ("small".to_string(), false)])
    );

    for part in 0..15 {
        store
            .delete_entities_by_file("big", &format!("src/part{part}.rs"))
            .await
            .unwrap();
    }
    let rows = recalled(&asked[1..], &vellev, &work, &|_| {})
        .await
        .unwrap();
    assert_eq!(rows[0].held, 3_500);
    assert_eq!(told(rows), BTreeMap::from([("big".to_string(), false)]));
}
