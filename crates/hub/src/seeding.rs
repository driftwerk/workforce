//! Seeding vellev's derived indexes from the command line: the code
//! vectors from the code graph and the artifact index from Postgres, with
//! no crawl, resumably, before or after a cutover.
//!
//! By default the vellev node embeds what is written, one run of its model
//! at a time. With an [`Outside`] embedder the same model served elsewhere
//! makes the vectors, several requests at once, and they are written with
//! the objects. Before any such vector is written, a sample is embedded
//! both ways and compared ([`workforce_vellev::seeding::equivalence`]):
//! an embedder that does not embed as the node does is refused.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use eyre::{eyre, Result};
use rand::Rng;
use serde::Deserialize;
use workforce_codebase_knowledge::graph::vectors::VectorFill;
use workforce_codebase_knowledge::indexer::fill::{Confirmed, VectorFiller};
use workforce_codebase_knowledge::vector::{
    CodeVectorStore, VectorCodeEntity, VellevCodeVectorStore,
};
use workforce_common::settings::WorkforceConfig;
use workforce_vellev::seeding::{equivalence, DocumentEmbedder, Equivalence, Seeding, MIN_COSINE};

use crate::knowledge::{FillPace, FillRun, KnowledgeService, VectorBackend};

/// The API an embedding endpoint speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmbedApi {
    /// Hugging Face text-embeddings-inference: `POST /embed`.
    Tei,
    /// OpenAI's: `POST /v1/embeddings`.
    Openai,
}

impl std::str::FromStr for EmbedApi {
    type Err = eyre::Report;

    fn from_str(api: &str) -> Result<Self> {
        match api {
            "tei" => Ok(Self::Tei),
            "openai" => Ok(Self::Openai),
            other => Err(eyre!(
                "an embedding API is \"tei\" or \"openai\", not \"{other}\""
            )),
        }
    }
}

/// An embedding endpoint outside the vellev node. Texts are sent as they
/// are, with no prefix, cut at the model's length by the endpoint, as the
/// node cuts them.
pub struct Outside {
    http: reqwest::Client,
    url: String,
    api: EmbedApi,
    model: String,
    key: Option<String>,
    /// How a request the endpoint is too busy for is tried again.
    again: Again,
}

/// How a request that did not reach an endpoint, or that it answered 429,
/// 500, 502, 503 or 504 to, is sent again: after
/// the wait the endpoint names (`Retry-After`), or else one that doubles
/// from `first`, neither longer than `longest`, until `tries` have been
/// made. An endpoint that is busy is working; one asked again at once by
/// every request it refused stays busy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Again {
    pub tries: u32,
    pub first: Duration,
    pub longest: Duration,
}

impl Default for Again {
    fn default() -> Self {
        Self {
            tries: 10,
            first: Duration::from_millis(500),
            longest: Duration::from_secs(30),
        }
    }
}

impl Again {
    /// The wait before try number `tried + 1`, given what the endpoint
    /// said to wait: its own word if it gave one, or the doubling wait,
    /// spread by up to a half so that requests refused together do not
    /// come back together.
    fn wait(&self, tried: u32, retry_after: Option<Duration>) -> Duration {
        let doubled = self.first.saturating_mul(1u32 << tried.min(16));
        let spread = 1.0 + rand::thread_rng().gen_range(0.0..0.5);
        retry_after
            .unwrap_or_else(|| doubled.mul_f64(spread))
            .min(self.longest)
    }
}

impl Outside {
    pub fn new(url: &str, api: EmbedApi, model: &str, key: Option<String>) -> Result<Self> {
        Ok(Self {
            again: Again::default(),
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(120))
                .build()?,
            url: url
                .trim_end_matches('/')
                .trim_end_matches("/v1")
                .to_string(),
            api,
            model: model.to_string(),
            key,
        })
    }

    /// Try a refused request again as `again` says.
    pub fn trying(mut self, again: Again) -> Self {
        self.again = again;
        self
    }
}

#[derive(Deserialize)]
struct OpenaiEmbeddings {
    data: Vec<OpenaiEmbedding>,
}

#[derive(Deserialize)]
struct OpenaiEmbedding {
    index: usize,
    embedding: Vec<f32>,
}

#[async_trait]
impl DocumentEmbedder for Outside {
    fn name(&self) -> String {
        format!("the embedding endpoint {} ({})", self.url, self.model)
    }

    async fn documents(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        let (path, body) = match self.api {
            EmbedApi::Tei => (
                "/embed",
                serde_json::json!({ "inputs": texts, "truncate": true }),
            ),
            EmbedApi::Openai => (
                "/v1/embeddings",
                serde_json::json!({ "model": self.model, "input": texts }),
            ),
        };
        let mut tried = 0;
        let answered = loop {
            let mut request = self.http.post(format!("{}{path}", self.url)).json(&body);
            if let Some(key) = &self.key {
                request = request.bearer_auth(key);
            }
            let answered = match request.send().await {
                Ok(answered) => answered,
                // Not reached, or the connection lost: as likely to pass
                // as a busy answer, and tried again as one is.
                Err(e) => {
                    tried += 1;
                    if tried >= self.again.tries {
                        let tries = match tried {
                            1 => String::new(),
                            tried => format!(" (tried {tried} times)"),
                        };
                        return Err(eyre::Report::new(e)
                            .wrap_err(format!("{} cannot be reached{tries}", self.name())));
                    }
                    tokio::time::sleep(self.again.wait(tried - 1, None)).await;
                    continue;
                }
            };
            let status = answered.status();
            if status.is_success() {
                break answered;
            }
            // Too busy for now (text-embeddings-inference answers 429 when
            // more inputs are waiting than it was started to hold), not
            // serving yet, or failing as a server or the proxy before it
            // does for a moment: none says anything of the texts, and a
            // run of hours is not ended by one of them.
            let busy = matches!(status.as_u16(), 429 | 500 | 502 | 503 | 504);
            let retry_after = answered
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|after| after.to_str().ok()?.trim().parse::<u64>().ok())
                .map(Duration::from_secs);
            tried += 1;
            if !busy || tried >= self.again.tries {
                let said: String = answered.text().await.unwrap_or_default();
                let said: String = said.chars().take(300).collect();
                let tries = match tried {
                    1 => String::new(),
                    tried => format!(" (asked {tried} times)"),
                };
                return Err(eyre!("{} answered {status}{tries}: {said}", self.name()));
            }
            tokio::time::sleep(self.again.wait(tried - 1, retry_after)).await;
        };
        let vectors: Vec<Vec<f32>> = match self.api {
            EmbedApi::Tei => answered.json().await?,
            EmbedApi::Openai => {
                let mut answered: OpenaiEmbeddings = answered.json().await?;
                answered.data.sort_by_key(|embedding| embedding.index);
                answered.data.into_iter().map(|e| e.embedding).collect()
            }
        };
        if vectors.len() != texts.len() {
            return Err(eyre!(
                "{} gave {} vectors for {} texts",
                self.name(),
                vectors.len(),
                texts.len()
            ));
        }
        Ok(vectors)
    }
}

