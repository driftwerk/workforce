//! What every workforce store on vellev shares: connecting to a tenant,
//! the node's own embedding model, the limit on one request, making sure a
//! collection exists in a shape the store can use, and markers.
//!
//! Artifacts are always embedded by the node's own bge ([`LOCAL_MODEL`]),
//! so the node must run it; memories are too, unless `[embeddings]` has the
//! hub embed them. Code is embedded with the model `[vellev.code]` names,
//! bge by default, and its node must run that one.

mod cap;
mod deletes;
mod ensure;
mod markers;
mod matching;
pub mod seeding;
mod shape;
mod signals;

use std::time::Duration;

use eyre::Result;
use vellev_client::api::Code;
use vellev_client::{Client, Options, Tenant};
use vellev_core::{
    Bm25, Collection, CollectionName, Distance, Embedding, EmbeddingProvider, Property, TenantId,
};

pub use cap::{Cap, MAX_RESULTS};
pub use deletes::{delete_where, DELETE_CAP};
pub use ensure::{
    classified, doctor, ensure, failure, identity, probe_model, Asked, Ensured, ModelUnavailable,
    Unusable, RECHECK_AFTER, RETRY_AFTER,
};
pub use markers::{
    index_claim, index_claimed, index_mark, index_marked, index_unmark, mark, mark_promoted,
    marked, marker_id, promoted, unmark, unmark_promoted, INDEX_MARKERS, MARKERS,
};
pub use matching::{unmatched, Unmatched};
pub use shape::{checked, mismatches, Refused};
/// What the stores filter by, for those that don't depend on the index
/// crate themselves.
pub use vellev_index::filter::Filter;

/// The model a collection the node embeds for runs.
pub const LOCAL_MODEL: &str = "BAAI/bge-base-en-v1.5";
pub const LOCAL_DIMENSIONS: u32 = 768;
/// What bge was trained to put before a query it retrieves for.
pub const LOCAL_QUERY_PREFIX: &str = "Represent this sentence for searching relevant passages: ";

/// How long one try of a write the node embeds may take. A batch of 100
/// memories or 50 code entities is embedded by the node's bge on its CPU
/// within the request; Weaviate's code store gave the same batches 120s,
/// four times the 30s a read is given.
pub const WRITE_TIMEOUT: Duration = Duration::from_secs(120);

/// How long one try of a request may take: a read, and a write the node
/// embeds; and how many tries a request is given in all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timeouts {
    pub read: Duration,
    pub write: Duration,
    /// Tries in all, at least one. One for a caller that has a deadline of
    /// its own and must not be held past it, as shadow mode's reads.
    pub attempts: u32,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            read: Options::default().timeout,
            write: WRITE_TIMEOUT,
            attempts: Options::default().attempts,
        }
    }
}

/// A tenant as reads see it, and as writes the node embeds see it. Both go
/// through one client, so a read sees what a write before it made.
#[derive(Debug, Clone)]
pub struct Tenants {
    pub reads: Tenant,
    pub writes: Tenant,
    /// The URL of the node they were reached at, without a trailing `/`.
    pub node: String,
}

/// `tenant` on the node at `url`, reached with `api_key`, with the default
/// [`Timeouts`]. Every request is bounded by its timeout and tried again
/// where that is safe.
pub fn connect(url: &str, api_key: &str, tenant: &str) -> Result<Tenants> {
    connect_with(url, api_key, tenant, Timeouts::default())
}

/// As [`connect`], with `timeouts`.
pub fn connect_with(url: &str, api_key: &str, tenant: &str, timeouts: Timeouts) -> Result<Tenants> {
    let options = Options::default()
        .timeout(timeouts.read)
        .attempts(timeouts.attempts.max(1));
    let client = Client::connect(&[url], api_key, options)?;
    let tenant = TenantId::new(tenant)?;
    Ok(Tenants {
        reads: client.tenant(&tenant),
        writes: client.within(timeouts.write).tenant(&tenant),
        node: url.trim_end_matches('/').to_string(),
    })
}

/// Embedding by the node's own bge, of `properties` in order.
pub fn local_embedding(properties: &[&str]) -> Embedding {
    Embedding {
        provider: EmbeddingProvider::Local,
        model: LOCAL_MODEL.to_string(),
        dimensions: LOCAL_DIMENSIONS,
        query_prefix: Some(LOCAL_QUERY_PREFIX.to_string()),
        document_prefix: None,
        properties: properties.iter().map(|p| p.to_string()).collect(),
    }
}

/// Embedding by the node's own `model`, of vectors of `dimensions`, with
/// `query_prefix` before a query and nothing before a document, of
/// `properties` in order.
pub fn embedding_of(
    model: &str,
    dimensions: u32,
    query_prefix: Option<&str>,
    properties: &[&str],
) -> Embedding {
    Embedding {
        model: model.to_string(),
        dimensions,
        query_prefix: query_prefix.map(str::to_string),
        ..local_embedding(properties)
    }
}

/// A collection measured by cosine, as every workforce collection is.
pub fn collection(
    name: CollectionName,
    properties: Vec<Property>,
    embedding: Embedding,
) -> Collection {
    Collection {
        name,
        properties,
        embedding,
        distance: Distance::Cosine,
        bm25: Bm25::default(),
        access_rule: None,
    }
}

/// Whether a write was refused because what is stored disagrees with it.
pub fn is_conflict(error: &vellev_client::Error) -> bool {
    error.code() == Some(Code::Conflict)
}

/// A real node for tests: `VELLEV_TEST_URL` and `VELLEV_TEST_KEY` name it,
/// and `VELLEV_TEST_LOCAL_EMBED=1` says it runs the local model. It must be
/// a throwaway node run with `VELLEV_MAX_FORMAT=2`, as CI runs one, never
/// one holding data you keep: the code tests make collections only format 2
/// holds, and a node's format, once raised, is never lowered. Tests of a
/// configured code model embed with bge-small as well, so the node must
/// allow it (`VELLEV_LOCAL_MODELS_ALLOWED=BAAI/bge-small-en-v1.5`).
#[cfg(feature = "testing")]
pub mod testing {
    use super::*;

    /// Whether the tests that need a node must run: with
    /// `VELLEV_TEST_REQUIRED=1`, as in CI, one that would skip itself for
    /// want of a node fails instead.
    fn required() -> bool {
        std::env::var("VELLEV_TEST_REQUIRED").as_deref() == Ok("1")
    }

    /// An embedder that has the node itself embed, as a server of the same
    /// model would: as `embedding` says (bge by default), with `prefix`
    /// before every text, or its vectors `scaled`, when told to be one that
    /// embeds otherwise.
    pub struct ByNode {
        pub tenants: Tenants,
        pub embedding: Embedding,
        pub prefix: String,
        pub scaled: f32,
        /// How many texts it was asked for.
        pub asked: std::sync::atomic::AtomicUsize,
    }

    impl ByNode {
        pub fn of(node: &Node) -> Self {
            Self {
                tenants: node.tenants(),
                embedding: local_embedding(&["text"]),
                prefix: String::new(),
                scaled: 1.0,
                asked: Default::default(),
            }
        }
    }

    #[async_trait::async_trait]
    impl seeding::DocumentEmbedder for ByNode {
        fn name(&self) -> String {
            "the node, asked as an embedding endpoint".to_string()
        }

        async fn documents(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
            self.asked
                .fetch_add(texts.len(), std::sync::atomic::Ordering::SeqCst);
            let prefixed: Vec<String> = texts
                .iter()
                .map(|text| format!("{}{text}", self.prefix))
                .collect();
            let node = seeding::NodeEmbedder::new(
                self.tenants.clone(),
                self.embedding.clone(),
                texts.len(),
            );
            Ok(node
                .documents(&prefixed)
                .await?
                .into_iter()
                .map(|vector| vector.into_iter().map(|x| x * self.scaled).collect())
                .collect())
        }
    }

    /// An embedder with no model behind it: each text's vector is made
    /// from its hash, the same for the same text, of `dimensions`. It
    /// counts the texts it is asked for, which is what was written whole.
    pub struct Counted {
        pub dimensions: usize,
        pub asked: std::sync::atomic::AtomicUsize,
    }

    impl Counted {
        pub fn new(dimensions: usize) -> Self {
            Self {
                dimensions,
                asked: Default::default(),
            }
        }

