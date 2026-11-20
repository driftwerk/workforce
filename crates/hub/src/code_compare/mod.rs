//! `workforce code compare`: whether code search on vellev finds what
//! Weaviate's finds, measured on both stores directly before a switch.
//!
//! A run samples known items from the code graph, asks each store what the
//! product asks (repo-filtered, 20, judged at 10), compares their hits with
//! a paired bootstrap, measures the recall of vellev's index against an exact
//! scan, and checks that the node applies its query prefix. Nothing measured
//! is written to: the one collection made, for the node to embed the
//! questions, is dropped.

mod stats;

#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use eyre::{eyre, Result, WrapErr};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use vellev_core::Distance;
use workforce_codebase_knowledge::graph::entities::{CodeEntity, EntityId, EntityKind};
use workforce_codebase_knowledge::graph::CodeGraph;
use workforce_codebase_knowledge::vector::{
    CodeVectorStore, Near, NearestTo, SearchedByVector, VectorCodeEntity, VectorSearchResult,
    VellevCodeVectorStore, WeaviateCodeVectorStore,
};
use workforce_common::settings::WorkforceConfig;
use workforce_common::vectorizable::VectorizableText;

use stats::{Applying, Estimate, Form, Sum, Unit};

const LIMIT: usize = 20;
const DEPTH: usize = 10;
// vellev-storage's SHORTLIST_ABOVE.
const SHORTLIST_ABOVE: usize = 4_000;
const NEAREST_ROUTE: &str = "/v1/tenants/{tenant}/collections/{collection}/nearest";
const WITHIN: f32 = 1e-3;
const PAGE: usize = 1000;
const NOTE_EVERY: usize = 500;
const FILES: [&str; 5] = [
    "queries.jsonl",
    "vectors.jsonl",
    "answers.jsonl",
    "recall.jsonl",
    "report.json",
];
const SUMS: &str = "SHA256SUMS";
const NOTHING_TO_EMBED: &str = "record text has nothing to embed";
const NO_QUERY: &str = "no question to ask";