/// How a seeding run embeds and paces.
#[derive(Clone)]
pub struct SeedOptions {
    /// An embedder outside the node, or `None` for the node's own model.
    pub outside: Option<Seeding>,
    /// The most objects stored per second; `0` for no limit.
    pub per_sec: u32,
    /// Texts embedded both ways before an outside embedder is trusted.
    pub sample: usize,
    /// Fill every repo again from the graph, whatever is recorded of its
    /// vectors.
    pub force: bool,
    /// Take the tenant's code vectors over for this code graph from the
    /// one they were kept for, and fill them again.
    pub take_over: bool,
    /// How many repos are filled at once.
    pub repos_at_once: usize,
    /// Told what the run is doing where it would otherwise be silent for
    /// a while.
    pub note: fn(&str),
}

impl SeedOptions {
    /// One repo at a time, nothing filled that is complete, saying nothing.
    pub fn of(outside: Option<Seeding>, per_sec: u32, sample: usize) -> Self {
        Self {
            outside,
            per_sec,
            sample,
            force: false,
            take_over: false,
            repos_at_once: 1,
            note: |_| {},
        }
    }
}

/// What seeding one repo's code vectors came to.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct SeededRepo {
    pub repo_id: String,
    pub name: String,
    /// What the store should hold, by the graph, and what it does.
    pub confirmed: Confirmed,
}

/// The texts of up to `sample` entities and summaries of the graph, as
/// the store embeds them, from all over it: entity ids are hashes, so a
/// page after a random one is a random page. Every other page is of
/// summaries, where the graph has any: they are prose, which a model may
/// embed otherwise than code.
async fn sample_texts(service: &KnowledgeService, sample: usize) -> Result<Vec<String>> {
    let repos = service.graph().list_repositories().await?;
    let mut texts = Vec::new();
    for turn in 0..sample.max(1) * 4 {
        if texts.len() >= sample || repos.is_empty() {
            break;
        }
        let (repo, after) = {
            let mut random = rand::thread_rng();
            let repo = &repos[random.gen_range(0..repos.len())];
            (repo, format!("{:016x}", random.gen::<u64>()))
        };
        let graph = service.graph();
        let page: Vec<String> = if turn % 2 == 1 {
            let page = graph.summaries_page(&repo.id, &after, None, 8).await?;
            page.into_iter().map(|summary| summary.text).collect()
        } else {
            let page = graph.entities_page(&repo.id, &after, None, 8).await?;
            page.iter()
                .map(|entity| {
                    VectorCodeEntity::from_code_entity(entity, &repo.id, None).searchable_content
                })
                .collect()
        };
        texts.extend(page.into_iter().filter(|text| !text.trim().is_empty()));
    }
    texts.truncate(sample);
    Ok(texts)
}

/// Compare `seeding`'s embedder with the node over `texts`, and refuse it
/// unless it embeds as the node does.
async fn trusted(
    tenants: &workforce_vellev::Tenants,
    embedding: &vellev_core::Embedding,
    seeding: &Seeding,
    texts: &[String],
) -> Result<Equivalence> {
    let measured = equivalence(tenants, embedding, seeding.embedder.as_ref(), texts).await?;
    measured.checked(&seeding.embedder.name(), MIN_COSINE)?;
    Ok(measured)
}

/// How a seeding run is paced. With an outside embedder and no limit on
/// the rate, pages are as long as the endpoint is sent at once, and enough
/// of them are stored at a time that one repo keeps it busy. With the
/// node's own model, or a limit, a page at a time, as the hub fills.
fn pace_of(options: &SeedOptions) -> FillPace {
    let mut pace = FillPace::at(options.per_sec);
    if let (Some(seeding), 0) = (&options.outside, options.per_sec) {
        pace.page = seeding.page();
        pace.ahead = seeding.pages_at_a_time();
    }
    pace
}