        pub fn asked(&self) -> usize {
            self.asked.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    /// A unit vector of `dimensions` made from `text`'s hash.
    fn hashed(text: &str, dimensions: usize) -> Vec<f32> {
        use std::hash::{Hash, Hasher};
        let spread: Vec<f32> = (0..dimensions)
            .map(|at| {
                let mut hasher = std::hash::DefaultHasher::new();
                (text, at).hash(&mut hasher);
                (hasher.finish() % 2_000) as f32 / 1_000.0 - 1.0
            })
            .collect();
        let norm = spread
            .iter()
            .map(|x| x * x)
            .sum::<f32>()
            .sqrt()
            .max(f32::EPSILON);
        spread.into_iter().map(|x| x / norm).collect()
    }

    #[async_trait::async_trait]
    impl seeding::DocumentEmbedder for Counted {
        fn name(&self) -> String {
            "a hash of each text".to_string()
        }

        async fn documents(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
            self.asked
                .fetch_add(texts.len(), std::sync::atomic::Ordering::SeqCst);
            Ok(texts
                .iter()
                .map(|text| hashed(text, self.dimensions))
                .collect())
        }
    }

    /// The model tests of a configured code model embed with, as a node
    /// under test allows it.
    pub const SMALL: &str = "BAAI/bge-small-en-v1.5";

    /// A local node that serves no tenant: it answers its health, and
    /// every request of a tenant with 421 `not_served_here`, counting those.
    pub struct NotServing {
        pub url: String,
        asked: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    /// What a node answers a request of a tenant it does not serve.
    pub fn not_served_here() -> axum::response::Response {
        use axum::response::IntoResponse;
        (
            axum::http::StatusCode::MISDIRECTED_REQUEST,
            axum::Json(serde_json::json!({ "error": {
                "code": "not_served_here",
                "message": "this node does not serve the tenant; ask a node that serves it",
            }})),
        )
            .into_response()
    }

    impl NotServing {
        pub async fn up() -> Self {
            use axum::response::IntoResponse;
            let asked = std::sync::Arc::<std::sync::atomic::AtomicUsize>::default();
            let counting = std::sync::Arc::clone(&asked);
            let app = axum::Router::new().fallback(move |uri: axum::http::Uri| {
                let counting = std::sync::Arc::clone(&counting);
                async move {
                    if uri.path().starts_with("/health") {
                        return axum::http::StatusCode::OK.into_response();
                    }
                    counting.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    not_served_here()
                }
            });
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("a free port");
            let url = format!("http://{}", listener.local_addr().expect("bound"));
            tokio::spawn(async move { axum::serve(listener, app).await });
            Self { url, asked }
        }

        /// The requests of a tenant it was sent.
        pub fn asked(&self) -> usize {
            self.asked.load(std::sync::atomic::Ordering::SeqCst)
        }

        pub fn tenants(&self, tenant: &str) -> Tenants {
            connect(&self.url, "key", tenant).expect("a usable fake node")
        }

        /// What a request of `tenant` here fails as.
        pub fn misrouted(&self, tenant: &str) -> Unusable {
            Unusable::Misrouted {
                tenant: tenant.to_string(),
                node: self.url.clone(),
            }
        }
    }

    /// What is logged on this thread, by a subscriber of the whole process:
    /// a test's events, on its own thread, are kept apart from every
    /// other's, and no test's subscriber can leave another's events
    /// unheard, as one of a thread's own can once a callsite was first
    /// reached on a thread without one.
    pub mod logged {
        use std::cell::RefCell;

        thread_local! {
            static LOGGED: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
        }

        struct ThisThread;

        impl std::io::Write for ThisThread {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                LOGGED.with(|logged| logged.borrow_mut().extend_from_slice(buf));
                Ok(buf.len())
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        /// Start keeping what this thread logs, from now.
        pub fn kept() {
            static SUBSCRIBED: std::sync::Once = std::sync::Once::new();
            SUBSCRIBED.call_once(|| {
                let subscriber = tracing_subscriber::fmt()
                    .with_writer(|| ThisThread)
                    .with_ansi(false)
                    .finish();
                tracing::subscriber::set_global_default(subscriber)
                    .expect("no other subscriber of the whole process");
            });
            LOGGED.with(|logged| logged.borrow_mut().clear());
        }

        /// What this thread logged since [`kept`].
        pub fn text() -> String {
            LOGGED.with(|logged| String::from_utf8_lossy(&logged.borrow()).into_owned())
        }

        /// The lines of [`text`] that hold `level`.
        pub fn at(level: &str) -> Vec<String> {
            text()
                .lines()
                .filter(|line| line.contains(level))
                .map(str::to_string)
                .collect()
        }

        /// What is logged on this thread while `f` runs.
        pub async fn during<F: std::future::Future>(f: F) -> String {
            kept();
            f.await;
            text()
        }
    }

    /// A fresh tenant on the node under test.
    pub struct Node {
        pub url: String,
        pub key: String,
        pub tenant: String,
    }

    impl Node {
        /// A fresh tenant, created with the admin key the node is reached
        /// with, as an operator creates one; or `None`, said why, to skip.
        pub async fn fresh() -> Option<Self> {
            let (Ok(url), Ok(key)) = (
                std::env::var("VELLEV_TEST_URL"),
                std::env::var("VELLEV_TEST_KEY"),
            ) else {
                assert!(
                    !required(),
                    "VELLEV_TEST_REQUIRED is set, and VELLEV_TEST_URL and VELLEV_TEST_KEY name no vellev node"
                );
                eprintln!("skipping: VELLEV_TEST_URL and VELLEV_TEST_KEY name no vellev node");
                return None;
            };
            let node = Self {
                url,
                key,
                tenant: format!("wf-test-{}", uuid::Uuid::new_v4().simple()),
            };
            node.tenant().create().await.expect("a test tenant");
            Some(node)
        }

        /// As [`Self::fresh`], on a node that runs the local model.
        pub async fn embedding() -> Option<Self> {
            if std::env::var("VELLEV_TEST_LOCAL_EMBED").as_deref() != Ok("1") {
                assert!(
                    !required(),
                    "VELLEV_TEST_REQUIRED is set, and VELLEV_TEST_LOCAL_EMBED=1 names no node with a local model"
                );
                eprintln!("skipping: VELLEV_TEST_LOCAL_EMBED=1 names no node with a local model");
                return None;
            }
            Self::fresh().await
        }

        pub fn tenants(&self) -> Tenants {
            connect(&self.url, &self.key, &self.tenant).expect("a usable test node")
        }

        pub fn tenant(&self) -> Tenant {
            self.tenants().reads
        }

        /// A local URL that answers nothing yet, as a node not up; see
        /// [`Self::come_up_at`].
        pub async fn not_up_yet() -> (String, u16) {
            let port = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .and_then(|listener| listener.local_addr())
                .expect("a free port")
                .port();
            (format!("http://127.0.0.1:{port}"), port)
        }

        /// From now on, pass connections to `port` on to the node, as a node
        /// coming up where a store was told to find it.
        pub async fn come_up_at(&self, port: u16) {
            let node = self
                .url
                .trim_start_matches("http://")
                .trim_end_matches('/')
                .to_string();
            let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
                .await
                .expect("the port is still free");
            tokio::spawn(async move {
                while let Ok((mut inbound, _)) = listener.accept().await {
                    let node = node.clone();
                    tokio::spawn(async move {
                        if let Ok(mut outbound) = tokio::net::TcpStream::connect(node).await {
                            let _ =
                                tokio::io::copy_bidirectional(&mut inbound, &mut outbound).await;
                        }
                    });
                }
            });
        }
    }

    /// The tenant goes with the test, passed or failed. `Drop` cannot wait
    /// on the async client, so a short-lived thread with its own runtime
    /// deletes it; a failure is reported, never a panic.
    impl Drop for Node {
        fn drop(&mut self) {
            let (url, key, tenant) = (self.url.clone(), self.key.clone(), self.tenant.clone());
            let deleted = std::thread::spawn(move || -> Result<()> {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()?;
                runtime.block_on(async {
                    connect(&url, &key, &tenant)?.reads.drop().await?;
                    Ok(())
                })
            })
            .join();
            match deleted {
                Ok(Ok(())) => {}
                Ok(Err(e)) => eprintln!("test tenant {} was not dropped: {e}", self.tenant),
                Err(_) => eprintln!("dropping test tenant {} panicked", self.tenant),
            }
        }
    }
}

#[cfg(test)]
pub(crate) async fn serve(app: axum::Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    url
}

#[cfg(test)]
mod tests {
    use vellev_core::DataType;

    use super::seeding::DocumentEmbedder;
    use super::*;

    const REMEDY: &str = "put it right";

    /// The model the tests of a configured one embed with.
    use crate::testing::SMALL;

    fn notes(embedding: Embedding) -> Collection {
        collection(
            CollectionName::new("Notes").unwrap(),
            vec![
                Property {
                    filterable: false,
                    ..Property::new("body", DataType::Text).searchable()
                },
                Property::new("owner", DataType::Text),
                Property::new("weight", DataType::Number),
                Property::new("at", DataType::Date),
            ],
            embedding,
        )
    }

    /// Vectors the client supplies, of `model`.
    fn supplied(model: &str, dimensions: u32) -> Embedding {
        Embedding {
            provider: EmbeddingProvider::None,
            model: model.into(),
            dimensions,
            query_prefix: None,
            document_prefix: None,
            properties: Vec::new(),
        }
    }

    #[test]
    fn a_collection_imported_with_vectors_of_the_right_size_holds_the_clients() {
        let wanted = notes(supplied(LOCAL_MODEL, 768));
        let mut found = notes(supplied("weaviate:none", 768));
        found
            .properties
            .iter_mut()
            .for_each(|p| (p.filterable, p.searchable) = (true, p.data_type.is_text()));
        assert!(mismatches(&found, &wanted, REMEDY).is_empty());
        assert!(checked(&found, &wanted, REMEDY).is_ok());
        assert_eq!(
            mismatches(&notes(supplied("weaviate:none", 384)), &wanted, REMEDY),
            ["vectors have 384 dimensions, expected 768"]
        );
    }

    #[test]
    fn the_clients_vectors_need_a_collection_of_their_model_and_size_that_does_not_embed() {
        let wanted = notes(supplied("titan", 1024));
        assert!(mismatches(&notes(supplied("titan", 1024)), &wanted, REMEDY).is_empty());
        assert_eq!(
            mismatches(&notes(local_embedding(&["body"])), &wanted, REMEDY),
            [
                "the collection embeds for itself, not with the hub's vectors",
                "it embeds with 'BAAI/bge-base-en-v1.5', expected 'titan'",
                "vectors have 768 dimensions, expected 1024",
            ]
        );
    }

    #[test]
    fn a_collection_indexed_less_than_needed_does_not_fit() {
        let mut broken = notes(supplied("bge", 768));
        broken.distance = Distance::Dot;
        broken.properties.retain(|p| p.name != "at");
        broken
            .properties
            .iter_mut()
            .for_each(|p| match p.name.as_str() {
                "owner" => p.filterable = false,
                "body" => p.searchable = false,
                "weight" => p.data_type = DataType::Text,
                _ => {}
            });
        assert_eq!(
            mismatches(&broken, &notes(supplied("bge", 768)), REMEDY),
            [
                "property 'body' is not searchable",
                "property 'owner' is not filterable",
                "property 'weight' is text, expected number",
                "property 'at' is missing",
                "distance is Dot, expected cosine",
            ]
        );
    }

    #[test]
    fn a_collection_the_node_embeds_for_needs_bge_on_the_same_text_with_its_prefix() {
        let wanted = notes(local_embedding(&["body"]));
        assert!(mismatches(&notes(local_embedding(&["body"])), &wanted, REMEDY).is_empty());
        assert_eq!(
            mismatches(&notes(local_embedding(&["owner"])), &wanted, REMEDY),
            ["it embeds [\"owner\"], expected [\"body\"]"]
        );
        let unprefixed = Embedding {
            query_prefix: None,
            ..local_embedding(&["body"])
        };
        assert_eq!(
            mismatches(&notes(unprefixed), &wanted, REMEDY),
            [format!(
                "its query prefix is None, expected Some({LOCAL_QUERY_PREFIX:?})"
            )]
        );
        assert_eq!(
            mismatches(&notes(supplied("other", 768)), &wanted, REMEDY),
            [
                "the collection does not embed for itself",
                "it embeds with 'other', expected 'BAAI/bge-base-en-v1.5'",
                "it embeds [], expected [\"body\"]",
                format!("its query prefix is None, expected Some({LOCAL_QUERY_PREFIX:?})").as_str(),
            ]
        );
    }

    /// What vellev's importer makes of a Weaviate class copied with its
    /// text2vec-transformers vectors is refused, with the remedy its store
    /// names, as a [`Refused`] its store can tell from a node not there.
    #[test]
    fn vectors_copied_from_weaviate_are_refused_with_the_stores_remedy() {
        let found = notes(supplied("weaviate:text2vec-transformers", 384));
        let wanted = notes(local_embedding(&["body"]));
        let refused = mismatches(&found, &wanted, "mirror it instead");
        assert_eq!(
            refused[0],
            "it holds vectors copied from Weaviate ('weaviate:text2vec-transformers') that the node cannot embed queries to match; mirror it instead"
        );
        let refusal = checked(&found, &wanted, REMEDY).unwrap_err();
        let refused = refusal.downcast_ref::<Refused>().expect("a typed refusal");
        assert_eq!(refused.collection, "Notes");
        assert!(refused.problems[0].ends_with(REMEDY), "{refused:?}");
    }

    /// A node that answers every request with `status` and `code`, counting
    /// the requests, except that listing a tenant's collections succeeds
    /// when `tenant_found`.
    struct Fake {
        status: u16,
        code: &'static str,
        message: &'static str,
        tenant_found: bool,
        requests: std::sync::atomic::AtomicUsize,
    }

    impl Fake {
        fn answering(status: u16, code: &'static str, message: &'static str) -> Self {
            Self {
                status,
                code,
                message,
                tenant_found: false,
                requests: Default::default(),
            }
        }

        fn requests(&self) -> usize {
            self.requests.load(std::sync::atomic::Ordering::SeqCst)
        }

        async fn serve(self: &std::sync::Arc<Self>) -> Tenants {
            use axum::response::IntoResponse;
            let fake = std::sync::Arc::clone(self);
            let app = axum::Router::new().fallback(move |uri: axum::http::Uri| {
                let fake = std::sync::Arc::clone(&fake);
                async move {
                    fake.requests
                        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    if fake.tenant_found && uri.path().ends_with("/collections") {
                        return axum::Json(serde_json::json!({ "collections": [] }))
                            .into_response();
                    }
                    (
                        axum::http::StatusCode::from_u16(fake.status).unwrap(),
                        [("retry-after", "0")],
                        axum::Json(serde_json::json!({
                            "error": { "code": fake.code, "message": fake.message }
                        })),
                    )
                        .into_response()
                }
            });
            let url = crate::serve(app).await;
            connect(&url, "key", "harbor").unwrap()
        }
    }

    fn unusable(report: &Report) -> Option<&Unusable> {
        report.downcast_ref::<Unusable>()
    }

    use eyre::Report;

    #[tokio::test]
    async fn a_node_that_cannot_serve_yet_is_asked_again_after_a_short_while() {
        let fake = std::sync::Arc::new(Fake::answering(503, "unavailable", "starting"));
        let ensured = Ensured::new(
            fake.serve().await,
            notes(local_embedding(&["body"])),
            REMEDY,
        );
        let first = ensured.records().await.unwrap_err();
        assert!(unusable(&first).is_none(), "{first}");
        let asked = fake.requests();
        let again = ensured.records().await.unwrap_err();
        assert_eq!(fake.requests(), asked, "told again without asking");
        assert_eq!(again.to_string(), format!("{first:#}"));
        tokio::time::sleep(RETRY_AFTER + Duration::from_millis(100)).await;
        assert!(ensured.records().await.is_err());
        assert!(fake.requests() > asked, "asked again");
    }

    /// Callers that ask at once of a node that hangs wait out one request
    /// between them, not one each, and a caller that gives up leaves the
    /// rest their answer.
    #[tokio::test]
    async fn callers_of_a_node_that_hangs_share_one_request() {
        let asked = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counting = std::sync::Arc::clone(&asked);
        let hang = Duration::from_millis(600);
        let app = axum::Router::new().fallback(move || {
            let counting = std::sync::Arc::clone(&counting);
            async move {
                counting.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                tokio::time::sleep(hang).await;
                (
                    axum::http::StatusCode::SERVICE_UNAVAILABLE,
                    axum::Json(serde_json::json!({
                        "error": { "code": "unavailable", "message": "starting" }
                    })),
                )
            }
        });
        let url = crate::serve(app).await;
        let timeouts = Timeouts {
            read: Duration::from_secs(5),
            write: Duration::from_secs(5),
            ..Timeouts::default()
        };
        let asking = || {
            let tenants = connect_with(&url, "key", "harbor", timeouts).unwrap();
            Ensured::new(tenants, notes(local_embedding(&["body"])), REMEDY)
        };
        let requests = || asked.load(std::sync::atomic::Ordering::SeqCst);

        // What one caller alone costs: the client's tries of one request.
        let began = std::time::Instant::now();
        assert!(asking().records().await.is_err());
        let (alone, alone_took) = (requests(), began.elapsed());

        // One caller gives up at once; the others still get their answer.
        let ensured = std::sync::Arc::new(asking());
        let began = std::time::Instant::now();
        let impatient = tokio::time::timeout(Duration::from_millis(50), ensured.records()).await;
        assert!(impatient.is_err());
        let callers: Vec<_> = (0..8)
            .map(|_| {
                let ensured = std::sync::Arc::clone(&ensured);
                tokio::spawn(async move { ensured.records().await.map(|_| ()) })
            })
            .collect();
        for caller in callers {
            let failed = caller.await.unwrap().unwrap_err();
            assert!(failed.to_string().contains("starting"), "{failed}");
        }
        assert_eq!(
            requests() - alone,
            alone,
            "nine callers asked as much as one does"
        );
        assert!(
            began.elapsed() < alone_took + alone_took / 2,
            "{:?} for nine callers, {alone_took:?} for one",
            began.elapsed()
        );
    }

    /// A check that ended without saying how it went is not waited on for
    /// ever: its caller is told, and the next asks the node afresh.
    #[tokio::test]
    async fn a_check_that_ended_without_saying_is_made_again() {
        let fake = std::sync::Arc::new(Fake::answering(503, "unavailable", "starting"));
        let ensured = Ensured::new(
            fake.serve().await,
            notes(local_embedding(&["body"])),
            REMEDY,
        );
        ensured.orphan_the_check();
        let told = ensured.records().await.unwrap_err().to_string();
        assert!(told.contains("could not be made sure of"), "{told}");
        assert_eq!(fake.requests(), 0);
        let asked = ensured.records().await.unwrap_err().to_string();
        assert!(asked.contains("starting"), "asked afresh: {asked}");
        assert!(fake.requests() > 0);
    }

    /// Two stores that adopt one collection at once may each name it: the
    /// lowest name is its identity, to both.
    #[test]
    fn a_collection_named_twice_has_the_lowest_name_for_its_identity() {
        let named = |names: &[&str]| {
            let mut found = notes(supplied("hub", 2));
            for name in names {
                found.properties.push(Property::new(
                    &format!("incarnation_{name}"),
                    DataType::Bool,
                ));
            }
            identity(&found)
        };
        assert_eq!(named(&[]), None);
        assert_eq!(named(&["b2"]).as_deref(), Some("b2"));
        assert_eq!(named(&["b2", "a9"]).as_deref(), Some("a9"));
        assert_eq!(named(&["a9", "b2"]).as_deref(), Some("a9"));
    }

    #[tokio::test]
    async fn a_refused_key_or_reach_is_final_and_names_the_tenant_until_rechecked() {
        for (status, code, expected) in [
            (
                401,
                "unauthorized",
                "does not accept the key for tenant 'harbor'",
            ),
            (
                403,
                "forbidden",
                "the key for tenant 'harbor' may not do this",
            ),
            (404, "not_found", "no tenant 'harbor'"),
            (
                400,
                "invalid",
                "cannot serve collection 'Notes' of tenant 'harbor'",
            ),
        ] {
            let message = match code {
                "not_found" => "tenant harbor does not exist",
                _ => "refused",
            };
            // An invalid answer is final only about the collection's shape,
            // so the tenant is there and making the collection is refused.
            let fake = std::sync::Arc::new(Fake {
                tenant_found: code == "invalid",
                ..Fake::answering(status, code, message)
            });
            let ensured = Ensured::new(
                fake.serve().await,
                notes(local_embedding(&["body"])),
                REMEDY,
            );
            let refused = ensured.records().await.unwrap_err();
            assert!(unusable(&refused).is_some(), "{code}: {refused}");
            assert!(refused.to_string().contains(expected), "{refused}");
            let asked = fake.requests();
            assert!(ensured.records().await.is_err());
            assert_eq!(fake.requests(), asked, "{code}: not asked again at once");
            assert!(ensured.recheck().await.is_err());
            assert!(
                fake.requests() > asked,
                "{code}: asked again when rechecked"
            );
        }
    }

    #[tokio::test]
    async fn a_node_allowing_fewer_results_is_asked_for_that_many() {
        let fake = std::sync::Arc::new(Fake::answering(
            400,
            "invalid",
            "limit must be from 1 to 3, not 10",
        ));
        let tenant = fake.serve().await.reads;
        let cap = Cap::default();
        let asked = std::sync::Mutex::new(Vec::new());
        let _ = cap
            .ask(10, |limit| {
                asked.lock().unwrap().push(limit);
                tenant.collections()
            })
            .await;
        assert_eq!(*asked.lock().unwrap(), [10, 3]);
        assert_eq!(cap.limit(10), 3);
        assert_eq!(cap.limit(2), 2);
    }

    /// A write the node takes longer than a read may to embed is waited for.
    #[tokio::test]
    async fn a_write_is_given_longer_than_a_read() {
        use std::time::Duration;
        let app = axum::Router::new().fallback(|| async {
            tokio::time::sleep(Duration::from_millis(600)).await;
            axum::Json(serde_json::json!({ "incarnation": 1, "position": 1, "outcomes": [] }))
        });
        let url = crate::serve(app).await;
        let timeouts = Timeouts {
            read: Duration::from_millis(200),
            write: Duration::from_secs(2),
            ..Timeouts::default()
        };
        let tenants = connect_with(&url, "key", "harbor", timeouts).unwrap();
        assert!(
            tenants.reads.apply(Vec::new()).await.is_err(),
            "a read gives up"
        );
        tenants
            .writes
            .apply(Vec::new())
            .await
            .expect("a write waits");
    }

    /// A node that cannot embed with the collection's model is told by one
    /// probe query, saying what to check; one whose collection the client
    /// embeds for is not asked.
    #[tokio::test]
    async fn a_node_that_cannot_embed_is_told_by_a_probe_naming_the_model() {
        let fake = std::sync::Arc::new(Fake::answering(
            503,
            "unavailable",
            "the collection's embedding model could not be used just now",
        ));
        let tenants = fake.serve().await;
        let wanted = notes(local_embedding(&["body"]));
        let records = tenants.reads.collection(&wanted.name);
        let refused = probe_model(&tenants, &records, &wanted).await.unwrap_err();
        let unavailable = refused
            .downcast_ref::<ModelUnavailable>()
            .expect("the model, named");
        assert_eq!(unavailable.model, LOCAL_MODEL);
        assert!(refused
            .to_string()
            .contains("check that the node runs BAAI/bge-base-en-v1.5"));
        let asked = fake.requests();
        probe_model(&tenants, &records, &notes(supplied("hub", 2)))
            .await
            .unwrap();
        assert_eq!(fake.requests(), asked, "nothing to probe");
    }

    /// A request about a collection that happens to have "tenant" in its name
    /// is not taken for a missing tenant: what was asked decides.
    #[tokio::test]
    async fn a_missing_collection_named_for_tenants_is_not_a_missing_tenant() {
        let fake = std::sync::Arc::new(Fake::answering(
            404,
            "not_found",
            "collection TenantNotes does not exist",
        ));
        let tenants = fake.serve().await;
        let name = CollectionName::new("TenantNotes").unwrap();
        let records = tenants.reads.collection(&name);
        let answered = records.count(None).await.unwrap_err();
        let asked = Asked::Request {
            name: &name,
            model: LOCAL_MODEL,
        };
        let failed = failure(answered, &tenants, asked);
        assert!(failed.downcast_ref::<Unusable>().is_none(), "{failed}");
        let answered = tenants.reads.collections().await.unwrap_err();
        let failed = failure(answered, &tenants, Asked::Tenant);
        assert!(
            matches!(
                failed.downcast_ref::<Unusable>(),
                Some(Unusable::NoTenant { .. })
            ),
            "{failed}"
        );
    }

    #[test]
    fn a_request_is_cut_to_what_one_request_gives() {
        let cap = Cap::default();
        assert_eq!(cap.limit(0), 1);
        assert_eq!(cap.limit(10), 10);
        assert_eq!(cap.limit(10_000), MAX_RESULTS as u32);
    }

    #[test]
    fn a_document_prefix_is_compared_where_the_node_embeds() {
        let wanted = notes(local_embedding(&["body"]));
        let prefixed = Embedding {
            document_prefix: Some("passage: ".into()),
            ..local_embedding(&["body"])
        };
        assert_eq!(
            mismatches(&notes(prefixed), &wanted, REMEDY),
            ["its document prefix is Some(\"passage: \"), expected None"]
        );
    }

    /// A node that says it cannot run the model at all is for an operator:
    /// final, naming the collection's own model, where one that cannot just
    /// now is asked again.
    #[tokio::test]
    async fn a_node_that_cannot_run_the_model_names_the_collections_model() {
        let fake = std::sync::Arc::new(Fake::answering(
            501,
            "model_unavailable",
            "the collection's embedding model cannot be used by this node",
        ));
        let tenants = fake.serve().await;
        let wanted = notes(embedding_of(SMALL, 384, None, &["body"]));
        let records = tenants.reads.collection(&wanted.name);
        for refused in [
            probe_model(&tenants, &records, &wanted).await.unwrap_err(),
            failure(
                records.count(None).await.unwrap_err(),
                &tenants,
                Asked::request(&wanted),
            ),
        ] {
            let Some(Unusable::Model {
                model, collection, ..
            }) = refused.downcast_ref()
            else {
                panic!("final, for an operator: {refused:?}");
            };
            assert_eq!((model.as_str(), collection.as_str()), (SMALL, "Notes"));
            let said = refused.to_string();
            assert!(
                said.contains("cannot run 'BAAI/bge-small-en-v1.5'")
                    && said.contains("cannot be used by this node")
                    && said.contains("[vellev] url"),
                "{said}"
            );
        }
        // A refusal with another code is not it.
        let other = std::sync::Arc::new(Fake::answering(409, "conflict", "odd"));
        let tenants = other.serve().await;
        let answered = tenants
            .reads
            .collection(&wanted.name)
            .count(None)
            .await
            .unwrap_err();
        let failed = failure(answered, &tenants, Asked::request(&wanted));
        assert!(failed.downcast_ref::<Unusable>().is_none(), "{failed}");
    }

    /// An embedder that gives every text the same made-up vector.
    pub(super) struct Fixed(pub(super) Vec<f32>);

    #[async_trait::async_trait]
    impl seeding::DocumentEmbedder for Fixed {
        fn name(&self) -> String {
            "a made-up embedder".to_string()
        }

        async fn documents(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
            Ok(texts.iter().map(|_| self.0.clone()).collect())
        }
    }

    fn is_misrouted(report: &Report, at: &Tenants) -> bool {
        unusable(report)
            == Some(&Unusable::Misrouted {
                tenant: "harbor".into(),
                node: at.node.clone(),
            })
    }

    /// A node that does not serve the tenant is told once, by its tenant and
    /// its URL, counted, and not asked again until the recheck; a request
    /// made outside a collection's checks is told and counted too. Run on a
    /// runtime of this thread, as `Ensured` spawns its check, with what it
    /// counts scraped as the hub's `/metrics` renders it.
    #[test]
    fn a_tenant_the_node_does_not_serve_is_told_once_named_and_counted() {
        use crate::testing::logged;
        let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
        let scrape = recorder.handle();
        let scraped = |count: u64| {
            let rendered = scrape.render();
            let line = format!("workforce_vellev_misrouted_total{{tenant=\"harbor\"}} {count}");
            assert!(rendered.lines().any(|at| at == line), "{line}: {rendered}");
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        logged::kept();
        metrics::with_local_recorder(&recorder, || {
            runtime.block_on(async {
                let node = crate::testing::NotServing::up().await;
                let tenants = node.tenants("harbor");
                let wanted = notes(local_embedding(&["body"]));
                let ensured = Ensured::new(tenants.clone(), wanted.clone(), REMEDY);
                let first = ensured.records().await.unwrap_err();
                assert!(is_misrouted(&first, &tenants), "{first:?}");
                assert!(first.to_string().starts_with("misrouted: "), "{first}");
                assert_eq!(node.asked(), 1);
                scraped(1);
                let errors = logged::at("ERROR");
                assert_eq!(errors.len(), 1, "{errors:?}");
                assert!(
                    errors[0].contains("tenant=harbor")
                        && errors[0].contains(&format!("node={}", node.url)),
                    "{errors:?}"
                );

                tokio::time::sleep(RETRY_AFTER + Duration::from_millis(100)).await;
                let again = ensured.records().await.unwrap_err();
                let written = ensured.run(|| tenants.writes.apply(Vec::new())).await;
                for cached in [again, written.unwrap_err()] {
                    assert!(is_misrouted(&cached, &tenants), "{cached:?}");
                }
                assert_eq!(node.asked(), 1, "told again without asking");
                scraped(1);

                let direct = tenants.writes.apply(Vec::new()).await.unwrap_err();
                let direct = failure(direct, &tenants, Asked::request(&wanted));
                assert!(is_misrouted(&direct, &tenants), "{direct:?}");
                assert_eq!(node.asked(), 2);
                scraped(2);
                assert_eq!(logged::at("ERROR").len(), 2);
            })
        });
    }

    /// A collection made sure of whose node then stops serving the tenant
    /// is told so by the request that meets it, and every request after
    /// until the recheck is told without asking.
    #[tokio::test]
    async fn a_misroute_met_by_run_is_kept_until_the_recheck() {
        use axum::response::IntoResponse;
        let wanted = notes(local_embedding(&["body"]));
        let asked = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let misrouting = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (held, counting, flipping) = (
            wanted.clone(),
            std::sync::Arc::clone(&asked),
            std::sync::Arc::clone(&misrouting),
        );
        let app = axum::Router::new().fallback(move |uri: axum::http::Uri| {
            let (held, counting, flipping) = (
                held.clone(),
                std::sync::Arc::clone(&counting),
                std::sync::Arc::clone(&flipping),
            );
            async move {
                use std::sync::atomic::Ordering::SeqCst;
                counting.fetch_add(1, SeqCst);
                let path = uri.path();
                if flipping.load(SeqCst) {
                    return crate::testing::not_served_here();
                }
                if path.ends_with("/collections") {
                    return axum::Json(serde_json::json!({ "collections": [] })).into_response();
                }
                if path.ends_with("/nearest") {
                    flipping.store(true, SeqCst);
                    return axum::Json(serde_json::json!({ "results": [] })).into_response();
                }
                axum::Json(held).into_response()
            }
        });
        let tenants = connect(&crate::serve(app).await, "key", "harbor").unwrap();
        let ensured = Ensured::new(tenants.clone(), wanted, REMEDY);
        let requests = || asked.load(std::sync::atomic::Ordering::SeqCst);
        let first = ensured.run(|| ensured.collection().count(None)).await;
        let first = first.unwrap_err();
        assert!(is_misrouted(&first, &tenants), "{first:?}");
        let made_sure = requests();
        let second = ensured.run(|| ensured.collection().count(None)).await;
        let second = second.unwrap_err();
        assert!(is_misrouted(&second, &tenants), "{second:?}");
        assert_eq!(requests(), made_sure, "told without asking");
    }

    /// Seeding by an embedder of [`LOCAL_DIMENSIONS`]-sized vectors.
    fn seeded() -> seeding::Seeding {
        let embedder = Fixed(vec![0.5; LOCAL_DIMENSIONS as usize]);
        seeding::Seeding::new(std::sync::Arc::new(embedder), 2, 1)
    }

    /// A line of `lines` to write.
    fn a_line(lines: &Collection) -> vellev_client::api::Change {
        let body = serde_json::json!({ "body": "a line" });
        vellev_client::api::Change::upsert(
            lines.name.clone(),
            vellev_core::ObjectId(uuid::Uuid::from_u128(1)),
            vellev_client::api::Content::new(body.as_object().cloned().unwrap()),
        )
    }

    /// The requests made outside a collection's checks, markers and
    /// seeding, are told as misrouted as well; anything else is as it was.
    #[tokio::test]
    async fn a_marker_or_seeding_request_the_node_does_not_serve_is_misrouted() {
        let tenants = crate::testing::NotServing::up().await.tenants("harbor");
        let claimed = index_claim(&tenants, "CodeGraph", "g").await.unwrap_err();
        assert!(is_misrouted(&claimed, &tenants), "{claimed:?}");
        let lines = notes(local_embedding(&["body"]));
        let applied =
            seeding::apply_embedded(&tenants, &lines, &seeded(), vec![a_line(&lines)]).await;
        let applied = applied.unwrap_err();
        assert!(is_misrouted(&applied, &tenants), "{applied:?}");
        let measured = seeding::equivalence(
            &tenants,
            &local_embedding(&["text"]),
            &Fixed(vec![1.0]),
            &[],
        )
        .await
        .unwrap_err();
        assert!(is_misrouted(&measured, &tenants), "{measured:?}");

        let missing = std::sync::Arc::new(Fake::answering(
            404,
            "not_found",
            "tenant harbor does not exist",
        ));
        let tenants = missing.serve().await;
        let claimed = index_claim(&tenants, "CodeGraph", "g").await.unwrap_err();
        assert!(unusable(&claimed).is_none(), "{claimed:?}");
        assert!(
            claimed
                .downcast_ref::<vellev_client::Error>()
                .is_some_and(vellev_client::Error::is_not_found),
            "{claimed:?}"
        );
    }

    /// Vectors made outside the node of another size than the collection's
    /// are refused, naming both, before anything is written.
    #[tokio::test]
    async fn a_seeded_vector_of_another_size_is_refused_and_nothing_written() {
        let fake = std::sync::Arc::new(Fake::answering(500, "internal", "not to be asked"));
        let tenants = fake.serve().await;
        let small = notes(embedding_of(SMALL, 384, None, &["body"]));
        let refused = seeding::apply_embedded(&tenants, &small, &seeded(), vec![a_line(&small)])
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(
            refused,
            "a made-up embedder gave a vector of 768 dimensions, and collection 'Notes' holds 384"
        );
        assert_eq!(fake.requests(), 0, "nothing written");
    }

    /// A made-up vector of `text`: the same for the same text, far from
    /// another's.
    fn made_up(text: &str) -> Vec<f32> {
        use std::hash::{Hash, Hasher};
        let vector: Vec<f32> = (0..16u8)
            .map(|at| {
                let mut hasher = std::collections::hash_map::DefaultHasher::new();
                (text, at).hash(&mut hasher);
                (hasher.finish() % 2001) as f32 / 1000.0 - 1.0
            })
            .collect();
        let length = vector.iter().map(|x| x * x).sum::<f32>().sqrt();
        vector.into_iter().map(|x| x / length).collect()
    }

    /// An embedder of [`made_up`] vectors of each text after `prefix`.
    struct Prefixing(String);

    #[async_trait::async_trait]
    impl seeding::DocumentEmbedder for Prefixing {
        fn name(&self) -> String {
            "a prefixing embedder".to_string()
        }

        async fn documents(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
            Ok(texts
                .iter()
                .map(|text| made_up(&format!("{}{text}", self.0)))
                .collect())
        }
    }

    /// A node that embeds what is written to it as [`made_up`] does, and
    /// keeps every text it was sent.
    async fn embedding_node() -> (Tenants, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
        probe_node(200, 0, true).await
    }

    /// As [`embedding_node`], answering `dropping` to a collection's drop,
    /// leaving the first `losing` objects out of what it lists, and listing
    /// the rest with their vectors only if `vectors`.
    async fn probe_node(
        dropping: u16,
        losing: usize,
        vectors: bool,
    ) -> (Tenants, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
        use axum::response::IntoResponse;
        let held: std::sync::Arc<std::sync::Mutex<Vec<(String, String)>>> = Default::default();
        let sent: std::sync::Arc<std::sync::Mutex<Vec<String>>> = Default::default();
        let (holding, sending) = (std::sync::Arc::clone(&held), std::sync::Arc::clone(&sent));
        let app = axum::Router::new().fallback(
            move |method: axum::http::Method, uri: axum::http::Uri, body: axum::body::Bytes| {
                let (held, sent) = (
                    std::sync::Arc::clone(&holding),
                    std::sync::Arc::clone(&sending),
                );
                async move {
                    let written =
                        serde_json::json!({ "incarnation": 1, "position": 1, "outcomes": [] });
                    if method == axum::http::Method::DELETE && dropping != 200 {
                        let status = axum::http::StatusCode::from_u16(dropping).unwrap();
                        let failed = serde_json::json!({ "error": {
                            "code": "internal",
                            "message": "the drop failed",
                        }});
                        return (status, axum::Json(failed)).into_response();
                    }
                    if uri.path().ends_with("/list") {
                        let objects: Vec<serde_json::Value> = held
                            .lock()
                            .unwrap()
                            .iter()
                            .skip(losing)
                            .map(|(id, text)| {
                                serde_json::json!({
                                    "id": id,
                                    "properties": { "text": text },
                                    "vector": vectors.then(|| made_up(text)),
                                })
                            })
                            .collect();
                        return axum::Json(serde_json::json!({ "objects": objects }))
                            .into_response();
                    }
                    if uri.path().ends_with("/changes") {
                        let batch: serde_json::Value = serde_json::from_slice(&body).unwrap();
                        for change in batch["changes"].as_array().into_iter().flatten() {
                            let text = change["properties"]["text"].as_str().unwrap().to_string();
                            sent.lock().unwrap().push(text.clone());
                            held.lock()
                                .unwrap()
                                .push((change["id"].as_str().unwrap().to_string(), text));
                        }
                    }
                    axum::Json(written).into_response()
                }
            },
        );
        let tenants = connect(&crate::serve(app).await, "key", "harbor").unwrap();
        (tenants, sent)
    }

    /// A probe the node cannot drop fails the embedding that made it, and
    /// the comparison made with it, naming it; so does a vector the node
    /// did not give back, or an object it listed with none; a probe left
    /// behind by a failed embedding is named with its failure.
    #[tokio::test]
    async fn a_probe_that_cannot_be_dropped_is_an_error() {
        let texts = ["parse the config".to_string()];
        let small = embedding_of(SMALL, 16, None, &["text"]);
        let (tenants, _) = probe_node(500, 0, true).await;
        let node = seeding::NodeEmbedder::new(tenants.clone(), small.clone(), 1);
        let refused = node.documents(&texts).await.unwrap_err().to_string();
        assert!(
            refused.starts_with("the collection made to embed with the node, 'WorkforceEmbedProbe")
                && refused.ends_with("', was not dropped"),
            "{refused}"
        );
        let refused = seeding::equivalence(&tenants, &small, &Prefixing(String::new()), &texts)
            .await
            .unwrap_err()
            .to_string();
        assert!(
            refused.contains("'WorkforceEmbedProbe") && refused.contains("was not dropped"),
            "{refused}"
        );

        let (tenants, _) = probe_node(200, 1, true).await;
        let node = seeding::NodeEmbedder::new(tenants, small.clone(), 1);
        let short = node
            .documents(&["a".to_string(), "b".to_string()])
            .await
            .unwrap_err()
            .to_string();
        assert!(
            short.ends_with("gave back vectors for 1 of 2 texts"),
            "{short}"
        );

        let (tenants, _) = probe_node(500, 1, true).await;
        let node = seeding::NodeEmbedder::new(tenants, small.clone(), 1);
        let both = node.documents(&["a".to_string(), "b".to_string()]).await;
        let both = format!("{:#}", both.unwrap_err());
        assert!(
            both.contains("was not dropped")
                && both.ends_with("gave back vectors for 1 of 2 texts"),
            "{both}"
        );

        let (tenants, _) = probe_node(200, 0, false).await;
        let node = seeding::NodeEmbedder::new(tenants, small, 1);
        let bare = node.documents(&texts).await.unwrap_err().to_string();
        assert!(
            bare.contains("of the probe 'WorkforceEmbedProbe")
                && bare.ends_with("' with no vector"),
            "{bare}"
        );
    }

    /// An embedder that puts the collection's own query prefix before
    /// documents is told so by that prefix; with no prefix there is none
    /// to confuse, so no text is sent after one.
    #[tokio::test]
    async fn equivalence_compares_with_the_collections_own_prefix() {
        let texts = ["parse the config".to_string()];
        let custom = embedding_of(SMALL, 16, Some("query: "), &["text"]);
        let (tenants, sent) = embedding_node().await;
        let measured =
            seeding::equivalence(&tenants, &custom, &Prefixing("query: ".into()), &texts)
                .await
                .unwrap();
        let sent = sent.lock().unwrap().clone();
        assert_eq!(sent.len(), 2 * measured.compared);
        assert_eq!(
            sent.iter()
                .filter(|text| text.starts_with("query: "))
                .count(),
            measured.compared,
            "{sent:?}"
        );
        assert_eq!(measured.query_prefix.as_deref(), Some("query: "));
        assert!(
            measured
                .mean_against_prefixed
                .is_some_and(|mean| mean > 0.999),
            "{measured:?}"
        );
        let refused = measured
            .checked("the endpoint", seeding::MIN_COSINE)
            .unwrap_err()
            .to_string();
        assert!(
            refused.contains("it puts the query prefix (\"query: \") before documents"),
            "{refused}"
        );

        let unprefixed = embedding_of(SMALL, 16, None, &["text"]);
        let (tenants, sent) = embedding_node().await;
        let measured =
            seeding::equivalence(&tenants, &unprefixed, &Prefixing(String::new()), &texts)
                .await
                .unwrap();
        assert_eq!(
            sent.lock().unwrap().len(),
            measured.compared,
            "sent once each"
        );
        assert_eq!(
            (measured.query_prefix, measured.mean_against_prefixed),
            (None, None)
        );
        assert!(measured.lowest > 0.9999, "{}", measured.lowest);
    }
}

#[cfg(all(test, feature = "testing"))]
mod node_tests {
    //! Against the node `VELLEV_TEST_URL` and `VELLEV_TEST_KEY` name.
    use vellev_client::api::Nearest;
    use vellev_core::DataType;

    use super::seeding::DocumentEmbedder;
    use super::testing::Node;
    use super::tests::Fixed;
    use super::*;

    fn notes() -> Collection {
        let vectors = Embedding {
            provider: EmbeddingProvider::None,
            model: "notes".into(),
            dimensions: 2,
            query_prefix: None,
            document_prefix: None,
            properties: Vec::new(),
        };
        collection(
            CollectionName::new("Notes").unwrap(),
            vec![Property::new("owner", DataType::Text)],
            vectors,
        )
    }

    #[tokio::test]
    async fn a_missing_tenant_is_named_not_created() {
        let Some(node) = Node::fresh().await else {
            return;
        };
        let missing = format!("{}-missing", node.tenant);
        let tenants = connect(&node.url, &node.key, &missing).unwrap();
        let tenant = tenants.reads.clone();
        let ensured = Ensured::new(tenants, notes(), "put it right");
        let refused = ensured.records().await.unwrap_err();
        assert!(
            matches!(refused.downcast_ref::<Unusable>(), Some(Unusable::NoTenant { tenant }) if *tenant == missing),
            "{refused}"
        );
        assert!(
            tenant.collections().await.unwrap_err().is_not_found(),
            "not created"
        );
    }

    #[tokio::test]
    async fn a_collection_dropped_after_it_was_made_sure_of_is_made_again() {
        let Some(node) = Node::fresh().await else {
            return;
        };
        let ensured = Ensured::new(node.tenants(), notes(), "put it right");
        ensured.records().await.unwrap();
        assert!(ensured.take_created());
        ensured.collection().drop().await.unwrap();
        let counted = ensured.run(|| ensured.collection().count(None)).await;
        assert_eq!(counted.unwrap(), 0);
        assert!(ensured.take_created(), "made again");
    }

    #[tokio::test]
    async fn a_refused_collection_put_right_is_used_once_rechecked() {
        let Some(node) = Node::fresh().await else {
            return;
        };
        let tenant = node.tenant();
        let wrong = collection(
            CollectionName::new("Notes").unwrap(),
            vec![Property::new("owner", DataType::Int)],
            notes().embedding,
        );
        tenant.collection(&wrong.name).create(&wrong).await.unwrap();
        let ensured = Ensured::new(node.tenants(), notes(), "put it right");
        assert!(ensured.records().await.is_err());
        tenant.collection(&wrong.name).drop().await.unwrap();
        assert!(
            ensured.records().await.is_err(),
            "still refused until looked at again"
        );
        ensured.recheck().await.unwrap();
    }

    /// vellev treats a tenant a key does not reach as one that does not
    /// exist, and a key it does not know as no key.
    #[tokio::test]
    async fn a_key_for_another_tenant_or_none_is_final_and_says_so() {
        let Some(node) = Node::fresh().await else {
            return;
        };
        let Some(other) = Node::fresh().await else {
            return;
        };
        let theirs = other.tenant().issue_key().await.unwrap().key;
        let ensured = Ensured::new(
            connect(&node.url, &theirs, &node.tenant).unwrap(),
            notes(),
            "put it right",
        );
        let refused = ensured.records().await.unwrap_err();
        assert!(
            matches!(
                refused.downcast_ref::<Unusable>(),
                Some(Unusable::NoTenant { .. })
            ),
            "{refused}"
        );
        let unknown = Ensured::new(
            connect(&node.url, "no-such-key", &node.tenant).unwrap(),
            notes(),
            "put it right",
        );
        let refused = unknown.records().await.unwrap_err();
        assert!(
            matches!(
                refused.downcast_ref::<Unusable>(),
                Some(Unusable::Key { .. })
            ),
            "{refused}"
        );
    }

    /// One request the node turns down as invalid is the request's own
    /// fault: the collection stays usable for the next.
    #[tokio::test]
    async fn an_invalid_request_does_not_make_the_collection_unusable() {
        let Some(node) = Node::fresh().await else {
            return;
        };
        let ensured = Ensured::new(node.tenants(), notes(), "put it right");
        let oversize = Nearest::to_vector(vec![1.0, 0.0], 100_000);
        let refused = ensured
            .run(|| ensured.collection().nearest(&oversize))
            .await
            .unwrap_err();
        assert!(refused.downcast_ref::<Unusable>().is_none(), "{refused}");
        let counted = ensured.run(|| ensured.collection().count(None)).await;
        assert_eq!(counted.unwrap(), 0, "the next request goes through");
    }

    /// The switch marker collection is the importer's, whichever comes
    /// first: an import after a mark, or a mark after an import.
    #[tokio::test]
    async fn the_switch_markers_are_kept_as_the_importer_makes_them() {
        let Some(node) = Node::fresh().await else {
            return;
        };
        let tenants = node.tenants();
        let tenant = &tenants.reads;
        mark(&tenants, "Memories").await.unwrap();
        // What vellev's importer makes of Weaviate's WorkforceMemorySwitch
        // class, spelled out here rather than taken from the code under test.
        let imported = collection(
            CollectionName::new(MARKERS).unwrap(),
            vec![
                Property::new("collection", DataType::Text).searchable(),
                Property::new("switchedAt", DataType::Date),
            ],
            Embedding {
                provider: EmbeddingProvider::None,
                model: "weaviate:none".into(),
                dimensions: 1,
                query_prefix: None,
                document_prefix: None,
                properties: Vec::new(),
            },
        );
        tenant
            .collection(&imported.name)
            .create(&imported)
            .await
            .expect("an import after a mark finds the collection as it makes it");
        assert!(marked(&tenants, "Memories").await.unwrap());

        let Some(other) = Node::fresh().await else {
            return;
        };
        let tenants = other.tenants();
        let tenant = &tenants.reads;
        tenant
            .collection(&imported.name)
            .create(&imported)
            .await
            .unwrap();
        mark(&tenants, "Memories")
            .await
            .expect("a mark after an import");
        assert!(marked(&tenants, "Memories").await.unwrap());
    }

    /// A test node's tenant goes when the node does.
    #[tokio::test]
    async fn a_test_tenant_is_dropped_with_its_node() {
        let Some(node) = Node::fresh().await else {
            return;
        };
        let tenant = node.tenant();
        assert!(tenant.collections().await.is_ok());
        drop(node);
        assert!(tenant.collections().await.unwrap_err().is_not_found());
    }

    /// What only one owner may keep in a tenant is the first claimant's,
    /// to every claimant after.
    #[tokio::test]
    async fn a_claim_is_the_first_claimants() {
        let Some(node) = Node::fresh().await else {
            return;
        };
        let tenants = node.tenants();
        let claims = (0..4).map(|n| {
            let tenants = tenants.clone();
            async move {
                index_claim(&tenants, "CodeGraph", &format!("graph-{n}"))
                    .await
                    .unwrap()
            }
        });
        let owners = futures::future::join_all(claims).await;
        assert!(owners.iter().all(|owner| *owner == owners[0]), "{owners:?}");
        assert_eq!(
            index_claim(&tenants, "CodeGraph", "late").await.unwrap(),
            owners[0]
        );
        assert_eq!(
            index_claim(&tenants, "Other", "late").await.unwrap(),
            "late"
        );
    }

    /// Whose a tenant's code vectors are is read without making the
    /// markers' collection, and after a claim is the claimant's.
    #[tokio::test]
    async fn index_claimed_reads_without_making() {
        let Some(node) = Node::fresh().await else {
            return;
        };
        let tenants = node.tenants();
        assert_eq!(index_claimed(&tenants, "CodeGraph").await.unwrap(), None);
        let indexes = tenants
            .reads
            .collection(&CollectionName::new(INDEX_MARKERS).unwrap());
        assert_eq!(indexes.schema().await.unwrap(), None, "nothing made");
        index_claim(&tenants, "CodeGraph", "g").await.unwrap();
        assert_eq!(
            index_claimed(&tenants, "CodeGraph")
                .await
                .unwrap()
                .as_deref(),
            Some("g")
        );
        assert_eq!(index_claimed(&tenants, "Other").await.unwrap(), None);
    }

    /// The node embeds more texts than one page of its listing holds, and
    /// gives each back in the order of the texts.
    #[tokio::test]
    async fn node_embedder_pages_its_listing() {
        let Some(node) = Node::embedding().await else {
            return;
        };
        let tenants = node.tenants();
        let small = embedding_of(testing::SMALL, 384, None, &["text"]);
        let texts: Vec<String> = (0..1500)
            .map(|n| format!("item {n} of a long list"))
            .collect();
        let made = seeding::NodeEmbedder::new(tenants.clone(), small.clone(), 500)
            .documents(&texts)
            .await
            .unwrap();
        assert_eq!(made.len(), texts.len());
        let alone = seeding::NodeEmbedder::new(tenants.clone(), small, 1)
            .documents(&[texts[1200].clone()])
            .await
            .unwrap();
        let dot = |a: &[f32], b: &[f32]| a.iter().zip(b).map(|(a, b)| a * b).sum::<f32>();
        assert!(dot(&alone[0], &made[1200]) > 0.9999);
        assert!(dot(&alone[0], &made[1199]) < 0.9999);
        let held = tenants.reads.collections().await.unwrap();
        assert!(
            held.iter()
                .all(|c| !c.name.as_str().starts_with("WorkforceEmbedProbe")),
            "every probe dropped"
        );
    }

    /// Index markers are kept in a collection of their own, so that the
    /// switch markers' holds nothing but what Weaviate's did: marking an
    /// index does not make it, and neither kind is taken for the other.
    #[tokio::test]
    async fn index_markers_are_kept_apart_from_the_switch_markers() {
        let Some(node) = Node::fresh().await else {
            return;
        };
        let tenants = node.tenants();
        let tenant = &tenants.reads;
        assert!(!index_marked(&tenants, "Artifacts@a").await.unwrap());
        index_mark(&tenants, "Artifacts@a").await.unwrap();
        index_mark(&tenants, "Artifacts@a").await.unwrap();
        assert!(index_marked(&tenants, "Artifacts@a").await.unwrap());
        assert!(!index_marked(&tenants, "Artifacts@b").await.unwrap());
        let switches = tenant.collection(&CollectionName::new(MARKERS).unwrap());
        assert_eq!(
            switches.schema().await.unwrap(),
            None,
            "an index marked leaves the switch markers' collection unmade"
        );
        let indexes = tenant.collection(&CollectionName::new(INDEX_MARKERS).unwrap());
        assert_eq!(indexes.count(None).await.unwrap(), 1);

        mark(&tenants, "Artifacts@a").await.unwrap();
        index_unmark(&tenants, "Artifacts@a").await.unwrap();
        assert!(!index_marked(&tenants, "Artifacts@a").await.unwrap());
        assert!(
            marked(&tenants, "Artifacts@a").await.unwrap(),
            "the switch's own"
        );
        index_unmark(&tenants, "Artifacts@a").await.unwrap();
    }

    /// A collection made identified is told from the next making of it,
    /// by every store that looks, and what it is asked for is unchanged.
    #[tokio::test]
    async fn a_collection_made_again_is_another_and_one_kept_is_the_same() {
        let Some(node) = Node::fresh().await else {
            return;
        };
        let at_once = Duration::ZERO;
        let look = || {
            Ensured::new(node.tenants(), notes(), "put it right")
                .identified()
                .rechecking(at_once)
        };
        let first = look();
        let records = first.records().await.unwrap();
        let made_with = identity(&records.schema().await.unwrap().unwrap());
        let made = first.identity().await.unwrap();
        assert_eq!(
            made_with,
            Some(made.clone()),
            "made with it, not given it after"
        );
        assert!(first.take_created());
        assert_eq!(first.identity().await.unwrap(), made);
        assert_eq!(
            look().identity().await.unwrap(),
            made,
            "the same to another store"
        );
        let plain = Ensured::new(node.tenants(), notes(), "put it right");
        plain
            .records()
            .await
            .expect("of the shape wanted, with its identity");

        first.collection().drop().await.unwrap();
        let again = first.identity().await.unwrap();
        assert_ne!(again, made, "made again, it is another");
        assert!(first.take_created());
        assert_eq!(look().identity().await.unwrap(), again);
    }

    /// Stores that make sure of one new collection at once all end with
    /// the collection one of them made, and tell it by the same identity.
    #[tokio::test]
    async fn stores_that_make_one_collection_at_once_agree_on_it() {
        let Some(node) = Node::fresh().await else {
            return;
        };
        let stores: Vec<_> = (0..6)
            .map(|_| {
                std::sync::Arc::new(
                    Ensured::new(node.tenants(), notes(), "put it right")
                        .identified()
                        .rechecking(Duration::ZERO),
                )
            })
            .collect();
        let made: Vec<_> = stores
            .iter()
            .map(|store| {
                let store = std::sync::Arc::clone(store);
                tokio::spawn(async move { store.records().await.map(|_| ()) })
            })
            .collect();
        for made in made {
            made.await.unwrap().expect("made, or found made");
        }
        let created = stores.iter().filter(|store| store.take_created()).count();
        assert_eq!(created, 1, "made once");
        let first = stores[0].identity().await.unwrap();
        for store in &stores {
            assert_eq!(store.identity().await.unwrap(), first);
        }
    }

    /// The identity is read from the node again only after the recheck
    /// time, or once a request finds the collection gone.
    #[tokio::test]
    async fn an_identity_is_kept_until_the_recheck_time_or_the_collection_is_found_gone() {
        let Some(node) = Node::fresh().await else {
            return;
        };
        let kept = Ensured::new(node.tenants(), notes(), "put it right").identified();
        let made = kept.identity().await.unwrap();
        kept.collection().drop().await.unwrap();
        assert_eq!(kept.identity().await.unwrap(), made, "not read again yet");
        kept.run(|| kept.collection().count(None)).await.unwrap();
        assert_ne!(
            kept.identity().await.unwrap(),
            made,
            "found gone by a request"
        );
    }

    /// A collection made before collections were identified is adopted as
    /// it is, with what it holds.
    #[tokio::test]
    async fn a_collection_made_without_an_identity_is_adopted_with_what_it_holds() {
        let Some(node) = Node::fresh().await else {
            return;
        };
        let wanted = notes();
        let plain = Ensured::new(node.tenants(), wanted.clone(), "put it right");
        let records = plain.records().await.unwrap();
        assert_eq!(identity(&records.schema().await.unwrap().unwrap()), None);
        let content = vellev_client::api::Content::new(
            serde_json::json!({ "owner": "kept" })
                .as_object()
                .cloned()
                .unwrap(),
        )
        .vector(vec![1.0, 0.0]);
        let id = vellev_core::ObjectId(uuid::Uuid::new_v4());
        records.put(id, &content).await.unwrap();

        let adopting = Ensured::new(node.tenants(), wanted.clone(), "put it right").identified();
        let adopted = adopting.identity().await.unwrap();
        assert!(!adopting.take_created(), "adopted, not made");
        assert_eq!(records.count(None).await.unwrap(), 1);
        let other = Ensured::new(node.tenants(), wanted, "put it right").identified();
        assert_eq!(other.identity().await.unwrap(), adopted);
    }

    fn lines() -> Collection {
        collection(
            CollectionName::new("Lines").unwrap(),
            vec![
                Property::new("owner", DataType::Text),
                Property {
                    filterable: false,
                    ..Property::new("text", DataType::Text)
                },
            ],
            local_embedding(&["text"]),
        )
    }

    fn line(id: u128, text: &str) -> vellev_client::api::Change {
        vellev_client::api::Change::upsert(
            lines().name,
            vellev_core::ObjectId(uuid::Uuid::from_u128(id)),
            vellev_client::api::Content::new(
                serde_json::json!({ "owner": "o", "text": text })
                    .as_object()
                    .cloned()
                    .unwrap(),
            ),
        )
    }

    /// An embedder that takes a while, and counts how many requests it has
    /// at once.
    #[derive(Default)]
    struct Busy {
        now: std::sync::atomic::AtomicUsize,
        most: std::sync::atomic::AtomicUsize,
    }

    #[async_trait::async_trait]
    impl seeding::DocumentEmbedder for Busy {
        fn name(&self) -> String {
            "a busy embedder".to_string()
        }

        async fn documents(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
            use std::sync::atomic::Ordering;
            let now = self.now.fetch_add(1, Ordering::SeqCst) + 1;
            self.most.fetch_max(now, Ordering::SeqCst);
            tokio::time::sleep(std::time::Duration::from_millis(40)).await;
            self.now.fetch_sub(1, Ordering::SeqCst);
            let mut vector = vec![0.0f32; LOCAL_DIMENSIONS as usize];
            vector[3] = 1.0;
            Ok(texts.iter().map(|_| vector.clone()).collect())
        }
    }

    /// However many pages are being written at once with one seeding, for
    /// however many collections or repos, the embedder has as many
    /// requests at once as the seeding says, and no more: enough of them
    /// to be kept busy, never more than it was started to take.
    #[tokio::test]
    async fn an_embedder_has_as_many_requests_at_once_as_it_is_given_and_no_more() {
        let Some(node) = Node::embedding().await else {
            return;
        };
        let tenants = connect(&node.url, &node.key, &node.tenant).unwrap();
        let wanted = lines();
        tenants
            .reads
            .collection(&wanted.name)
            .create(&wanted)
            .await
            .unwrap();
        let busy = std::sync::Arc::new(Busy::default());
        let seeding = seeding::Seeding::new(busy.clone(), 1, 3);
        assert_eq!((seeding.page(), seeding.pages_at_a_time()), (3, 4));
        let page = |from: u128| -> Vec<vellev_client::api::Change> {
            (from..from + 12).map(|n| line(n, "a line")).collect()
        };
        let pages = (0..3).map(|n| {
            let (seeding, wanted, tenants) = (seeding.clone(), wanted.clone(), &tenants);
            let page = page(100 * (n + 1));
            async move { seeding::apply_embedded(tenants, &wanted, &seeding, page).await }
        });
        let stored = futures::future::try_join_all(pages).await.unwrap();
        assert_eq!(stored, [12, 12, 12]);
        assert_eq!(busy.most.load(std::sync::atomic::Ordering::SeqCst), 3);
    }

    /// The node stores a vector sent with an object of a collection it
    /// embeds for, as it is, and embeds only what comes without one: what
    /// seeding from outside the node rests on.
    #[tokio::test]
    async fn the_node_stores_a_vector_sent_with_an_object_it_would_have_embedded() {
        let Some(node) = Node::embedding().await else {
            return;
        };
        let tenants = connect(&node.url, &node.key, &node.tenant).unwrap();
        let wanted = lines();
        tenants
            .reads
            .collection(&wanted.name)
            .create(&wanted)
            .await
            .unwrap();
        let mut made_up = vec![0.0f32; LOCAL_DIMENSIONS as usize];
        made_up[7] = 1.0;
        let seeding = seeding::Seeding::new(std::sync::Arc::new(Fixed(made_up.clone())), 2, 2);
        let changes = vec![
            line(1, "one"),
            line(2, "two"),
            line(3, "   "),
            line(4, "four"),
            line(5, "five"),
        ];
        let applied = seeding::apply_embedded(&tenants, &wanted, &seeding, changes)
            .await
            .unwrap();
        assert_eq!(applied, 5);
        tenants.writes.apply(vec![line(6, "six")]).await.unwrap();

        let page = vellev_client::api::Page::new(100).vectors(true);
        let listed = tenants
            .reads
            .collection(&wanted.name)
            .list(&page)
            .await
            .unwrap();
        let vector = |id: u128| {
            let id = vellev_core::ObjectId(uuid::Uuid::from_u128(id));
            let object = listed
                .objects
                .iter()
                .find(|object| object.id == id)
                .unwrap();
            object.vector.clone()
        };
        for sent in [1, 2, 4, 5] {
            assert_eq!(vector(sent), Some(made_up.clone()), "stored as sent");
        }
        assert_eq!(vector(3), None, "nothing to embed, nothing sent");
        let embedded = vector(6).expect("embedded by the node");
        assert_ne!(embedded, made_up);
        assert_eq!(embedded.len(), LOCAL_DIMENSIONS as usize);

        // A vector of another size is refused before anything is written.
        let short = seeding::Seeding::new(std::sync::Arc::new(Fixed(vec![1.0, 0.0])), 2, 2);
        let refused = seeding::apply_embedded(&tenants, &wanted, &short, vec![line(9, "nine")])
            .await
            .unwrap_err()
            .to_string();
        assert!(
            refused.contains("2 dimensions") && refused.contains("768"),
            "{refused}"
        );
        let count = tenants
            .reads
            .collection(&wanted.name)
            .count(None)
            .await
            .unwrap();
        assert_eq!(count, 6);
    }

    /// An outside embedder is trusted only if it embeds as the node does:
    /// the same model alike passes; one that puts the query prefix before
    /// documents, or serves something else, is refused, saying which.
    #[tokio::test]
    async fn an_embedder_is_measured_against_the_node_before_it_is_trusted() {
        let Some(node) = Node::embedding().await else {
            return;
        };
        let tenants = connect(&node.url, &node.key, &node.tenant).unwrap();
        let embedding = local_embedding(&["anything"]);
        let texts: Vec<String> = [
            "pub fn parse_config(path: &str) -> Config",
            "Reads the TOML configuration file and applies overrides.",
            "impl Scheduler { fn tick(&mut self) }",
            "",
        ]
        .map(String::from)
        .to_vec();

        let alike = testing::ByNode::of(&node);
        let measured = seeding::equivalence(&tenants, &embedding, &alike, &texts)
            .await
            .unwrap();
        assert_eq!(
            measured.compared,
            3 + 16,
            "the blank one is not compared, and the standing sample always is"
        );
        assert!(measured.lowest > 0.9999, "{measured:?}");
        assert!((measured.mean_length - 1.0).abs() < 0.001, "{measured:?}");
        assert!(
            measured
                .mean_against_prefixed
                .is_some_and(|mean| mean < 0.99),
            "{measured:?}"
        );
        measured.checked("it", seeding::MIN_COSINE).unwrap();

        let prefixing = testing::ByNode {
            prefix: LOCAL_QUERY_PREFIX.to_string(),
            ..testing::ByNode::of(&node)
        };
        let measured = seeding::equivalence(&tenants, &embedding, &prefixing, &texts)
            .await
            .unwrap();
        assert!(measured.lowest < seeding::MIN_COSINE, "{measured:?}");
        let refused = measured
            .checked("the endpoint", seeding::MIN_COSINE)
            .unwrap_err()
            .to_string();
        assert!(
            refused.contains("the endpoint does not embed as the node does")
                && refused.contains("puts the query prefix")
                && refused.contains("Nothing was written"),
            "{refused}"
        );

        let mut other = vec![0.0f32; LOCAL_DIMENSIONS as usize];
        other[3] = 1.0;
        let measured = seeding::equivalence(&tenants, &embedding, &Fixed(other), &texts)
            .await
            .unwrap();
        let refused = measured
            .checked("it", seeding::MIN_COSINE)
            .unwrap_err()
            .to_string();
        assert!(refused.contains("serves another model"), "{refused}");

        // Longer vectors of the same direction are as near.
        let longer = testing::ByNode {
            scaled: 3.0,
            ..testing::ByNode::of(&node)
        };
        let measured = seeding::equivalence(&tenants, &embedding, &longer, &texts)
            .await
            .unwrap();
        assert!(
            measured.lowest > 0.9999 && measured.mean_length > 2.9,
            "{measured:?}"
        );

        // With nothing of its own to compare, the standing sample is
        // compared; an embedder compared over too few is not trusted,
        // however near.
        let standing = seeding::equivalence(&tenants, &embedding, &alike, &[])
            .await
            .unwrap();
        assert_eq!(standing.compared, seeding::MIN_SAMPLE);
        standing.checked("it", seeding::MIN_COSINE).unwrap();
        for compared in [0, seeding::MIN_SAMPLE - 1] {
            let few = seeding::Equivalence {
                compared,
                ..standing.clone()
            };
            let refused = few
                .checked("it", seeding::MIN_COSINE)
                .unwrap_err()
                .to_string();
            assert!(refused.contains("at least 16 are needed"), "{refused}");
        }

        // A comparison dropped part of the way, its collection made,
        // leaves no collection either.
        let probes = || async {
            let held = tenants.reads.collections().await.unwrap();
            let probes = held.iter();
            probes
                .filter(|c| c.name.as_str().starts_with("WorkforceEmbedProbe"))
                .count()
        };
        assert_eq!(probes().await, 0, "the ones that ended are dropped");
        {
            let slow = seeding::equivalence(&tenants, &embedding, &alike, &texts);
            tokio::pin!(slow);
            let made = async {
                while probes().await == 0 {
                    tokio::time::sleep(Duration::from_millis(2)).await;
                }
            };
            let made = tokio::time::timeout(Duration::from_secs(60), made);
            tokio::select! {
                ended = &mut slow => panic!("ended before it could be dropped: {ended:?}"),
                made = made => made.expect("a collection is made to compare"),
            }
        }
        let began = std::time::Instant::now();
        while probes().await > 0 {
            assert!(
                began.elapsed() < Duration::from_secs(20),
                "the collection made to compare is dropped"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    #[test]
    fn the_embedded_text_is_the_listed_properties_a_line_each() {
        let properties = |json: serde_json::Value| json.as_object().cloned().unwrap();
        let mut embedding = local_embedding(&["title", "tags", "body"]);
        let whole = properties(serde_json::json!({
            "body": "the body",
            "tags": ["one", "two"],
            "weight": 0.5,
            "owner": "not embedded",
            "title": "A title",
        }));
        assert_eq!(
            seeding::embedded_text(&embedding, &whole),
            "A title\none\ntwo\nthe body"
        );
        assert_eq!(
            seeding::embedded_text(&embedding, &properties(serde_json::json!({}))),
            ""
        );
        embedding.document_prefix = Some("passage: ".into());
        assert_eq!(
            seeding::embedded_text(&embedding, &properties(serde_json::json!({ "body": "b" }))),
            "passage: b"
        );
        assert_eq!(
            seeding::embedded_text(&embedding, &properties(serde_json::json!({ "body": " " }))),
            " ",
            "nothing to embed is not prefixed"
        );
    }
}