/// What `code compare run` is told.
#[derive(Debug, Clone, Serialize)]
pub struct CompareOptions {
    pub seed: u64,
    pub entities: usize,
    pub recall_queries: usize,
    pub same_queries: usize,
    pub resamples: usize,
    pub weaviate_url: String,
    pub node_metrics: String,
    pub no_prefix: bool,
    pub out: PathBuf,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
struct Scored {
    searches: u64,
    exactly: u64,
    estimated: u64,
}

#[async_trait]
trait Worked: Send + Sync {
    async fn scored(&self) -> Result<Scored>;
}

struct NodeWork {
    client: reqwest::Client,
    url: String,
}

#[async_trait]
impl Worked for NodeWork {
    async fn scored(&self) -> Result<Scored> {
        let answered = self
            .client
            .get(&self.url)
            .send()
            .await?
            .error_for_status()?;
        Ok(stats::work_sums(&answered.text().await?, NEAREST_ROUTE))
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Query {
    id: String,
    repo_id: String,
    entity_id: String,
    qualified_name: String,
    start_line: u32,
    kind: String,
    form: Form,
    text: String,
    weight: f64,
    stratum_weight: f64,
}

struct Target {
    entity: CodeEntity,
    text: String,
    queries: Vec<Query>,
}

struct Sample {
    targets: Vec<Target>,
    population: Population,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
struct Population {
    eligible: BTreeMap<String, usize>,
    allocated: BTreeMap<String, usize>,
    unsampled: BTreeMap<String, usize>,
    excluded: BTreeMap<String, usize>,
    summary_stale: usize,
    repos: BTreeMap<String, usize>,
}

fn asked(form: Form, text: Option<String>) -> Option<(Form, String)> {
    text.filter(|text| VectorizableText::try_new(text.as_str()).is_ok())
        .map(|text| (form, text))
}

fn unsummarised(entity: &CodeEntity) -> Vec<(Form, String)> {
    let doc = entity
        .doc_comment
        .as_deref()
        .and_then(stats::first_sentence);
    [
        asked(Form::Name, Some(entity.name.clone())),
        asked(Form::Doc, doc),
    ]
    .into_iter()
    .flatten()
    .collect()
}

async fn summarised(
    graph: &CodeGraph,
    entity: &CodeEntity,
) -> Result<(Option<(Form, String)>, bool)> {
    let granularity = match entity.kind {
        EntityKind::Function | EntityKind::Method => "function",
        EntityKind::File => "file",
        EntityKind::Module => "module",
        _ => return Ok((None, false)),
    };
    Ok(match graph.get_summary(&entity.id, granularity).await? {
        Some((_, true)) => (None, true),
        Some((text, false)) => (asked(Form::Summary, stats::first_sentence(&text)), false),
        None => (None, false),
    })
}

fn record_text(entity: &CodeEntity) -> String {
    VectorCodeEntity::from_code_entity(entity, &entity.repo_id, None).searchable_content
}

async fn eligible(
    graph: &CodeGraph,
    entity: &CodeEntity,
    population: &mut Population,
) -> Result<bool> {
    let excluded = match VectorizableText::try_new(record_text(entity)) {
        Err(_) => Some(NOTHING_TO_EMBED),
        Ok(_) => {
            *population.repos.entry(entity.repo_id.clone()).or_default() += 1;
            let questioned =
                !unsummarised(entity).is_empty() || summarised(graph, entity).await?.0.is_some();
            (!questioned).then_some(NO_QUERY)
        }
    };
    if let Some(why) = excluded {
        *population.excluded.entry(why.to_string()).or_default() += 1;
    }
    Ok(excluded.is_none())
}

/// Each kind's share, in integers, of the entities with an embeddable record
/// text and question, those of the lowest keys: one pass a page at a time.
async fn sampled(graph: &CodeGraph, seed: u64, entities: usize) -> Result<Sample> {
    let mut population = Population::default();
    let mut lowest: BTreeMap<String, BTreeMap<([u8; 32], String, String), CodeEntity>> =
        BTreeMap::new();
    let mut repos: Vec<String> = graph
        .list_repositories()
        .await?
        .into_iter()
        .map(|repo| repo.id)
        .collect();
    repos.sort();
    for repo in &repos {
        let mut after = String::new();
        loop {
            let page = graph.entities_page(repo, &after, None, PAGE).await?;
            let Some(last) = page.last().map(|entity| entity.id.0.clone()) else {
                break;
            };
            for entity in page {
                if !eligible(graph, &entity, &mut population).await? {
                    continue;
                }
                let kind = format!("{:?}", entity.kind);
                *population.eligible.entry(kind.clone()).or_default() += 1;
                let key = stats::key(seed, "sample", &[&entity.repo_id, &entity.id.0]);
                let kept = lowest.entry(kind).or_default();
                kept.insert((key, entity.repo_id.clone(), entity.id.0.clone()), entity);
                if kept.len() > entities {
                    kept.pop_last();
                }
            }
            after = last;
        }
    }
    population.allocated = stats::allocated(&population.eligible, entities);
    population.unsampled = population
        .allocated
        .iter()
        .filter(|(_, taken)| **taken == 0)
        .map(|(kind, _)| (kind.clone(), population.eligible[kind]))
        .collect();
    let mut targets = Vec::new();
    for (kind, kept) in lowest {
        let taken = population.allocated[&kind];
        let stratum_weight = stats::stratum_weight(population.eligible[&kind], taken);
        for entity in kept.into_values().take(taken) {
            let (summary, stale) = summarised(graph, &entity).await?;
            population.summary_stale += usize::from(stale);
            let queries = unsummarised(&entity)
                .into_iter()
                .chain(summary)
                .map(|(form, text)| Query {
                    id: format!("{}/{}/{}", entity.repo_id, entity.id.0, form.name()),
                    repo_id: entity.repo_id.clone(),
                    entity_id: entity.id.0.clone(),
                    qualified_name: entity.qualified_name.clone(),
                    start_line: entity.start_line as u32,
                    kind: kind.clone(),
                    form,
                    text,
                    weight: form.weight(),
                    stratum_weight,
                })
                .collect();
            let text = record_text(&entity);
            targets.push(Target {
                entity,
                text,
                queries,
            });
        }
    }
    targets.sort_by(|a, b| {
        (&a.entity.repo_id, &a.entity.id.0).cmp(&(&b.entity.repo_id, &b.entity.id.0))
    });
    Ok(Sample {
        targets,
        population,
    })
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Answer {
    #[serde(skip_serializing_if = "Option::is_none")]
    object: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    distance: Option<f32>,
    #[serde(flatten)]
    found: VectorSearchResult,
}

impl From<Near> for Answer {
    fn from(near: Near) -> Self {
        Self {
            object: Some(near.object),
            distance: Some(near.distance),
            found: near.found,
        }
    }
}

impl From<VectorSearchResult> for Answer {
    fn from(found: VectorSearchResult) -> Self {
        Self {
            object: None,
            distance: None,
            found,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Side {
    results: Vec<Answer>,
    hit: bool,
    hit_raw: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    siblings: Option<Vec<usize>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Answers {
    id: String,
    weaviate: Side,
    vellev: Side,
    vellev_vector: Side,
    #[serde(skip_serializing_if = "Option::is_none")]
    vellev_plain: Option<Side>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Vectors {
    id: String,
    prefixed: Vec<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    plain: Option<Vec<f32>>,
}

/// One question as it was asked, with every store's answer judged.
struct Asked {
    query: Query,
    vectors: Vectors,
    answers: Answers,
}

impl Asked {
    fn sum(&self) -> Sum {
        let (answers, weight) = (&self.answers, self.query.weight * self.query.stratum_weight);
        let one = |side: &Side| f64::from(u8::from(side.hit));
        let raw = |side: &Side| f64::from(u8::from(side.hit_raw));
        let (weaviate, vellev) = (one(&answers.weaviate), one(&answers.vellev));
        Sum {
            weight,
            d: weight * (vellev - weaviate),
            d_prefix: answers
                .vellev_plain
                .as_ref()
                .map_or(0.0, |plain| weight * (vellev - one(plain))),
            d_raw: weight * (raw(&answers.vellev) - raw(&answers.weaviate)),
            weaviate: weight * weaviate,
            vellev: weight * vellev,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Recalled {
    id: String,
    repo_id: String,
    held: usize,
    k: usize,
    index: Vec<Answer>,
    exact: Vec<Answer>,
    scored: (Scored, Scored),
    shortlisted: bool,
    recall: f64,
    self_check: bool,
}

#[derive(Clone)]
struct Held {
    qualified_name: String,
    text: String,
}

/// Results resolved to the graph's entities as the product resolves them,
/// each looked up once.
struct Judge<'a> {
    graph: &'a CodeGraph,
    held: HashMap<String, Option<Held>>,
}

impl Judge<'_> {
    async fn held(&mut self, entity_id: &str) -> Result<Option<Held>> {
        if let Some(held) = self.held.get(entity_id) {
            return Ok(held.clone());
        }
        let entity = self
            .graph
            .get_entity(&EntityId(entity_id.to_string()))
            .await?;
        let held = entity.map(|entity| Held {
            qualified_name: entity.qualified_name.clone(),
            text: record_text(&entity),
        });
        self.held.insert(entity_id.to_string(), held.clone());
        Ok(held)
    }

    /// `hit`: among the first [`DEPTH`] distinct entities the graph holds of
    /// `results`, the target's text. `hit_raw`: among the first [`DEPTH`] as
    /// they came, held under their own qualified name, the target's text.
    async fn side(
        &mut self,
        target: &Target,
        results: Vec<Answer>,
        weaviate: bool,
    ) -> Result<Side> {
        let (mut seen, mut hit, mut hit_raw) = (HashSet::new(), false, false);
        for (at, result) in results.iter().enumerate() {
            let Some(held) = self.held(&result.found.entity_id).await? else {
                continue;
            };
            let is_target = held.text == target.text;
            if seen.len() < DEPTH && seen.insert(result.found.entity_id.clone()) {
                hit |= is_target;
            }
            if at < DEPTH && held.qualified_name == result.found.qualified_name {
                hit_raw |= is_target;
            }
        }
        Ok(Side {
            hit,
            hit_raw,
            siblings: weaviate.then(|| siblings(&target.entity, &results)),
            results,
        })
    }
}

/// The places, from one, of `results` of `target`'s id under another
/// qualified name: siblings the graph dropped.
fn siblings(target: &CodeEntity, results: &[Answer]) -> Vec<usize> {
    results
        .iter()
        .enumerate()
        .filter(|(_, result)| {
            result.found.entity_id == target.id.0
                && result.found.qualified_name != target.qualified_name
        })
        .map(|(at, _)| at + 1)
        .collect()
}

fn failed_on(side: &str, id: &str) -> impl FnOnce(eyre::Report) -> eyre::Report {
    let said = format!("{side} failed on query {id}");
    move |e| e.wrap_err(said)
}

fn picked<'a>(
    seed: u64,
    purpose: &str,
    asked: impl Iterator<Item = &'a Asked>,
    count: usize,
) -> Vec<&'a Asked> {
    let mut keyed: Vec<([u8; 32], &Asked)> = asked
        .map(|one| (stats::key(seed, purpose, &[&one.query.id]), one))
        .collect();
    keyed.sort_by(|a, b| a.0.cmp(&b.0));
    keyed.into_iter().take(count).map(|(_, one)| one).collect()
}

async fn nearest(
    vellev: &dyn SearchedByVector,
    to: NearestTo<'_>,
    query: &Query,
    side: &str,
) -> Result<Vec<Answer>> {
    let found = vellev.nearest(to, &query.repo_id, LIMIT).await;
    Ok(found
        .map_err(failed_on(side, &query.id))?
        .into_iter()
        .map(Answer::from)
        .collect())
}

async fn answered(
    judge: &mut Judge<'_>,
    weaviate: &dyn CodeVectorStore,
    vellev: &dyn SearchedByVector,
    (target, query): (&Target, &Query),
    vectors: &Vectors,
) -> Result<Answers> {
    let from_weaviate = weaviate
        .search_entities(&query.text, Some(&query.repo_id), None, LIMIT)
        .await
        .map_err(failed_on("weaviate", &query.id))?;
    let by_text = nearest(vellev, NearestTo::Text(&query.text), query, "vellev").await?;
    let prefixed = NearestTo::Vector(&vectors.prefixed);
    let by_vector = nearest(vellev, prefixed, query, "vellev vector").await?;
    let by_plain = match &vectors.plain {
        Some(plain) => {
            Some(nearest(vellev, NearestTo::Vector(plain), query, "vellev plain").await?)
        }
        None => None,
    };
    let weaviate = from_weaviate.into_iter().map(Answer::from).collect();
    Ok(Answers {
        id: query.id.clone(),
        weaviate: judge.side(target, weaviate, true).await?,
        vellev: judge.side(target, by_text, false).await?,
        vellev_vector: judge.side(target, by_vector, false).await?,
        vellev_plain: match by_plain {
            Some(plain) => Some(judge.side(target, plain, false).await?),
            None => None,
        },
    })
}

async fn vectors_of(
    vellev: &dyn SearchedByVector,
    queries: &[(&Target, &Query)],
) -> Result<Vec<Vectors>> {
    let texts: Vec<String> = queries
        .iter()
        .map(|(_, query)| query.text.clone())
        .collect();
    let prefix = vellev.query_prefix();
    let prefixed = prefix
        .iter()
        .flat_map(|prefix| texts.iter().map(move |text| format!("{prefix}{text}")));
    let written: Vec<String> = texts.iter().cloned().chain(prefixed).collect();
    let embedded = vellev.embedded(&written).await.wrap_err("probe failed")?;
    let (plain, prefixed) = match prefix {
        Some(_) => embedded.split_at(texts.len()),
        None => (&[][..], embedded.as_slice()),
    };
    Ok(queries
        .iter()
        .zip(prefixed)
        .enumerate()
        .map(|(at, ((_, query), prefixed))| Vectors {
            id: query.id.clone(),
            prefixed: prefixed.clone(),
            plain: plain.get(at).cloned(),
        })
        .collect())
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
struct Counts {
    graph: usize,
    weaviate: usize,
    vellev: usize,
}

struct Measured {
    asked: Vec<Asked>,
    recall: Vec<Recalled>,
    repos: BTreeMap<String, Counts>,
    findings: Findings,
}

async fn measured(
    graph: &CodeGraph,
    weaviate: &dyn CodeVectorStore,
    vellev: &dyn SearchedByVector,
    work: &dyn Worked,
    options: &CompareOptions,
    note: &(dyn Fn(&str) + Send + Sync),
) -> Result<Measured> {
    let Sample {
        targets,
        population,
    } = sampled(graph, options.seed, options.entities).await?;
    note(&format!(
        "{} entities eligible, of {} kinds; {} left out; {} sampled",
        population.eligible.values().sum::<usize>(),
        population.eligible.len(),
        population.excluded.values().sum::<usize>(),
        targets.len()
    ));
    let queries: Vec<(&Target, &Query)> = targets
        .iter()
        .flat_map(|target| target.queries.iter().map(move |query| (target, query)))
        .collect();
    note(&format!(
        "having the node embed {} questions, one text a write",
        queries.len()
    ));
    let vectors = vectors_of(vellev, &queries).await?;
    let mut judge = Judge {
        graph,
        held: HashMap::new(),
    };
    let mut asked = Vec::with_capacity(queries.len());
    for (at, (question, vectors)) in queries.iter().zip(vectors).enumerate() {
        if at % NOTE_EVERY == 0 {
            note(&format!("asking question {} of {}", at + 1, queries.len()));
        }
        let answers = answered(&mut judge, weaviate, vellev, *question, &vectors).await?;
        let query = question.1.clone();
        asked.push(Asked {
            query,
            vectors,
            answers,
        });
    }
    let large = |one: &&Asked| {
        population
            .repos
            .get(&one.query.repo_id)
            .is_some_and(|held| *held > SHORTLIST_ABOVE)
    };
    let qualified = asked.iter().filter(large).count();
    let recall_asked: Vec<(&Query, &Vectors)> = picked(
        options.seed,
        "recall",
        asked.iter().filter(large),
        options.recall_queries,
    )
    .into_iter()
    .map(|one| (&one.query, &one.vectors))
    .collect();
    let same_ids = picked(options.seed, "same", asked.iter(), options.same_queries)
        .into_iter()
        .map(|one| one.query.id.clone())
        .collect();
    let recall = recalled(&recall_asked, vellev, work, note).await?;
    let mut repos = BTreeMap::new();
    for repo in asked
        .iter()
        .map(|one| one.query.repo_id.as_str())
        .collect::<BTreeSet<_>>()
    {
        let counts = Counts {
            graph: population.repos.get(repo).copied().unwrap_or_default(),
            weaviate: weaviate
                .entity_count(repo)
                .await
                .map_err(failed_on("weaviate", repo))?,
            vellev: vellev
                .counted(repo)
                .await
                .map_err(failed_on("vellev", repo))?,
        };
        repos.insert(repo.to_string(), counts);
    }
    let findings = found(
        &asked,
        &recall,
        vellev.query_prefix(),
        options,
        qualified,
        population,
        same_ids,
    );
    for gate in &findings.gates {
        let passed = if gate.passed { "passed" } else { "FAILED" };
        note(&format!("{passed} {}: {}", gate.name, gate.said));
    }
    Ok(Measured {
        asked,
        recall,
        repos,
        findings,
    })
}

/// Check 2: each question asked of the node's index alone, its work read
/// before and after, then each repo scanned once for all of its questions.
async fn recalled(
    asked: &[(&Query, &Vectors)],
    vellev: &dyn SearchedByVector,
    work: &dyn Worked,
    note: &(dyn Fn(&str) + Send + Sync),
) -> Result<Vec<Recalled>> {
    let mut by_repo: BTreeMap<&str, Vec<_>> = BTreeMap::new();
    for (query, vectors) in asked {
        let before = work
            .scored()
            .await
            .map_err(failed_on("node metrics", &query.id))?;
        let to = NearestTo::Vector(&vectors.prefixed);
        let index = nearest(vellev, to, query, "vellev vector").await?;
        let after = work
            .scored()
            .await
            .map_err(failed_on("node metrics", &query.id))?;
        by_repo.entry(query.repo_id.as_str()).or_default().push((
            *query,
            *vectors,
            index,
            (before, after),
        ));
    }
    let mut recalled = Vec::new();
    for (repo, asked) in by_repo {
        note(&format!(
            "scanning every entity of {repo} for {} questions",
            asked.len()
        ));
        let wanted: Vec<(&[f32], Option<&str>)> = asked
            .iter()
            .map(|(_, vectors, index, _)| {
                let first = index.first().and_then(|first| first.object.as_deref());
                (vectors.prefixed.as_slice(), first)
            })
            .collect();
        let scanned = vellev
            .nearest_exactly(&wanted, repo, DEPTH)
            .await
            .map_err(failed_on("exact scan", repo))?;
        let held = scanned.held;
        let scans = scanned.nearest.into_iter().zip(scanned.firsts);
        for ((query, _, index, scored), (exact, first)) in asked.into_iter().zip(scans) {
            let counted = scored.1.searches.saturating_sub(scored.0.searches);
            let shortlisted = stats::shortlisted(scored.0, scored.1, held).ok_or_else(|| {
                eyre!(
                    "node metrics failed on query {}: the node counted {counted} searches for one request, and must be idle during a run",
                    query.id
                )
            })?;
            let (k, exact): (usize, Vec<Answer>) = (
                held.min(DEPTH),
                exact.into_iter().map(Answer::from).collect(),
            );
            let recall = stats::recall(&index, &exact, k);
            recalled.push(Recalled {
                id: query.id.clone(),
                repo_id: query.repo_id.clone(),
                held,
                k,
                self_check: stats::first_agrees(index.first(), first)
                    && (shortlisted || recall == 1.0),
                index,
                exact,
                scored,
                shortlisted,
                recall,
            });
        }
    }
    Ok(recalled)
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
struct Check1 {
    overall: Estimate,
    kinds: BTreeMap<String, (usize, Estimate)>,
    d_raw: Estimate,
    weaviate: f64,
    vellev: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
struct Prefix {
    query_prefix: Option<String>,
    applied: Applying,
    d_prefix: Option<Estimate>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize)]
struct Agreement {
    same: usize,
    same_agreed: usize,
    others: usize,
    others_agreed: usize,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize)]
struct Check2 {
    qualified: usize,
    asked: usize,
    shortlisted: usize,
    recall: Option<f64>,
    self_checked: bool,
}

/// The share of sampled targets whose Weaviate answers hold a sibling the
/// graph dropped; and by repo, the targets, those holding one, and siblings.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
struct Siblings {
    share: f64,
    repos: BTreeMap<String, Collided>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize)]
struct Collided {
    targets: usize,
    holding: usize,
    colliding: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
struct Gate {
    name: String,
    passed: bool,
    said: String,
}

#[derive(Debug, Clone, Serialize)]
struct Findings {
    sample: Population,
    check1: Check1,
    siblings: Siblings,
    prefix: Prefix,
    check2: Check2,
    agreement: Agreement,
    gates: Vec<Gate>,
    verdict: &'static str,
    same_queries: BTreeSet<String>,
}

#[derive(Default)]
struct Tally {
    units: Vec<Unit>,
    siblings: BTreeMap<String, Collided>,
    agreement: Agreement,
    applied: bool,
}

/// The statistics and gates of a run, in one fold over its targets.
fn found(
    asked: &[Asked],
    recall: &[Recalled],
    query_prefix: Option<String>,
    options: &CompareOptions,
    qualified: usize,
    sample: Population,
    same_queries: BTreeSet<String>,
) -> Findings {
    let agrees =
        |a: &Answers, b: &Side| stats::same_top(&a.vellev.results, &b.results, DEPTH, WITHIN);
    let target = |a: &Asked, b: &Asked| {
        (&a.query.repo_id, &a.query.entity_id) == (&b.query.repo_id, &b.query.entity_id)
    };
    let tally = asked
        .chunk_by(target)
        .fold(Tally::default(), |mut tally, queries| {
            let query = &queries[0].query;
            let sum = queries.iter().map(Asked::sum).sum::<Sum>();
            tally.units.push(Unit {
                stratum: query.kind.clone(),
                queries: queries.len(),
                sum,
            });
            let colliding: usize = queries
                .iter()
                .filter_map(|one| one.answers.weaviate.siblings.as_ref())
                .map(Vec::len)
                .sum();
            let repo = tally.siblings.entry(query.repo_id.clone()).or_default();
            repo.targets += 1;
            repo.holding += usize::from(colliding > 0);
            repo.colliding += colliding;
            for one in queries {
                let agreed = agrees(&one.answers, &one.answers.vellev_vector);
                let (count, agreeing) = match same_queries.contains(&one.query.id) {
                    true => (&mut tally.agreement.same, &mut tally.agreement.same_agreed),
                    false => (
                        &mut tally.agreement.others,
                        &mut tally.agreement.others_agreed,
                    ),
                };
                *count += 1;
                *agreeing += usize::from(agreed);
                let plain = one.answers.vellev_plain.as_ref();
                tally.applied |= agreed && plain.is_some_and(|plain| !agrees(&one.answers, plain));
            }
            tally
        });
    let replicates = stats::bootstrap(&tally.units, options.resamples, options.seed);
    let total = tally.units.iter().map(|unit| unit.sum).sum::<Sum>();
    let estimate = |measure: fn(&Sum) -> f64| Estimate::of(total, &replicates.overall, measure);
    let kinds = stats::strata(&tally.units)
        .into_iter()
        .map(|(kind, units)| {
            let queries = units.iter().map(|unit| unit.queries).sum();
            let sum = units.iter().map(|unit| unit.sum).sum::<Sum>();
            (
                kind.to_string(),
                (
                    queries,
                    Estimate::of(sum, &replicates.strata[kind], |sum| sum.d),
                ),
            )
        })
        .collect();
    let check1 = Check1 {
        overall: estimate(|sum| sum.d),
        kinds,
        d_raw: estimate(|sum| sum.d_raw),
        weaviate: total.mean(|sum| sum.weaviate),
        vellev: total.mean(|sum| sum.vellev),
    };
    let applied = match (&query_prefix, options.no_prefix, tally.applied) {
        (None, true, _) => Applying::ByStatement,
        (Some(_), _, true) => Applying::Applied,
        _ => Applying::NotApplied,
    };
    let prefix = Prefix {
        d_prefix: query_prefix.as_ref().map(|_| estimate(|sum| sum.d_prefix)),
        query_prefix,
        applied,
    };
    let shortlisted: Vec<f64> = recall
        .iter()
        .filter(|row| row.shortlisted)
        .map(|row| row.recall)
        .collect();
    let check2 = Check2 {
        qualified,
        asked: recall.len(),
        shortlisted: shortlisted.len(),
        recall: (!shortlisted.is_empty())
            .then(|| shortlisted.iter().sum::<f64>() / shortlisted.len() as f64),
        self_checked: recall.iter().all(|row| row.self_check),
    };
    let gates = stats::gates(&check1, &prefix, &tally.agreement, &check2);
    let targets: usize = tally.siblings.values().map(|repo| repo.targets).sum();
    let holding: usize = tally.siblings.values().map(|repo| repo.holding).sum();
    Findings {
        sample,
        check1,
        siblings: Siblings {
            share: holding as f64 / targets.max(1) as f64,
            repos: tally.siblings,
        },
        prefix,
        check2,
        agreement: tally.agreement,
        verdict: if gates.iter().all(|gate| gate.passed) {
            "pass"
        } else {
            "fail"
        },
        gates,
        same_queries,
    }
}

#[derive(Debug, Clone, Serialize)]
struct GraphAbout {
    identity: String,
    backend: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    sha256: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct VellevAbout {
    url: String,
    tenant: String,
    claim: String,
    identity_start: String,
    identity_end: String,
    model: String,
    dimensions: u32,
    query_prefix: Option<String>,
    distance: Distance,
}

#[derive(Debug, Clone, Serialize)]
struct Manifest {
    version: String,
    started: String,
    ended: String,
    #[serde(flatten)]
    options: CompareOptions,
    limit: usize,
    depth: usize,
    graph: GraphAbout,
    repos: BTreeMap<String, Counts>,
    vellev: VellevAbout,
    written_alone: bool,
}

#[derive(Debug, Clone, Serialize)]
struct Report {
    manifest: Manifest,
    #[serde(flatten)]
    findings: Findings,
}

/// A finished run: where its files are, and its report.
#[derive(Debug)]
pub struct Run {
    pub dir: PathBuf,
    report: Report,
}

impl Run {
    /// `Err` naming every failed gate, unless the verdict is a pass.
    pub fn passed(&self) -> Result<()> {
        let failed: Vec<String> = self
            .report
            .findings
            .gates
            .iter()
            .filter(|gate| !gate.passed)
            .map(|gate| format!("{} ({})", gate.name, gate.said))
            .collect();
        match failed.is_empty() {
            true => Ok(()),
            false => Err(eyre!(
                "the comparison failed {} gate(s): {}; its files in {} are complete",
                failed.len(),
                failed.join("; "),
                self.dir.display()
            )),
        }
    }
}

/// What a run found, as an operator reads it.
impl std::fmt::Display for Run {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let found = &self.report.findings;
        let (check1, check2, agreement) = (&found.check1, &found.check2, &found.agreement);
        let estimate = |e: &Estimate| format!("{:+.4} [{:+.4}, {:+.4}]", e.d, e.lower, e.upper);
        writeln!(
            f,
            "  hit@10: weaviate {:.4}, vellev {:.4}",
            check1.weaviate, check1.vellev
        )?;
        writeln!(f, "  D (vellev − weaviate): {}", estimate(&check1.overall))?;
        for (kind, (queries, e)) in &check1.kinds {
            writeln!(f, "  D of {kind} ({queries} queries): {}", estimate(e))?;
        }
        let share = found.siblings.share;
        writeln!(
            f,
            "  D_raw: {}; Weaviate holds a dropped sibling for {share:.4} of targets",
            estimate(&check1.d_raw)
        )?;
        match &found.prefix.d_prefix {
            Some(e) => writeln!(
                f,
                "  the query prefix {:?} adds {}",
                found.prefix.query_prefix.as_deref().unwrap_or_default(),
                estimate(e)
            )?,
            None => writeln!(f, "  no query prefix")?,
        }
        match check2.recall {
            Some(recall) => writeln!(
                f,
                "  recall@10 over {} shortlisted queries: {recall:.4} of {} asked",
                check2.shortlisted, check2.asked
            )?,
            None => writeln!(
                f,
                "  recall: exhaustive, the node shortlisted none of {} queries",
                check2.asked
            )?,
        }
        writeln!(
            f,
            "  vector answers agree with text answers on {} of {} same queries, {} of {} others",
            agreement.same_agreed, agreement.same, agreement.others_agreed, agreement.others
        )?;
        write!(
            f,
            "  {} ({} files summed in {SUMS})",
            self.dir.display(),
            FILES.len()
        )
    }
}

fn partial(out: &Path) -> PathBuf {
    let mut name = out.as_os_str().to_owned();
    name.push(".partial");
    PathBuf::from(name)
}

fn refused_if_there(out: &Path) -> Result<()> {
    match [out.to_path_buf(), partial(out)].into_iter().find(|there| there.exists()) {
        Some(there) => Err(eyre!(
            "{} exists: a run never writes over one; keep it to look into, or remove it, and run again",
            there.display()
        )),
        None => Ok(()),
    }
}

/// Write a file through `write`, make it durable, and hash it as it lies on disk.
fn wrote(path: &Path, write: impl FnOnce(&mut BufWriter<File>) -> Result<()>) -> Result<String> {
    let mut writer = BufWriter::new(File::create(path)?);
    write(&mut writer)?;
    writer
        .into_inner()
        .map_err(|e| e.into_error())?
        .sync_all()?;
    sha256_of(path)
}

fn sha256_of(path: &Path) -> Result<String> {
    let mut hasher = Sha256::new();
    let mut file = File::open(path).wrap_err_with(|| format!("reading {}", path.display()))?;
    std::io::copy(&mut file, &mut hasher)?;
    Ok(hex::encode(hasher.finalize()))
}

fn lines<T: Serialize>(
    rows: impl Iterator<Item = T>,
) -> impl FnOnce(&mut BufWriter<File>) -> Result<()> {
    move |writer| {
        rows.into_iter().try_for_each(|row| {
            serde_json::to_writer(&mut *writer, &row)?;
            Ok(writer.write_all(b"\n")?)
        })
    }
}

/// Make `out`'s `.partial`, measure into it, write the files and then their
/// sums, and move them to `out`. A failure leaves `.partial` with no sums.
async fn recorded(
    out: &Path,
    measure: impl std::future::Future<Output = Result<(Measured, Manifest)>>,
) -> Result<Run> {
    refused_if_there(out)?;
    let partial = partial(out);
    std::fs::create_dir_all(partial.parent().unwrap_or(Path::new(".")))?;
    std::fs::create_dir(&partial).wrap_err_with(|| format!("making {}", partial.display()))?;
    let (measured, manifest) = measure.await?;
    let report = Report {
        manifest,
        findings: measured.findings,
    };
    let (asked, at) = (&measured.asked, |name: &str| partial.join(name));
    let sums = [
        wrote(&at(FILES[0]), lines(asked.iter().map(|one| &one.query)))?,
        wrote(&at(FILES[1]), lines(asked.iter().map(|one| &one.vectors)))?,
        wrote(&at(FILES[2]), lines(asked.iter().map(|one| &one.answers)))?,
        wrote(&at(FILES[3]), lines(measured.recall.iter()))?,
        wrote(&at(FILES[4]), |writer| {
            serde_json::to_writer_pretty(&mut *writer, &report)?;
            Ok(writer.write_all(b"\n")?)
        })?,
    ];
    let listed: String = FILES
        .iter()
        .zip(&sums)
        .map(|(name, sum)| format!("{sum}  {name}\n"))
        .collect();
    wrote(&at(SUMS), |writer| Ok(writer.write_all(listed.as_bytes())?))?;
    std::fs::rename(&partial, out)
        .wrap_err_with(|| format!("moving {} to {}", partial.display(), out.display()))?;
    Ok(Run {
        dir: out.to_path_buf(),
        report,
    })
}

struct Opened {
    graph: CodeGraph,
    about: GraphAbout,
}

/// The code graph `config` names (in `data_dir`, if given), opened as the hub
/// opens it: a SQLite graph is hashed first, and refused while its write-ahead
/// log holds pages the hash would not cover.
fn opened_graph(config: &WorkforceConfig, data_dir: Option<&Path>) -> Result<Opened> {
    let data_dir = data_dir.map_or_else(
        || PathBuf::from(crate::seeding::knowledge_dirs(config).1),
        Path::to_path_buf,
    );
    let backend = config.codegraph.backend.clone();
    let sha256 = match backend.as_str() {
        "postgres" => None,
        _ => {
            let path = workforce_knowledge_service::code_graph_path(&data_dir);
            let wal = PathBuf::from(format!("{}-wal", path.display()));
            if std::fs::metadata(&wal).is_ok_and(|wal| wal.len() > 0) {
                return Err(eyre!(
                    "{} holds pages not yet in the code graph, which its hash would not cover: open the graph once with sqlite3 and close it (or copy it with `.backup`), then run again",
                    wal.display()
                ));
            }
            if !path.is_file() {
                return Err(eyre!("there is no code graph at {}", path.display()));
            }
            Some(sha256_of(&path)?)
        }
    };
    let knowledge = &config.knowledge;
    let (codegraph, postgres) = (&config.codegraph, config.postgres.as_ref());
    let graph = workforce_knowledge_service::open_code_graph(
        &data_dir,
        knowledge,
        &knowledge.vector_backend,
        codegraph,
        postgres,
    )?;
    let identity = graph.identity().to_string();
    Ok(Opened {
        graph,
        about: GraphAbout {
            identity,
            backend,
            sha256,
        },
    })
}

/// Measure code search on the vellev `[vellev.code]` names against the
/// Weaviate at `options.weaviate_url`, over the code graph, and write the
/// run's files to `options.out`. A run whose gates fail is still a run (see
/// [`Run::passed`]).
pub async fn compare_code_search(
    config: &WorkforceConfig,
    data_dir: Option<&Path>,
    options: CompareOptions,
    note: &(dyn Fn(&str) + Send + Sync),
) -> Result<Run> {
    refused_if_there(&options.out)?;
    let vellev = VellevCodeVectorStore::configured(&config.vellev)?;
    if let (true, Some(prefix)) = (options.no_prefix, vellev.query_prefix()) {
        return Err(eyre!("--no-prefix says the code model has no query prefix, and [vellev.code] gives it {prefix:?}"));
    }
    let opened = opened_graph(config, data_dir)?;
    let weaviate = WeaviateCodeVectorStore::new(
        options.weaviate_url.clone(),
        config.weaviate.api_key.as_deref(),
    );
    let work = NodeWork {
        client: reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()?,
        url: options.node_metrics.clone(),
    };
    compared(&opened, &weaviate, &vellev, &work, &options, note).await
}

async fn compared(
    opened: &Opened,
    weaviate: &dyn CodeVectorStore,
    vellev: &VellevCodeVectorStore,
    work: &dyn Worked,
    options: &CompareOptions,
    note: &(dyn Fn(&str) + Send + Sync),
) -> Result<Run> {
    let started = chrono::Utc::now().to_rfc3339();
    let identity_start = vellev.found_as_wanted().await?;
    let (graph, tenant) = (opened.graph.identity(), vellev.tenant().unwrap_or_default());
    let claim = vellev.claimed_by().await?.unwrap_or_default();
    if claim != graph {
        let claimed = match claim.as_str() {
            "" => "no code graph".to_string(),
            claim => format!("the code graph {claim}"),
        };
        return Err(eyre!(
            "the code vectors of vellev tenant '{tenant}' are claimed for {claimed}, not this one ({graph}): a run measures a tenant built from the graph it is given"
        ));
    }
    if !weaviate.health_check().await? {
        return Err(eyre!("Weaviate at {} is not healthy", weaviate.base_url()));
    }
    let measure = async {
        let measured = measured(&opened.graph, weaviate, vellev, work, options, note).await?;
        let identity_end = vellev.found_as_wanted().await?;
        if identity_end != identity_start {
            return Err(eyre!(
                "collection replaced during the run: the code collections were {identity_start} and are now {identity_end}"
            ));
        }
        let (_, embedding) = vellev.embedded_as();
        let manifest = Manifest {
            version: env!("CARGO_PKG_VERSION").to_string(),
            started,
            ended: chrono::Utc::now().to_rfc3339(),
            options: options.clone(),
            limit: LIMIT,
            depth: DEPTH,
            graph: opened.about.clone(),
            repos: measured.repos.clone(),
            vellev: VellevAbout {
                url: vellev.base_url().to_string(),
                tenant,
                claim,
                identity_start,
                identity_end,
                model: embedding.model,
                dimensions: embedding.dimensions,
                query_prefix: embedding.query_prefix,
                distance: vellev.distance(),
            },
            written_alone: true,
        };
        Ok((measured, manifest))
    };
    recorded(&options.out, measure).await
}