/// What `workforce code reindex-vectors` does: bring the code vectors in
/// vellev in line with the code graph, for every indexed repo or `only`
/// one, and confirm each against the graph.
///
/// Nothing is crawled and nothing deleted first: what is stored is stored
/// again under the same id, a fill keeps its place in the graph, and a run
/// that is stopped goes on from there when run again, so it may run before
/// a cutover, alongside the hub, and as often as needed. With
/// `options.outside`, vectors are made by that embedder, once a sample of
/// the graph's own texts shows it embeds as the node does; `checked` is
/// told what was measured. `Err` if any repo was left incomplete.
pub async fn reindex_code_vectors(
    config: &WorkforceConfig,
    only: Option<&str>,
    options: SeedOptions,
    checked: &(dyn Fn(&Equivalence) + Send + Sync),
    progress: &(dyn Fn(&str, &VectorFill) + Send + Sync),
) -> Result<Vec<SeededRepo>> {
    if !config.code_vectors_in_vellev() {
        return Err(eyre!(
            "code vectors are not kept in vellev: [knowledge] vector_backend is \"{}\". Set it to \"vellev\" for this run (WORKFORCE_KNOWLEDGE_VECTOR_BACKEND=vellev) to seed vellev before the hub is switched to it",
            config.knowledge.vector_backend
        ));
    }
    let vellev = &config.vellev;
    let key = vellev
        .api_key
        .as_deref()
        .ok_or_else(|| eyre!("[vellev] api_key is not set"))?;
    let mut store = VellevCodeVectorStore::new(&vellev.url, key, &vellev.tenant)?;
    let plain = VellevCodeVectorStore::new(&vellev.url, key, &vellev.tenant)?;
    if let Some(seeding) = &options.outside {
        store = store.seeded_by(seeding.clone());
    }
    let store: Arc<dyn CodeVectorStore> = Arc::new(store);
    store.ensure_collections().await?;

    let root = config
        .knowledge
        .root
        .clone()
        .unwrap_or_else(|| config.hub.data_dir.clone());
    let data_dir = config
        .knowledge
        .data_dir
        .clone()
        .unwrap_or_else(|| root.clone());
    let service = KnowledgeService::new_with_backend(
        root.into(),
        data_dir.into(),
        &config.knowledge,
        VectorBackend::Given(Arc::clone(&store)),
        &config.codegraph,
        config.postgres.as_ref(),
        None,
    )?;

    if let Some(seeding) = &options.outside {
        let texts = sample_texts(&service, options.sample).await?;
        (options.note)(&format!(
            "comparing {} with the node over {} texts before anything is written: the node embeds them with its own model, which may take a minute",
            seeding.embedder.name(),
            texts.len() + workforce_vellev::seeding::MIN_SAMPLE
        ));
        let (tenants, embedding) = plain.embedded_as()?;
        checked(&trusted(tenants, &embedding, seeding, &texts).await?);
    }
    if options.take_over {
        (options.note)("taking the tenant's code vectors over for this code graph");
        store.take_over().await?;
    }

    let run = FillRun {
        force: options.force || options.take_over,
        at_once: options.repos_at_once.max(1),
        ..FillRun::at(pace_of(&options))
    };
    (options.note)(match run.force {
        true => "filling every repo's vectors again from the code graph",
        false => "filling the vectors of repos that are not complete, and comparing the rest with the code graph",
    });
    let pass = service.fill(only, run, progress).await?;
    let filler = VectorFiller::new(Arc::clone(service.graph()), Arc::clone(&store));
    let mut seeded = Vec::new();
    for repo in service.vector_status().await? {
        if only.is_some_and(|only| only != repo.repo_id) {
            continue;
        }
        seeded.push(SeededRepo {
            confirmed: filler.confirm(&repo.repo_id).await?,
            repo_id: repo.repo_id,
            name: repo.name,
        });
    }
    if !pass.failed.is_empty() {
        let why: Vec<String> = service
            .vector_status()
            .await?
            .into_iter()
            .filter(|repo| pass.failed.contains(&repo.repo_id))
            .map(|repo| format!("{}: {}", repo.repo_id, repo.last_error.unwrap_or_default()))
            .collect();
        return Err(eyre!(
            "the code vectors of {} repo(s) are not complete; run this again to go on from where each stopped. {}",
            pass.failed.len(),
            why.join("; ")
        ));
    }
    if only.is_some() && seeded.is_empty() {
        return Err(eyre!(
            "no indexed repo '{}' in the code graph",
            only.unwrap_or_default()
        ));
    }
    Ok(seeded)
}

