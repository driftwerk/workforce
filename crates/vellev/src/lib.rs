//! What every workforce store on vellev shares: connecting to a tenant,
//! the node's own embedding model, the limit on one request, making sure a
//! collection exists in a shape the store can use, and markers.
//!
//! Artifacts and code are always embedded by the node's own bge
//! ([`LOCAL_MODEL`]), so the node must run it; memories are too, unless
//! `[embeddings]` has the hub embed them.

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
    doctor, ensure, failure, identity, probe_model, Asked, Ensured, ModelUnavailable, Unusable,
    RECHECK_AFTER, RETRY_AFTER,
};
pub use markers::{
    index_claim, index_mark, index_marked, index_unmark, mark, mark_promoted, marked, marker_id,
    promoted, unmark, INDEX_MARKERS, MARKERS,
};
pub use matching::{unmatched, Unmatched};
pub use shape::{checked, mismatches, Refused};

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
}

/// `tenant` on the node at `url`, reached with `api_key`, with the default
/// [`Timeouts`]. Every request is bounded by its timeout and tried again
/// where that is safe.
pub fn connect(url: &str, api_key: &str, tenant: &str) -> Result<Tenants> {
    connect_with(url, api_key, tenant, Timeouts::default())
}

/// As [`connect`], with `timeouts`.
pub fn connect_with(url: &str, api_key: &str, tenant: &str, timeouts: Timeouts) -> Result<Tenants> {
    let options = Options {
        timeout: timeouts.read,
        attempts: timeouts.attempts.max(1),
        ..Options::default()
    };
    let client = Client::connect(&[url], api_key, options)?;
    let tenant = TenantId::new(tenant)?;
    Ok(Tenants {
        reads: client.tenant(&tenant),
        writes: client.within(timeouts.write).tenant(&tenant),
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
/// and `VELLEV_TEST_LOCAL_EMBED=1` says it runs the local model.
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
    /// model would: with `prefix` before every text, or its vectors
    /// `scaled`, when told to be one that embeds otherwise.
    pub struct ByNode {
        pub tenants: Tenants,
        pub prefix: String,
        pub scaled: f32,
        /// How many texts it was asked for.
        pub asked: std::sync::atomic::AtomicUsize,
    }

    impl ByNode {
        pub fn of(node: &Node) -> Self {
            Self {
                tenants: connect(&node.url, &node.key, &node.tenant).expect("a usable test node"),
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
            use vellev_client::api::{Change, Page};
            self.asked
                .fetch_add(texts.len(), std::sync::atomic::Ordering::SeqCst);
            let scratch = collection(
                CollectionName::new(format!("ByNode{}", uuid::Uuid::new_v4().simple()))?,
                vec![Property::new("text", vellev_core::DataType::Text)],
                local_embedding(&["text"]),
            );
            let records = self.tenants.reads.collection(&scratch.name);
            records.create(&scratch).await?;
            let ids: Vec<vellev_core::ObjectId> = texts
                .iter()
                .map(|_| vellev_core::ObjectId(uuid::Uuid::new_v4()))
                .collect();
            let changes = ids
                .iter()
                .zip(texts)
                .map(|(id, text)| Change::Upsert {
                    collection: scratch.name.clone(),
                    id: *id,
                    properties: serde_json::json!({ "text": format!("{}{text}", self.prefix) })
                        .as_object()
                        .cloned()
                        .unwrap_or_default(),
                    vector: None,
                })
                .collect();
            self.tenants.writes.apply(changes).await?;
            let page = Page {
                filter: None,
                after: None,
                limit: 1000,
                vectors: true,
            };
            let listed = records.list(&page).await?;
            records.drop().await?;
            let made: std::collections::HashMap<_, _> = listed
                .objects
                .into_iter()
                .map(|object| (object.id, object.vector.unwrap_or_default()))
                .collect();
            Ok(ids
                .iter()
                .map(|id| {
                    let vector = made.get(id).cloned().unwrap_or_default();
                    vector.into_iter().map(|x| x * self.scaled).collect()
                })
                .collect())
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

        pub fn tenant(&self) -> Tenant {
            connect(&self.url, &self.key, &self.tenant)
                .expect("a usable test node")
                .reads
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
mod tests {
    use vellev_core::DataType;

    use super::*;

    const REMEDY: &str = "put it right";

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

        async fn serve(self: &std::sync::Arc<Self>) -> Tenant {
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
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            connect(&url, "key", "harbor").unwrap().reads
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
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let timeouts = Timeouts {
            read: Duration::from_secs(5),
            write: Duration::from_secs(5),
            ..Timeouts::default()
        };
        let asking = || {
            let tenant = connect_with(&url, "key", "harbor", timeouts).unwrap().reads;
            Ensured::new(tenant, notes(local_embedding(&["body"])), REMEDY)
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
        let tenant = fake.serve().await;
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
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
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
        let tenant = fake.serve().await;
        let wanted = notes(local_embedding(&["body"]));
        let records = tenant.collection(&wanted.name);
        let refused = probe_model(&tenant, &records, &wanted).await.unwrap_err();
        let unavailable = refused
            .downcast_ref::<ModelUnavailable>()
            .expect("the model, named");
        assert_eq!(unavailable.model, LOCAL_MODEL);
        assert!(refused
            .to_string()
            .contains("check that the node runs BAAI/bge-base-en-v1.5"));
        let asked = fake.requests();
        probe_model(&tenant, &records, &notes(supplied("hub", 2)))
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
        let tenant = fake.serve().await;
        let name = CollectionName::new("TenantNotes").unwrap();
        let answered = tenant.collection(&name).count(None).await.unwrap_err();
        let failed = failure(answered, &tenant, Asked::Request(&name));
        assert!(failed.downcast_ref::<Unusable>().is_none(), "{failed}");
        let answered = tenant.collections().await.unwrap_err();
        let failed = failure(answered, &tenant, Asked::Tenant);
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
    /// final, naming the model, where one that cannot just now is asked
    /// again.
    #[tokio::test]
    async fn a_node_that_cannot_run_the_model_at_all_is_unusable_naming_the_model() {
        let fake = std::sync::Arc::new(Fake::answering(
            501,
            "model_unavailable",
            "the collection's embedding model cannot be used by this node",
        ));
        let tenant = fake.serve().await;
        let wanted = notes(local_embedding(&["body"]));
        let records = tenant.collection(&wanted.name);
        for refused in [
            probe_model(&tenant, &records, &wanted).await.unwrap_err(),
            failure(
                records.count(None).await.unwrap_err(),
                &tenant,
                Asked::Request(&wanted.name),
            ),
        ] {
            let Some(Unusable::Model {
                model, collection, ..
            }) = refused.downcast_ref()
            else {
                panic!("final, for an operator: {refused:?}");
            };
            assert_eq!(
                (model.as_str(), collection.as_str()),
                (LOCAL_MODEL, "Notes")
            );
            let said = refused.to_string();
            assert!(
                said.contains("cannot run 'BAAI/bge-base-en-v1.5'")
                    && said.contains("cannot be used by this node")
                    && said.contains("[vellev] url"),
                "{said}"
            );
        }
        // Another refusal with a code this client does not know is not it.
        let other = std::sync::Arc::new(Fake::answering(500, "model_unavailable", "odd"));
        let tenant = other.serve().await;
        let answered = tenant
            .collection(&wanted.name)
            .count(None)
            .await
            .unwrap_err();
        let failed = failure(answered, &tenant, Asked::Request(&wanted.name));
        assert!(failed.downcast_ref::<Unusable>().is_none(), "{failed}");
    }
}

#[cfg(all(test, feature = "testing"))]
mod node_tests {
    //! Against the node `VELLEV_TEST_URL` and `VELLEV_TEST_KEY` name.
    use vellev_client::api::Nearest;
    use vellev_core::DataType;

    use super::testing::Node;
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
        let tenant = connect(&node.url, &node.key, &missing).unwrap().reads;
        let ensured = Ensured::new(tenant.clone(), notes(), "put it right");
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
        let ensured = Ensured::new(node.tenant(), notes(), "put it right");
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
        let ensured = Ensured::new(tenant.clone(), notes(), "put it right");
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
            connect(&node.url, &theirs, &node.tenant).unwrap().reads,
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
            connect(&node.url, "no-such-key", &node.tenant)
                .unwrap()
                .reads,
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
        let ensured = Ensured::new(node.tenant(), notes(), "put it right");
        let oversize = Nearest {
            vector: Some(vec![1.0, 0.0]),
            text: None,
            limit: 100_000,
            filter: None,
            oversample: None,
            vectors: false,
        };
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
        let tenant = node.tenant();
        mark(&tenant, "Memories").await.unwrap();
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
        assert!(marked(&tenant, "Memories").await.unwrap());

        let Some(other) = Node::fresh().await else {
            return;
        };
        let tenant = other.tenant();
        tenant
            .collection(&imported.name)
            .create(&imported)
            .await
            .unwrap();
        mark(&tenant, "Memories")
            .await
            .expect("a mark after an import");
        assert!(marked(&tenant, "Memories").await.unwrap());
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
        let tenant = node.tenant();
        let claims = (0..4).map(|n| {
            let tenant = tenant.clone();
            async move {
                index_claim(&tenant, "CodeGraph", &format!("graph-{n}"))
                    .await
                    .unwrap()
            }
        });
        let owners = futures::future::join_all(claims).await;
        assert!(owners.iter().all(|owner| *owner == owners[0]), "{owners:?}");
        assert_eq!(
            index_claim(&tenant, "CodeGraph", "late").await.unwrap(),
            owners[0]
        );
        assert_eq!(index_claim(&tenant, "Other", "late").await.unwrap(), "late");
    }

    /// Index markers are kept in a collection of their own, so that the
    /// switch markers' holds nothing but what Weaviate's did: marking an
    /// index does not make it, and neither kind is taken for the other.
    #[tokio::test]
    async fn index_markers_are_kept_apart_from_the_switch_markers() {
        let Some(node) = Node::fresh().await else {
            return;
        };
        let tenant = node.tenant();
        assert!(!index_marked(&tenant, "Artifacts@a").await.unwrap());
        index_mark(&tenant, "Artifacts@a").await.unwrap();
        index_mark(&tenant, "Artifacts@a").await.unwrap();
        assert!(index_marked(&tenant, "Artifacts@a").await.unwrap());
        assert!(!index_marked(&tenant, "Artifacts@b").await.unwrap());
        let switches = tenant.collection(&CollectionName::new(MARKERS).unwrap());
        assert_eq!(
            switches.schema().await.unwrap(),
            None,
            "an index marked leaves the switch markers' collection unmade"
        );
        let indexes = tenant.collection(&CollectionName::new(INDEX_MARKERS).unwrap());
        assert_eq!(indexes.count(None).await.unwrap(), 1);

        mark(&tenant, "Artifacts@a").await.unwrap();
        index_unmark(&tenant, "Artifacts@a").await.unwrap();
        assert!(!index_marked(&tenant, "Artifacts@a").await.unwrap());
        assert!(
            marked(&tenant, "Artifacts@a").await.unwrap(),
            "the switch's own"
        );
        index_unmark(&tenant, "Artifacts@a").await.unwrap();
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
            Ensured::new(node.tenant(), notes(), "put it right")
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
        let plain = Ensured::new(node.tenant(), notes(), "put it right");
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
                    Ensured::new(node.tenant(), notes(), "put it right")
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
        let kept = Ensured::new(node.tenant(), notes(), "put it right").identified();
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
        let plain = Ensured::new(node.tenant(), wanted.clone(), "put it right");
        let records = plain.records().await.unwrap();
        assert_eq!(identity(&records.schema().await.unwrap().unwrap()), None);
        let content = vellev_client::api::Content {
            properties: serde_json::json!({ "owner": "kept" })
                .as_object()
                .cloned()
                .unwrap(),
            vector: Some(vec![1.0, 0.0]),
        };
        let id = vellev_core::ObjectId(uuid::Uuid::new_v4());
        records.put(id, &content).await.unwrap();

        let adopting = Ensured::new(node.tenant(), wanted.clone(), "put it right").identified();
        let adopted = adopting.identity().await.unwrap();
        assert!(!adopting.take_created(), "adopted, not made");
        assert_eq!(records.count(None).await.unwrap(), 1);
        let other = Ensured::new(node.tenant(), wanted, "put it right").identified();
        assert_eq!(other.identity().await.unwrap(), adopted);
    }

    /// An embedder that gives every text the same made-up vector.
    struct Fixed(Vec<f32>);

    #[async_trait::async_trait]
    impl seeding::DocumentEmbedder for Fixed {
        fn name(&self) -> String {
            "a made-up embedder".to_string()
        }

        async fn documents(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
            Ok(texts.iter().map(|_| self.0.clone()).collect())
        }
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
        vellev_client::api::Change::Upsert {
            collection: lines().name,
            id: vellev_core::ObjectId(uuid::Uuid::from_u128(id)),
            properties: serde_json::json!({ "owner": "o", "text": text })
                .as_object()
                .cloned()
                .unwrap(),
            vector: None,
        }
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
            let (seeding, wanted, writes) = (seeding.clone(), wanted.clone(), &tenants.writes);
            let page = page(100 * (n + 1));
            async move { seeding::apply_embedded(writes, &wanted, &seeding, page).await }
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
        let applied = seeding::apply_embedded(&tenants.writes, &wanted, &seeding, changes)
            .await
            .unwrap();
        assert_eq!(applied, 5);
        tenants.writes.apply(vec![line(6, "six")]).await.unwrap();

        let page = vellev_client::api::Page {
            filter: None,
            after: None,
            limit: 100,
            vectors: true,
        };
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
        let refused =
            seeding::apply_embedded(&tenants.writes, &wanted, &short, vec![line(9, "nine")])
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
        assert!(measured.mean_against_prefixed < 0.99, "{measured:?}");
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