/// As [`trusted`], for the artifact index: over the texts of up to
/// `sample` artifacts.
pub(crate) async fn artifacts_trusted(
    tenants: &workforce_vellev::Tenants,
    embedding: &vellev_core::Embedding,
    seeding: &Seeding,
    texts: &[String],
) -> Result<Equivalence> {
    trusted(tenants, embedding, seeding, texts).await
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;

    use workforce_codebase_knowledge::graph::vectors::FillStage;
    use workforce_common::settings::KnowledgeConfig;
    use workforce_vellev::testing::{ByNode, Node};

    use super::*;

    async fn serving(app: axum::Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        url
    }

    /// Both APIs are asked for a batch at once, with no prefix, and their
    /// vectors taken in the order of the texts; a refusal is told.
    #[tokio::test]
    async fn an_outside_embedder_speaks_tei_and_openai() {
        use axum::routing::post;
        use axum::Json;
        let tei = serving(axum::Router::new().route(
            "/embed",
            post(|Json(asked): Json<serde_json::Value>| async move {
                assert_eq!(asked["truncate"], true);
                let made: Vec<Vec<f32>> = asked["inputs"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|text| vec![text.as_str().unwrap().len() as f32])
                    .collect();
                Json(made)
            }),
        ))
        .await;
        let texts = vec!["a".to_string(), "bbb".to_string()];
        let made = Outside::new(&tei, EmbedApi::Tei, "bge", None)
            .unwrap()
            .documents(&texts)
            .await
            .unwrap();
        assert_eq!(made, [[1.0], [3.0]]);

        let openai = serving(axum::Router::new().route(
            "/v1/embeddings",
            post(
                |headers: axum::http::HeaderMap, Json(asked): Json<serde_json::Value>| async move {
                    assert_eq!(headers["authorization"], "Bearer secret");
                    assert_eq!(asked["model"], "bge");
                    // Out of order, as the API allows.
                    let mut data: Vec<_> = asked["input"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .enumerate()
                        .map(|(index, text)| {
                            serde_json::json!({
                                "index": index,
                                "embedding": [text.as_str().unwrap().len()]
                            })
                        })
                        .collect();
                    data.reverse();
                    Json(serde_json::json!({ "data": data }))
                },
            ),
        ))
        .await;
        let keyed = Outside::new(
            &format!("{openai}/v1/"),
            EmbedApi::Openai,
            "bge",
            Some("secret".into()),
        )
        .unwrap();
        assert_eq!(keyed.documents(&texts).await.unwrap(), [[1.0], [3.0]]);
        assert!(keyed.name().contains("bge"));

        let down = serving(
            axum::Router::new()
                .fallback(|| async { (axum::http::StatusCode::TOO_MANY_REQUESTS, "slow down") }),
        )
        .await;
        let hasty = Again {
            tries: 3,
            first: Duration::from_millis(1),
            longest: Duration::from_millis(5),
        };
        let refused = Outside::new(&down, EmbedApi::Tei, "bge", None)
            .unwrap()
            .trying(hasty)
            .documents(&texts)
            .await
            .unwrap_err()
            .to_string();
        assert!(
            refused.contains("429")
                && refused.contains("slow down")
                && refused.contains("asked 3 times"),
            "{refused}"
        );
        assert!("grpc".parse::<EmbedApi>().is_err());
    }

    /// An endpoint that answers `status` to its first `busy` requests,
    /// with `retry_after` if any, and embeds after; and how many requests
    /// it has had.
    async fn busy_then_embedding(
        status: u16,
        busy: usize,
        retry_after: Option<&'static str>,
    ) -> (String, Arc<std::sync::atomic::AtomicUsize>) {
        use axum::response::IntoResponse;
        let asked = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = Arc::clone(&asked);
        let url = serving(axum::Router::new().route(
            "/embed",
            axum::routing::post(move |axum::Json(sent): axum::Json<serde_json::Value>| {
                let counted = Arc::clone(&counted);
                async move {
                    if counted.fetch_add(1, Ordering::SeqCst) < busy {
                        let status = axum::http::StatusCode::from_u16(status).unwrap();
                        return match retry_after {
                            Some(after) => {
                                (status, [("retry-after", after)], "Model is overloaded")
                                    .into_response()
                            }
                            None => (status, "Model is overloaded").into_response(),
                        };
                    }
                    let texts = sent["inputs"].as_array().unwrap().len();
                    axum::Json(vec![vec![1.0f32]; texts]).into_response()
                }
            }),
        ))
        .await;
        (url, asked)
    }

    /// An endpoint too busy for a request (429, as
    /// text-embeddings-inference answers when more texts are waiting than
    /// it holds, or 503) is asked again, after the wait it names or one
    /// that grows, and the vectors it then makes are taken: a seeding run
    /// does not fail, nor flood it, because it was busy. What it refuses
    /// for another reason is not asked for twice.
    #[tokio::test]
    async fn an_endpoint_that_is_busy_is_asked_again_after_a_wait() {
        let texts = vec!["a".to_string()];
        let again = Again {
            tries: 5,
            first: Duration::from_millis(80),
            longest: Duration::from_millis(300),
        };
        let embedder = |url: &str| {
            Outside::new(url, EmbedApi::Tei, "bge", None)
                .unwrap()
                .trying(again)
        };

        // Waits that grow: 80 ms and more, then 160 ms and more.
        let (url, asked) = busy_then_embedding(429, 2, None).await;
        let began = std::time::Instant::now();
        assert_eq!(embedder(&url).documents(&texts).await.unwrap(), [[1.0]]);
        assert_eq!(asked.load(Ordering::SeqCst), 3);
        // Each upper bound below is far from what the wrong wait would
        // take, so that a loaded machine is not mistaken for one.
        let slow = Duration::from_secs(10);
        let waited = began.elapsed();
        assert!(
            waited >= Duration::from_millis(240) && waited < slow,
            "{waited:?}"
        );

        // The wait the endpoint names, where it names one: half a minute,
        // cut to the longest this embedder waits.
        let (url, asked) = busy_then_embedding(503, 1, Some("30")).await;
        let began = std::time::Instant::now();
        assert_eq!(embedder(&url).documents(&texts).await.unwrap(), [[1.0]]);
        assert_eq!(asked.load(Ordering::SeqCst), 2);
        let waited = began.elapsed();
        assert!(
            waited >= again.longest,
            "the named wait not heeded: {waited:?}"
        );
        assert!(waited < slow, "the named wait, not cut: {waited:?}");
        // None, where it names none: by an embedder whose own first wait
        // is half a minute.
        let patient = Again {
            first: Duration::from_secs(30),
            longest: Duration::from_secs(30),
            ..again
        };
        let (url, _) = busy_then_embedding(429, 1, Some("0")).await;
        let began = std::time::Instant::now();
        Outside::new(&url, EmbedApi::Tei, "bge", None)
            .unwrap()
            .trying(patient)
            .documents(&texts)
            .await
            .unwrap();
        let waited = began.elapsed();
        assert!(waited < slow, "not at once, as it said: {waited:?}");

        // So is one a server fails at for a moment, and one that does not
        // reach it: here nothing listens until the second try is due.
        for failing in [500, 502, 504] {
            let (url, asked) = busy_then_embedding(failing, 1, None).await;
            assert_eq!(embedder(&url).documents(&texts).await.unwrap(), [[1.0]]);
            assert_eq!(asked.load(Ordering::SeqCst), 2, "{failing}");
        }
        let port = {
            let free = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            free.local_addr().unwrap().port()
        };
        let late = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(40)).await;
            let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
                .await
                .unwrap();
            let app = axum::Router::new().route(
                "/embed",
                axum::routing::post(|| async { axum::Json(vec![vec![1.0f32]]) }),
            );
            axum::serve(listener, app).await.unwrap();
        });
        let reached = embedder(&format!("http://127.0.0.1:{port}"));
        assert_eq!(reached.documents(&texts).await.unwrap(), [[1.0]]);
        late.abort();
        let never = embedder("http://127.0.0.1:9").documents(&texts).await;
        let never = format!("{:#}", never.unwrap_err());
        assert!(
            never.contains("cannot be reached (tried 5 times)"),
            "{never}"
        );

        // Not for ever, and not what is refused for another reason.
        let (url, asked) = busy_then_embedding(429, 100, None).await;
        let refused = embedder(&url).documents(&texts).await.unwrap_err();
        assert!(refused.to_string().contains("asked 5 times"), "{refused}");
        assert_eq!(asked.load(Ordering::SeqCst), 5);
        let (url, asked) = busy_then_embedding(413, 100, None).await;
        let refused = embedder(&url).documents(&texts).await.unwrap_err();
        assert!(refused.to_string().contains("413"), "{refused}");
        assert_eq!(asked.load(Ordering::SeqCst), 1);
    }

    /// The hub tests CI runs against a vellev node are named in a file,
    /// since the rest of the hub's suite has run already by then: every
    /// test that asks for a node is named there, and nothing else is, so
    /// that none is left out without anyone noticing.
    #[test]
    fn the_node_tests_ci_runs_are_the_ones_that_need_a_node() {
        fn sources(dir: &std::path::Path, found: &mut Vec<std::path::PathBuf>) {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    sources(&path, found);
                } else if path.extension().is_some_and(|rs| rs == "rs") {
                    found.push(path);
                }
            }
        }
        let mut files = Vec::new();
        sources(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
            &mut files,
        );
        // Spelt so that this test is not one of them.
        let asks: Vec<String> = ["fresh", "embedding"]
            .iter()
            .map(|how| format!("Node::{how}()"))
            .collect();
        // Every function that asks for a node, with whether it is a test
        // and whether it is one that is not run.
        let mut need: Vec<String> = Vec::new();
        let mut helpers: Vec<String> = Vec::new();
        let mut tests: Vec<String> = Vec::new();
        for file in files {
            let (mut within, mut a_test, mut ignored) = (String::new(), false, false);
            let (mut next_is_test, mut next_is_ignored) = (false, false);
            for line in std::fs::read_to_string(&file).unwrap().lines() {
                let line = line.trim_start();
                if line.starts_with("#[test]") || line.starts_with("#[tokio::test") {
                    next_is_test = true;
                }
                if line.starts_with("#[ignore") {
                    next_is_ignored = true;
                }
                let declared = line.trim_start_matches("pub(crate) ");
                let declared = declared.trim_start_matches("pub ");
                let declared = declared.trim_start_matches("async ");
                if let Some(named) = declared.strip_prefix("fn ") {
                    let name = named.split(['(', '<']).next().unwrap_or_default();
                    within = name.to_string();
                    (a_test, ignored) = (next_is_test, next_is_ignored);
                    (next_is_test, next_is_ignored) = (false, false);
                }
                if a_test && !within.is_empty() {
                    tests.push(within.clone());
                }
                if asks.iter().any(|ask| line.contains(ask.as_str())) {
                    match (a_test, ignored) {
                        (true, false) => need.push(within.clone()),
                        (true, true) => {}
                        (false, _) => helpers.push(within.clone()),
                    }
                }
            }
        }
        // A helper that asked for the node would stand in the list for the
        // tests that call it, which would go unlisted and unrun.
        assert!(
            helpers.is_empty(),
            "a test asks for its node itself, not through a helper: {helpers:?}"
        );
        need.sort();
        need.dedup();
        let mut named: Vec<String> = include_str!("../vellev-node-tests.txt")
            .lines()
            .filter(|line| !line.starts_with('#') && !line.trim().is_empty())
            .map(|line| line.trim().to_string())
            .collect();
        named.sort();
        tests.sort();
        tests.dedup();
        // Only a test is listed: a helper's name would satisfy a list of
        // names and run nothing.
        let not_tests: Vec<&String> = named.iter().filter(|n| !tests.contains(n)).collect();
        assert!(not_tests.is_empty(), "listed and not tests: {not_tests:?}");
        assert!(need.len() >= 20, "{need:?}");
        assert_eq!(
            named, need,
            "crates/hub/vellev-node-tests.txt names the hub tests that ask for a vellev node"
        );
    }

    /// A run with an outside embedder stores pages as long as the endpoint
    /// is sent at once, several at a time; one the node embeds for, or
    /// one held to a rate, a page at a time.
    #[test]
    fn the_pace_of_a_seeding_run_keeps_an_outside_embedder_busy() {
        struct Never;
        #[async_trait]
        impl DocumentEmbedder for Never {
            fn name(&self) -> String {
                "never asked".into()
            }
            async fn documents(&self, _texts: &[String]) -> Result<Vec<Vec<f32>>> {
                Err(eyre!("never asked"))
            }
        }
        let outside = || Some(Seeding::new(Arc::new(Never), 256, 16));
        let unpaced = pace_of(&SeedOptions::of(outside(), 0, 200));
        assert_eq!((unpaced.per_sec, unpaced.page, unpaced.ahead), (0, 4096, 4));
        let held_back = pace_of(&SeedOptions::of(outside(), 50, 200));
        assert_eq!((held_back.per_sec, held_back.ahead), (50, 1));
        assert!(held_back.page < 100, "small pages: {}", held_back.page);
        let by_the_node = pace_of(&SeedOptions::of(None, 0, 200));
        assert_eq!(by_the_node.ahead, 1);
    }

    /// An artifact search is given a deadline for vellev's index and none
    /// for Weaviate's, which had none on every release before.
    #[test]
    fn artifact_search_wait_is_for_vellev_alone() {
        let mut config = WorkforceConfig::default();
        config.weaviate.enabled = true;
        assert_eq!(crate::serve::artifact_search_wait(&config), None);
        config.artifacts.index_backend = "vellev".into();
        config.artifacts.index_search_timeout_ms = 2500;
        let (deadline, rest) = crate::serve::artifact_search_wait(&config).unwrap();
        assert_eq!(deadline, Duration::from_millis(2500));
        assert_eq!(rest, workforce_orchestrator::artifact_store::SEARCH_REST);
    }

    /// The keeper of the artifact index gives one try at one artifact
    /// thirty seconds on a vellev node, whose client would wait minutes;
    /// and otherwise keeps its own pace: half a minute after a round in
    /// which the index took nothing, an artifact tried again after half a
    /// minute at first and a quarter of an hour at the most.
    #[test]
    fn the_artifact_index_is_kept_at_the_pace_for_its_backend() {
        use workforce_orchestrator::artifact_store::IndexPace;
        let mut config = WorkforceConfig::default();
        config.weaviate.enabled = true;
        let weaviate = crate::serve::artifact_index_pace(&config);
        assert_eq!(weaviate.deadline, IndexPace::DEFAULT.deadline);
        config.artifacts.index_backend = "vellev".into();
        let vellev = crate::serve::artifact_index_pace(&config);
        assert_eq!(vellev.deadline, Duration::from_secs(30));
        for pace in [weaviate, vellev] {
            assert_eq!(pace.retry, Duration::from_secs(30));
            assert_eq!(pace.watch, Duration::from_secs(600));
            assert_eq!(pace.backoff, Duration::from_secs(30));
            assert_eq!(pace.backoff_cap, Duration::from_secs(900));
        }
    }

    /// The sample an outside embedder is compared over holds summaries as
    /// well as entities, where the graph has any: they are embedded too,
    /// and are prose.
    #[tokio::test]
    async fn the_sample_an_embedder_is_compared_over_holds_summaries_too() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        std::fs::create_dir_all(root.join("src")).unwrap();
        let source: String = (0..40)
            .map(|n| {
                format!("/// Does thing {n}.\npub fn thing_{n}() -> usize {{\n    {n}\n}}\n\n")
            })
            .collect();
        std::fs::write(root.join("src/lib.rs"), source).unwrap();
        let service = KnowledgeService::new_with_backend(
            root.clone(),
            dir.path().join("data"),
            &KnowledgeConfig::default(),
            VectorBackend::Weaviate(&Default::default()),
            &Default::default(),
            None,
            None,
        )
        .unwrap();
        service.trigger_full_index_for(&root).await.unwrap();
        let repo = KnowledgeService::repo_id_for(&root);
        let entities = service.graph().all_entities(&repo).await.unwrap();
        for entity in &entities {
            let text = format!("A summary, in prose, of {}.", entity.name);
            service
                .graph()
                .upsert_summary(&entity.id, "function", &text, "hash", "model")
                .await
                .unwrap();
        }
        let sample = sample_texts(&service, 60).await.unwrap();
        let summaries = sample.iter().filter(|t| t.starts_with("A summary")).count();
        assert_eq!(sample.len(), 60);
        assert!(
            summaries >= 10 && sample.len() - summaries >= 10,
            "{summaries} summaries of {}",
            sample.len()
        );
    }

    /// A small repo indexed into a SQLite code graph under `dir`, and the
    /// config that keeps its vectors in `node`'s tenant.
    async fn indexed(node: &Node, dir: &std::path::Path) -> (WorkforceConfig, String) {
        let root = dir.join("repo");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join("src/config.rs"),
            "/// Reads the TOML configuration file.\npub fn parse_config(path: &str) -> String {\n    path.to_string()\n}\n\n/// Sends a message over SMTP.\npub fn send_email(to: &str) -> bool {\n    !to.is_empty()\n}\n",
        )
        .unwrap();
        let mut config = WorkforceConfig::default();
        config.knowledge = KnowledgeConfig {
            root: Some(root.to_string_lossy().to_string()),
            data_dir: Some(dir.join("data").to_string_lossy().to_string()),
            vector_backend: "vellev".into(),
            ..KnowledgeConfig::default()
        };
        config.vellev.url = node.url.clone();
        config.vellev.api_key = Some(node.key.clone());
        config.vellev.tenant = node.tenant.clone();
        let service = KnowledgeService::new_with_backend(
            root.clone(),
            dir.join("data"),
            &config.knowledge,
            VectorBackend::of(&config),
            &config.codegraph,
            None,
            None,
        )
        .unwrap();
        service.trigger_full_index_for(&root).await.unwrap();
        (config, KnowledgeService::repo_id_for(&root))
    }

    fn options(outside: Option<Arc<ByNode>>) -> SeedOptions {
        SeedOptions::of(outside.map(|embedder| Seeding::new(embedder, 2, 2)), 0, 20)
    }

    /// The code vectors are seeded from the graph with vectors an outside
    /// embedder makes, once it is shown to embed as the node does, and what
    /// vellev then holds is found by a question the node embeds; run
    /// again, it has nothing to do. An embedder that embeds otherwise is
    /// refused before anything is written.
    #[tokio::test]
    async fn code_vectors_are_seeded_with_vectors_made_outside_the_node() {
        let Some(node) = Node::embedding().await else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let (config, repo) = indexed(&node, dir.path()).await;
        let store = VellevCodeVectorStore::new(&node.url, &node.key, &node.tenant).unwrap();

        let otherwise = Arc::new(ByNode {
            prefix: "search_document: ".into(),
            ..ByNode::of(&node)
        });
        let refused =
            reindex_code_vectors(&config, None, options(Some(otherwise)), &|_| {}, &|_, _| {})
                .await
                .unwrap_err()
                .to_string();
        assert!(
            refused.contains("does not embed as the node does"),
            "{refused}"
        );
        assert_eq!(
            store.entity_count(&repo).await.unwrap(),
            0,
            "nothing written"
        );

        let outside = Arc::new(ByNode::of(&node));
        let measured = std::sync::Mutex::new(None);
        let pages = std::sync::Mutex::new(Vec::new());
        let seeded = reindex_code_vectors(
            &config,
            None,
            options(Some(Arc::clone(&outside))),
            &|equivalence| *measured.lock().unwrap() = Some(equivalence.clone()),
            &|repo, fill| pages.lock().unwrap().push((repo.to_string(), fill.stage)),
        )
        .await
        .unwrap();
        let measured = measured.into_inner().unwrap().expect("compared first");
        assert!(
            measured.compared > 0 && measured.lowest > 0.9999,
            "{measured:?}"
        );
        assert_eq!(seeded.len(), 1);
        assert_eq!(seeded[0].repo_id, repo);
        assert!(seeded[0].confirmed.agrees(), "{:?}", seeded[0].confirmed);
        let stored = seeded[0].confirmed.entities_stored;
        assert!(stored > 1);
        assert_eq!(pages.lock().unwrap().last().unwrap().1, FillStage::Done);
        let asked = outside.asked.load(Ordering::SeqCst);
        assert!(asked >= measured.compared + stored, "made outside: {asked}");

        let found = store
            .search_entities("reading the configuration file", Some(&repo), None, 3)
            .await
            .unwrap();
        assert!(
            found[0].qualified_name.contains("parse_config"),
            "found by a question the node embeds: {found:?}"
        );

        let again = reindex_code_vectors(&config, Some(&repo), options(None), &|_| {}, &|_, _| {})
            .await
            .unwrap();
        assert_eq!(again, seeded, "complete already");
        assert_eq!(outside.asked.load(Ordering::SeqCst), asked);

        // Forced, every vector is made and written again, whatever is
        // recorded; and a stray the graph has not is gone at the end.
        let mut stray = VectorCodeEntity::from_code_entity(
            &KnowledgeService::new_with_backend(
                dir.path().join("repo"),
                dir.path().join("data"),
                &config.knowledge,
                VectorBackend::Given(Arc::new(
                    VellevCodeVectorStore::new(&node.url, &node.key, &node.tenant).unwrap(),
                )),
                &config.codegraph,
                None,
                None,
            )
            .unwrap()
            .graph()
            .all_entities(&repo)
            .await
            .unwrap()[0],
            &repo,
            None,
        );
        stray.entity_id = "gone::long_ago".into();
        stray.qualified_name = "gone::long_ago".into();
        store.index_entities(&[stray]).await.unwrap();
        assert_eq!(store.entity_count(&repo).await.unwrap(), stored + 1);
        let forced = SeedOptions {
            force: true,
            ..options(Some(Arc::clone(&outside)))
        };
        let pages = std::sync::Mutex::new(0);
        let again = reindex_code_vectors(&config, Some(&repo), forced, &|_| {}, &|_, _| {
            *pages.lock().unwrap() += 1
        })
        .await
        .unwrap();
        assert_eq!(again, seeded);
        assert!(*pages.lock().unwrap() > 0, "filled again");
        assert!(outside.asked.load(Ordering::SeqCst) >= asked + stored);
        assert_eq!(store.entity_count(&repo).await.unwrap(), stored);
        let no_such =
            reindex_code_vectors(&config, Some("nope"), options(None), &|_| {}, &|_, _| {})
                .await
                .unwrap_err()
                .to_string();
        assert!(no_such.contains("no indexed repo 'nope'"), "{no_such}");

        let mut elsewhere = config.clone();
        elsewhere.knowledge.vector_backend = "weaviate".into();
        let refused = reindex_code_vectors(&elsewhere, None, options(None), &|_| {}, &|_, _| {})
            .await
            .unwrap_err()
            .to_string();
        assert!(
            refused.contains("WORKFORCE_KNOWLEDGE_VECTOR_BACKEND=vellev"),
            "{refused}"
        );
    }

    /// What a seeding run says it is doing, for the test below.
    static SAID: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

    /// A tenant whose code vectors are another code graph's is not seeded:
    /// the run fails saying whose they are and what to do. Told to take
    /// them over, it does, and fills them. And a run says what it is doing
    /// where it would otherwise be silent: while the node embeds the
    /// sample, and when it begins to fill.
    #[tokio::test]
    async fn a_tenants_code_vectors_are_taken_over_at_an_operators_word() {
        let Some(node) = Node::embedding().await else {
            return;
        };
        let (first_dir, second_dir) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let (first, repo) = indexed(&node, first_dir.path()).await;
        reindex_code_vectors(&first, None, options(None), &|_| {}, &|_, _| {})
            .await
            .unwrap();
        let (second, others) = indexed(&node, second_dir.path()).await;
        let refused = reindex_code_vectors(&second, None, options(None), &|_| {}, &|_, _| {})
            .await
            .unwrap_err()
            .to_string();
        assert!(
            refused.contains("another code graph") && refused.contains("--take-over"),
            "{refused}"
        );

        let taking = SeedOptions {
            take_over: true,
            note: |said| SAID.lock().unwrap().push(said.to_string()),
            ..options(Some(Arc::new(ByNode::of(&node))))
        };
        let seeded = reindex_code_vectors(&second, None, taking, &|_| {}, &|_, _| {})
            .await
            .unwrap();
        assert_eq!(seeded.len(), 1);
        assert_eq!(seeded[0].repo_id, others);
        assert!(seeded[0].confirmed.agrees(), "{:?}", seeded[0].confirmed);
        let said = SAID.lock().unwrap().clone();
        assert_eq!(said.len(), 3, "{said:?}");
        assert!(
            said[0].contains("comparing") && said[0].contains("may take a minute"),
            "{said:?}"
        );
        assert!(said[1].contains("taking the tenant's code vectors over"));
        assert!(said[2].contains("filling every repo's vectors again"));
        // The first graph's are its no longer.
        let refused = reindex_code_vectors(&first, Some(&repo), options(None), &|_| {}, &|_, _| {})
            .await
            .unwrap_err()
            .to_string();
        assert!(refused.contains("another code graph"), "{refused}");
    }

    /// With no outside embedder the node embeds, and the run that is
    /// stopped by a node that goes away goes on when run again.
    #[tokio::test]
    async fn a_seeding_run_that_fails_says_so_and_goes_on_when_run_again() {
        let Some(node) = Node::embedding().await else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let (config, repo) = indexed(&node, dir.path()).await;
        let mut away = config.clone();
        away.vellev.url = "http://127.0.0.1:1".into();
        assert!(
            reindex_code_vectors(&away, None, options(None), &|_| {}, &|_, _| {})
                .await
                .is_err()
        );
        let seeded = reindex_code_vectors(&config, None, options(None), &|_| {}, &|_, _| {})
            .await
            .unwrap();
        assert_eq!(seeded[0].repo_id, repo);
        assert!(seeded[0].confirmed.agrees());
    }

    /// Not a test: measures seeding against the node under test, for its
    /// throughput figures. `WORKFORCE_MEASURE_REPO` names a checkout
    /// to index; `WORKFORCE_MEASURE_SECS` how long each run is given;
    /// `WORKFORCE_MEASURE_EMBED_URL` a text-embeddings-inference endpoint
    /// serving bge-base-en-v1.5, to measure seeding through it as well and
    /// how near its vectors are to the node's.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "a measurement, run by hand"]
    async fn measure_seeding() {
        let Some(node) = Node::embedding().await else {
            return;
        };
        let repo_root = std::env::var("WORKFORCE_MEASURE_REPO").expect("a checkout to index");
        let secs: u64 = std::env::var("WORKFORCE_MEASURE_SECS")
            .ok()
            .and_then(|secs| secs.parse().ok())
            .unwrap_or(120);
        let dir = tempfile::tempdir().unwrap();
        let root = std::path::PathBuf::from(repo_root);
        let mut config = WorkforceConfig::default();
        config.knowledge = KnowledgeConfig {
            root: Some(root.to_string_lossy().to_string()),
            data_dir: Some(dir.path().join("data").to_string_lossy().to_string()),
            vector_backend: "vellev".into(),
            ..KnowledgeConfig::default()
        };
        config.vellev.url = node.url.clone();
        config.vellev.api_key = Some(node.key.clone());
        config.vellev.tenant = node.tenant.clone();
        let service = KnowledgeService::new_with_backend(
            root.clone(),
            dir.path().join("data"),
            &config.knowledge,
            VectorBackend::of(&config),
            &config.codegraph,
            None,
            None,
        )
        .unwrap();
        let began = std::time::Instant::now();
        service.trigger_full_index_for(&root).await.unwrap();
        let repo = KnowledgeService::repo_id_for(&root);
        let entities = service.graph().get_stats(&repo).await.unwrap().entity_count;
        println!(
            "MEASURE indexed {entities} entities in {:?}",
            began.elapsed()
        );
        let store =
            Arc::new(VellevCodeVectorStore::new(&node.url, &node.key, &node.tenant).unwrap());

        // How long a question takes to be embedded and answered, asked
        // every two seconds while `running`.
        let asking = |running: Arc<std::sync::atomic::AtomicBool>| {
            let store = Arc::clone(&store);
            let repo = repo.clone();
            tokio::spawn(async move {
                let mut took = Vec::new();
                while running.load(Ordering::SeqCst) {
                    // A new question each time: the node keeps the vectors
                    // of questions it has embedded lately.
                    let question = format!(
                        "how is the configuration read {}",
                        uuid::Uuid::new_v4().simple()
                    );
                    let began = std::time::Instant::now();
                    if let Err(e) = store.search_entities(&question, Some(&repo), None, 5).await {
                        println!("MEASURE a question failed: {e:#}");
                    }
                    took.push(began.elapsed().as_millis() as u64);
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
                took.sort();
                took
            })
        };
        let quiet = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let idle = asking(Arc::clone(&quiet));
        tokio::time::sleep(Duration::from_secs(20)).await;
        quiet.store(false, Ordering::SeqCst);
        let idle = idle.await.unwrap();
        println!("MEASURE question latency, node idle (ms, sorted): {idle:?}");

        // What vellev takes when the vectors cost nothing to make (the most
        // any outside embedder can reach), and what it takes from an
        // embedder that answers each request after a fixed while: if
        // `--embed-concurrency` requests are kept with it across pages,
        // that is concurrency x batch / the while, up to the ceiling.
        struct ReadyMade(Duration);
        #[async_trait]
        impl DocumentEmbedder for ReadyMade {
            fn name(&self) -> String {
                "vectors ready-made".into()
            }
            async fn documents(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
                if !self.0.is_zero() {
                    tokio::time::sleep(self.0).await;
                }
                Ok(texts
                    .iter()
                    .map(|text| {
                        let mut vector = vec![0.01f32; 768];
                        vector[text.len() % 768] = 1.0;
                        vector
                    })
                    .collect())
            }
        }
        let ms = Duration::from_millis;
        for (batch, concurrency, latency) in [
            (32usize, 8usize, ms(0)),
            (128, 8, ms(0)),
            (256, 16, ms(0)),
            (32, 4, ms(100)),
            (32, 8, ms(100)),
            (128, 8, ms(400)),
        ] {
            let name_of = |collection: &str| vellev_core::CollectionName::new(collection).unwrap();
            for collection in ["CodeEntity", "CodeSummary"] {
                let _ = node.tenant().collection(&name_of(collection)).drop().await;
            }
            service.graph().clear_vector_fill(&repo).await.unwrap();
            let seeding = Seeding::new(Arc::new(ReadyMade(latency)), batch, concurrency);
            let seeded = VellevCodeVectorStore::new(&node.url, &node.key, &node.tenant)
                .unwrap()
                .seeded_by(seeding.clone());
            let seeded: Arc<dyn CodeVectorStore> = Arc::new(seeded);
            let given = KnowledgeService::new_with_backend(
                root.clone(),
                dir.path().join("data"),
                &config.knowledge,
                VectorBackend::Given(Arc::clone(&seeded)),
                &config.codegraph,
                None,
                None,
            )
            .unwrap();
            // As `workforce code reindex-vectors --embed-url` runs it.
            let run = FillRun {
                at_once: 4,
                ..FillRun::at(FillPace {
                    page: seeding.page(),
                    ahead: seeding.pages_at_a_time(),
                    ..FillPace::at(0)
                })
            };
            let began = std::time::Instant::now();
            let pass = given.fill(Some(&repo), run, &|_, _| {}).await.unwrap();
            let took = began.elapsed().as_secs_f64();
            let confirmed = VectorFiller::new(Arc::clone(given.graph()), Arc::clone(&seeded))
                .confirm(&repo)
                .await
                .unwrap();
            assert!(confirmed.agrees(), "every entity stored: {confirmed:?}");
            let stored = confirmed.entities_stored + confirmed.summaries_stored;
            let by_embedder = match latency.is_zero() {
                true => "no limit".to_string(),
                false => format!(
                    "{:.0}/s",
                    (batch * concurrency) as f64 / latency.as_secs_f64()
                ),
            };
            println!(
                "MEASURE ready-made vectors, {concurrency} x {batch}, {latency:?} a request (the embedder allows {by_embedder}), {} pages of {} at a time: {stored} stored in {took:.1}s = {:.0}/s (failed: {:?}); 4.2M at this rate: {:.1} h",
                seeding.pages_at_a_time(),
                seeding.page(),
                stored as f64 / took,
                pass.failed,
                4_200_000.0 / (stored as f64 / took) / 3600.0
            );

            // What comparing the whole repo with the graph costs, when
            // the two agree: a hub's first pass after it starts.
            if (batch, concurrency) == (256, 16) {
                let restarted = KnowledgeService::new_with_backend(
                    root.clone(),
                    dir.path().join("data"),
                    &config.knowledge,
                    VectorBackend::of(&config),
                    &config.codegraph,
                    None,
                    None,
                )
                .unwrap();
                let began = std::time::Instant::now();
                let pass = restarted
                    .fill(Some(&repo), FillRun::at(FillPace::at(0)), &|_, _| {})
                    .await
                    .unwrap();
                let took = began.elapsed().as_secs_f64();
                println!(
                    "MEASURE comparing {stored} vectors with the graph, unpaced: {took:.1}s = {:.0}/s (did anything: {:?}, failed: {:?}); 4.2M at this rate: {:.1} min",
                    stored as f64 / took,
                    pass.filled,
                    pass.failed,
                    4_200_000.0 / (stored as f64 / took) / 60.0
                );
            }
        }
        if std::env::var("WORKFORCE_MEASURE_ONLY").as_deref() == Ok("ceiling") {
            return;
        }

        let outside = std::env::var("WORKFORCE_MEASURE_EMBED_URL")
            .ok()
            .map(|url| {
                let embedder =
                    Outside::new(&url, EmbedApi::Tei, "BAAI/bge-base-en-v1.5", None).unwrap();
                Seeding::new(Arc::new(embedder), 32, 8)
            });
        let mut runs = vec![
            ("the node, unpaced", None, 0u32),
            ("the node, 5 a second", None, 5),
        ];
        if let Some(seeding) = &outside {
            runs.push(("outside, 8 x 32", Some(seeding.clone()), 0));
        }
        for (name, outside, per_sec) in runs {
            // Each run from an empty collection and a fresh fill.
            let name_of = |collection: &str| vellev_core::CollectionName::new(collection).unwrap();
            for collection in ["CodeEntity", "CodeSummary"] {
                let _ = node.tenant().collection(&name_of(collection)).drop().await;
            }
            service.graph().clear_vector_fill(&repo).await.unwrap();
            let stored = Arc::new(std::sync::atomic::AtomicU64::new(0));
            let counted = Arc::clone(&stored);
            let measured = Arc::new(std::sync::Mutex::new(None));
            let told = Arc::clone(&measured);
            let running = Arc::new(std::sync::atomic::AtomicBool::new(true));
            let questions = asking(Arc::clone(&running));
            let options = SeedOptions::of(outside, per_sec, 200);
            let on_checked =
                move |equivalence: &Equivalence| *told.lock().unwrap() = Some(equivalence.clone());
            let on_page =
                move |_: &str, fill: &VectorFill| counted.store(fill.done, Ordering::SeqCst);
            let began = std::time::Instant::now();
            let run = reindex_code_vectors(&config, Some(&repo), options, &on_checked, &on_page);
            let finished = match tokio::time::timeout(Duration::from_secs(secs), run).await {
                Ok(Ok(_)) => true,
                Ok(Err(e)) => {
                    println!("MEASURE {name}: failed: {e:#}");
                    false
                }
                Err(_) => false,
            };
            let took = began.elapsed().as_secs_f64();
            running.store(false, Ordering::SeqCst);
            let latencies = questions.await.unwrap();
            let stored = stored.load(Ordering::SeqCst);
            println!(
                "MEASURE {name}: {stored} stored in {took:.0}s = {:.1}/s (finished: {finished}); 4.2M at this rate: {:.1} h",
                stored as f64 / took,
                4_200_000.0 / (stored as f64 / took) / 3600.0
            );
            println!("MEASURE {name}: question latency meanwhile (ms, sorted): {latencies:?}");
            let equivalence = measured.lock().unwrap().clone();
            if let Some(equivalence) = equivalence {
                println!("MEASURE {name}: against the node: {equivalence:?}");
            }
        }
    }
}
