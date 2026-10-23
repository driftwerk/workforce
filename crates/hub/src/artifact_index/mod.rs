//! Indexing pipeline + async auto-tagger for Artifacts v2 (DW-551).
//!
//! Implements [`ArtifactIndexer`] for the orchestrator's `KnowledgeStore`. On
//! create/update the store, on a detached task, asks [`ArtifactIndexer::tags`]
//! for the artifact's tags: an LLM's 3–7 topical ones (best-effort) merged
//! with the author's. The store writes the union back to `artifacts.tags`,
//! then has `title + tags + content` embedded into the `Artifacts`
//! collection of the vector database ([`ArtifactVectors`]: Weaviate or
//! vellev, each embedding for itself, no embedding key). `artifact.search`
//! searches by meaning; the store re-loads and ACL-filters the returned ids.
//!
//! Tagging is best-effort, and so is the index for whoever writes an
//! artifact: create/update always succeeds (DW-551 AC). A write the vector
//! database did not take stays noted in Postgres, and the store puts the
//! index right later.
//!
//! The tagger LLM is supplied out-of-band via [`set_tagger_llm`] (called from
//! `serve::build_inner` where a provider is in scope), so this module needs
//! only `config` to construct.

mod vellev;
mod weaviate;

use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use tracing::warn;
use workforce_agent::llm::provider::{LlmProvider, ProviderKind};
use workforce_agent::llm::types::{ContentBlock, LlmMessage};
use workforce_agent::runner::with_served_log;
use workforce_common::cost::{CostEntry, CostTracker, TokenCounts};
use workforce_common::subscription::Origin;
use workforce_orchestrator::artifact_store::{normalize_tags, Artifact, ArtifactIndexer};

pub use vellev::{Pace as VellevPace, VellevArtifacts, TRY_DEADLINE as VELLEV_TRY_DEADLINE};
pub use weaviate::WeaviateArtifacts;

const COLLECTION: &str = "Artifacts";

/// Process-global tagger LLM. Set once from `build_inner`; read lazily by the
/// indexer when it needs to auto-tag. Absent → tagging is skipped (the
/// embed still runs).
static TAGGER_LLM: OnceLock<Arc<dyn LlmProvider>> = OnceLock::new();

/// Where the tagger's calls are costed. Set once, by the first agent built.
static TAGGER_COSTS: OnceLock<Arc<CostTracker>> = OnceLock::new();

/// The pseudo-agent the tagger's cost rows are filed under.
const TAGGER_AGENT: &str = "artifacts";

/// Install the LLM used by the artifact auto-tagger (idempotent — first wins).
pub fn set_tagger_llm(llm: Arc<dyn LlmProvider>) {
    let _ = TAGGER_LLM.set(llm);
}

/// Offer an agent's own provider — its primary alone, never the chain over
/// it — to the tagger; returns whether it was taken. A subscription agent's
/// never is: the tagger is process-wide, so that agent's login would pay
/// for every agent's artifacts.
pub fn offer_agent_tagger(primary: ProviderKind, llm: Arc<dyn LlmProvider>) -> bool {
    !primary.is_subscription() && TAGGER_LLM.set(llm).is_ok()
}

/// Cost the tagger's calls to `costs` (idempotent — first wins).
pub fn cost_tagging_to(costs: Arc<CostTracker>) {
    let _ = TAGGER_COSTS.set(costs);
}

/// Where artifact vectors are kept: one object per artifact, holding its
/// id and the text the database embeds for itself.
#[async_trait]
pub trait ArtifactVectors: Send + Sync {
    /// Which kind of index this is, in a word (see
    /// [`ArtifactIndexer::backend`]).
    fn backend(&self) -> &'static str {
        "index"
    }

    /// Store `text` as the artifact's, replacing what it had, and keep
    /// `text_hash` with it if the store has somewhere to.
    async fn upsert(&self, artifact_id: &str, text: &str, text_hash: &str) -> eyre::Result<()>;
    /// Forget the artifact.
    async fn remove(&self, artifact_id: &str) -> eyre::Result<()>;
    /// The artifacts nearest `query` in meaning, as `(id, certainty)`.
    async fn search(&self, query: &str, limit: usize) -> eyre::Result<Vec<(String, f32)>>;
    /// Whether the store holds every artifact (see
    /// [`ArtifactIndexer::complete`]).
    async fn complete(&self) -> eyre::Result<bool>;
    /// Record that it may not (see [`ArtifactIndexer::mark_incomplete`]).
    async fn mark_incomplete(&self) -> eyre::Result<()>;
    /// Record that it does (see [`ArtifactIndexer::mark_complete`]).
    async fn mark_complete(&self) -> eyre::Result<()>;
    /// Of `page`, the ids the store lacks or holds another text of (see
    /// [`ArtifactIndexer::differing`]).
    async fn differing(&self, page: &[(String, String)]) -> eyre::Result<Vec<String>>;
    /// Whether the store keeps each artifact's text hash.
    fn compares_text(&self) -> bool {
        true
    }
    /// See [`ArtifactIndexer::search_counted`].
    async fn search_counted(
        &self,
        query: &str,
        limit: usize,
    ) -> eyre::Result<(Vec<(String, f32)>, usize)> {
        let found = self.search(query, limit).await?;
        Ok((found, self.count().await?))
    }
    /// See [`ArtifactIndexer::refused_index`].
    fn refused_index(&self) -> Option<String> {
        None
    }
    /// How many artifacts the store holds.
    async fn count(&self) -> eyre::Result<usize>;
    /// A page of the ids the store holds (see
    /// [`ArtifactIndexer::ids_after`]).
    async fn ids_after(
        &self,
        after: Option<String>,
        limit: usize,
    ) -> eyre::Result<(Vec<String>, Option<String>)>;
}

/// The auto-tagger in front of an [`ArtifactVectors`] store.
pub struct ArtifactIndex<V> {
    vectors: V,
}

impl<V: ArtifactVectors> ArtifactIndex<V> {
    pub fn new(vectors: V) -> Self {
        Self { vectors }
    }

    /// Ask the LLM for tags. Returns `[]` on any failure / no LLM.
    async fn auto_tag(&self, a: &Artifact) -> Vec<String> {
        let Some(llm) = TAGGER_LLM.get() else {
            return Vec::new();
        };
        let system = "You label knowledge-base artifacts. Reply with ONLY 3-7 short \
                      topical tags as a comma-separated list, all lowercase, single or \
                      two words each, no sentences, no markdown, no trailing period.";
        let body: String = a.content.chars().take(4000).collect();
        let user = format!("Type: {}\nTitle: {}\n\n{}", a.artifact_type, a.title, body);
        let (tagged, served) = with_served_log(llm.create_message(
            Some(system),
            &[LlmMessage::user(user)],
            &[],
            None,
            Some(64),
            false,
        ))
        .await;
        match tagged {
            Ok(resp) => {
                if let Some(costs) = TAGGER_COSTS.get() {
                    costs.record(CostEntry {
                        task_type: Some("artifact_tagging"),
                        identity_id: Some(&a.created_by),
                        description: Some("artifact:tags"),
                        served,
                        ..CostEntry::new(
                            TAGGER_AGENT,
                            &resp.model,
                            TokenCounts::from(&resp.usage),
                            Origin::Knowledge,
                        )
                    });
                }
                let text = extract_text(&resp.content);
                let tags: Vec<String> = text
                    .split([',', '\n'])
                    .map(|t| t.trim().trim_matches(['#', '-', '*', '.', '"']).to_string())
                    .filter(|t| !t.is_empty())
                    .collect();
                normalize_tags(tags).into_iter().take(7).collect()
            }
            Err(e) => {
                warn!(error = ?e, id = %a.id, "artifact auto-tag failed");
                Vec::new()
            }
        }
    }
}

#[async_trait]
impl<V: ArtifactVectors> ArtifactIndexer for ArtifactIndex<V> {
    fn backend(&self) -> &'static str {
        self.vectors.backend()
    }

    async fn tags(&self, artifact: &Artifact) -> Vec<String> {
        let creator = uuid::Uuid::parse_str(&artifact.owner_id).ok();
        let auto = crate::user_config::billed_to(creator, self.auto_tag(artifact)).await;
        normalize_tags([artifact.tags.clone(), auto].concat())
    }

    async fn remove(&self, id: String) -> eyre::Result<()> {
        self.vectors.remove(&id).await
    }

    async fn search(&self, query: &str, limit: usize) -> eyre::Result<Vec<(String, f32)>> {
        self.vectors.search(query, limit).await
    }

    async fn search_counted(
        &self,
        query: &str,
        limit: usize,
    ) -> eyre::Result<(Vec<(String, f32)>, usize)> {
        self.vectors.search_counted(query, limit).await
    }

    fn refused_index(&self) -> Option<String> {
        self.vectors.refused_index()
    }

    async fn reindex(&self, artifact: &Artifact, text_hash: &str) -> eyre::Result<()> {
        self.vectors
            .upsert(
                &artifact.id,
                &indexed_text(artifact, &artifact.tags),
                text_hash,
            )
            .await
    }

    async fn complete(&self) -> eyre::Result<bool> {
        self.vectors.complete().await
    }

    async fn mark_incomplete(&self) -> eyre::Result<()> {
        self.vectors.mark_incomplete().await
    }

    async fn mark_complete(&self) -> eyre::Result<()> {
        self.vectors.mark_complete().await
    }

    async fn differing(&self, page: &[(String, String)]) -> eyre::Result<Vec<String>> {
        self.vectors.differing(page).await
    }

    fn compares_text(&self) -> bool {
        self.vectors.compares_text()
    }

    async fn count(&self) -> eyre::Result<usize> {
        self.vectors.count().await
    }

    async fn ids_after(
        &self,
        after: Option<String>,
        limit: usize,
    ) -> eyre::Result<(Vec<String>, Option<String>)> {
        self.vectors.ids_after(after, limit).await
    }
}

/// What is embedded for an artifact: its title, its tags and its content.
fn indexed_text(artifact: &Artifact, tags: &[String]) -> String {
    text_embedded(&artifact.title, tags, &artifact.content)
}

/// What is embedded for an artifact of this title, tags and content.
pub(crate) fn text_embedded(title: &str, tags: &[String], content: &str) -> String {
    format!("{title}\n\ntags: {}\n\n{content}", tags.join(", "))
}

/// Deterministic object id for an artifact id, so re-indexing replaces
/// rather than duplicates.
fn object_id(artifact_id: &str) -> uuid::Uuid {
    uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_URL, artifact_id.as_bytes())
}

/// Collect the text from an LLM response's content blocks.
fn extract_text(content: &[ContentBlock]) -> String {
    content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    /// Artifact vectors kept in memory, failing every upsert when told to.
    #[derive(Default)]
    struct Held {
        upserted: Mutex<Vec<(String, String, String)>>,
        removed: Mutex<Vec<String>>,
        failing: bool,
        marked: std::sync::atomic::AtomicBool,
    }

    #[async_trait]
    impl ArtifactVectors for Arc<Held> {
        async fn upsert(&self, artifact_id: &str, text: &str, text_hash: &str) -> eyre::Result<()> {
            if self.failing {
                return Err(eyre::eyre!("the vector database is down"));
            }
            self.upserted.lock().unwrap().push((
                artifact_id.to_string(),
                text.to_string(),
                text_hash.to_string(),
            ));
            Ok(())
        }

        async fn remove(&self, artifact_id: &str) -> eyre::Result<()> {
            self.removed.lock().unwrap().push(artifact_id.to_string());
            Ok(())
        }

        async fn search(&self, query: &str, limit: usize) -> eyre::Result<Vec<(String, f32)>> {
            Ok(vec![(format!("{query}:{limit}"), 0.5)])
        }

        async fn complete(&self) -> eyre::Result<bool> {
            Ok(self.marked.load(std::sync::atomic::Ordering::SeqCst))
        }

        async fn mark_incomplete(&self) -> eyre::Result<()> {
            self.marked
                .store(false, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }

        async fn mark_complete(&self) -> eyre::Result<()> {
            self.marked.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }

        async fn differing(&self, page: &[(String, String)]) -> eyre::Result<Vec<String>> {
            let upserted = self.upserted.lock().unwrap();
            let held = |id: &str, hash: &str| upserted.iter().any(|(i, _, h)| i == id && h == hash);
            Ok(page
                .iter()
                .filter(|(id, hash)| !held(id, hash))
                .map(|(id, _)| id.clone())
                .collect())
        }

        async fn count(&self) -> eyre::Result<usize> {
            Ok(self.upserted.lock().unwrap().len())
        }

        async fn ids_after(
            &self,
            _after: Option<String>,
            _limit: usize,
        ) -> eyre::Result<(Vec<String>, Option<String>)> {
            let upserted = self.upserted.lock().unwrap();
            Ok((upserted.iter().map(|(id, _, _)| id.clone()).collect(), None))
        }
    }

    fn index(held: &Arc<Held>) -> ArtifactIndex<Arc<Held>> {
        ArtifactIndex::new(Arc::clone(held))
    }

    fn runbook() -> Artifact {
        Artifact::new(
            "spec-1",
            workforce_orchestrator::artifact_store::ArtifactType::Spec,
            "Runbook",
            "Roll back with care.",
            "owner",
            "owner",
            workforce_orchestrator::artifact_store::Visibility::Private,
            vec!["Ops".into()],
        )
    }

    /// An artifact's tags are its own, normalised, with any the
    /// process-wide tagger adds, if another test installed one.
    #[tokio::test]
    async fn an_artifacts_tags_are_its_own_and_the_taggers() {
        let held = Arc::new(Held::default());
        let tags = index(&held).tags(&runbook()).await;
        assert_eq!(tags[0], "ops", "{tags:?}");
        assert!(held.upserted.lock().unwrap().is_empty(), "nothing indexed");
    }

    /// An artifact is embedded as stored, its tags as they are, with its
    /// text hash; it is removed and searched for through the store, and
    /// completeness is the store's.
    #[tokio::test]
    async fn an_artifact_is_indexed_as_stored_and_removed_and_searched_through_the_store() {
        let held = Arc::new(Held::default());
        let index = index(&held);
        index.reindex(&runbook(), "h1").await.unwrap();
        assert_eq!(
            *held.upserted.lock().unwrap(),
            [(
                "spec-1".to_string(),
                "Runbook\n\ntags: ops\n\nRoll back with care.".to_string(),
                "h1".to_string()
            )]
        );
        let page = [
            ("spec-1".to_string(), "h1".to_string()),
            ("spec-2".to_string(), "h2".to_string()),
        ];
        assert_eq!(index.differing(&page).await.unwrap(), ["spec-2"]);
        assert_eq!(index.ids_after(None, 10).await.unwrap().0, ["spec-1"]);
        assert!(index.compares_text());
        assert_eq!(index.count().await.unwrap(), 1);
        assert!(!index.complete().await.unwrap());
        index.mark_complete().await.unwrap();
        assert!(index.complete().await.unwrap());
        index.mark_incomplete().await.unwrap();
        assert!(!index.complete().await.unwrap());

        index.remove("spec-1".into()).await.unwrap();
        assert_eq!(*held.removed.lock().unwrap(), ["spec-1"]);
        assert_eq!(
            index.search("roll back", 3).await.unwrap(),
            [("roll back:3".to_string(), 0.5)]
        );
    }

    /// The store is told of an upsert the vector database did not take,
    /// to index the artifact later.
    #[tokio::test]
    async fn a_failed_upsert_is_told() {
        let held = Arc::new(Held {
            failing: true,
            ..Held::default()
        });
        let failed = index(&held).reindex(&runbook(), "h1").await.unwrap_err();
        assert!(failed.to_string().contains("is down"), "{failed}");
        assert!(held.upserted.lock().unwrap().is_empty());
    }

    #[test]
    fn tagger_not_installed_from_subscription_agent() {
        let llm: Arc<dyn LlmProvider> = Arc::new(workforce_agent::llm::ollama::OllamaClient::new(
            "http://127.0.0.1:9",
        ));
        for kind in [
            ProviderKind::ChatGpt,
            ProviderKind::ClaudeCode,
            ProviderKind::Codex,
        ] {
            assert!(!offer_agent_tagger(kind, Arc::clone(&llm)), "{kind:?}");
        }
        assert!(
            TAGGER_LLM
                .get()
                .is_none_or(|tagger| !Arc::ptr_eq(tagger, &llm)),
            "a subscription agent's provider never becomes the process-wide tagger"
        );
    }

    #[test]
    fn object_id_is_deterministic() {
        assert_eq!(object_id("spec-1"), object_id("spec-1"));
        assert_ne!(object_id("spec-1"), object_id("spec-2"));
        assert_eq!(
            object_id("spec-1"),
            uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_URL, b"spec-1"),
            "the id Weaviate's index has always used"
        );
    }

    #[test]
    fn the_indexed_text_is_title_tags_and_content() {
        let artifact = Artifact::new(
            "spec-1",
            workforce_orchestrator::artifact_store::ArtifactType::Spec,
            "Runbook",
            "Roll back with care.",
            "owner",
            "owner",
            workforce_orchestrator::artifact_store::Visibility::Private,
            Vec::new(),
        );
        assert_eq!(
            indexed_text(&artifact, &["ops".into(), "deploy".into()]),
            "Runbook\n\ntags: ops, deploy\n\nRoll back with care."
        );
    }

    #[test]
    fn extract_text_joins_text_blocks() {
        let blocks = vec![
            ContentBlock::Text {
                text: "rust, db".into(),
            },
            ContentBlock::Text { text: "api".into() },
        ];
        assert_eq!(extract_text(&blocks), "rust, db api");
    }

    /// Artifacts in Postgres indexed in a vellev node, end to end. Needs
    /// a node that runs the local model.
    mod kept_in_vellev {
        use std::time::Duration;

        use workforce_orchestrator::artifact_store::{
            ArtifactBackend, ArtifactFilter, ArtifactType, Caller, IndexPace, KnowledgeStore,
            Visibility,
        };
        use workforce_vellev::testing::Node;

        use super::super::vellev::Pace;
        use super::*;

        const QUICK: IndexPace = IndexPace {
            // What the node did not take is tried again the next round.
            backoff: Duration::ZERO,
            retry: Duration::from_millis(50),
            watch: Duration::from_secs(3600),
            reconcile: Duration::from_secs(3600),
            ..IndexPace::DEFAULT
        };

        /// Artifact vectors that refuse to store the artifacts named.
        struct Refusing {
            vectors: VellevArtifacts,
            refused: &'static [&'static str],
        }

        #[async_trait]
        impl ArtifactVectors for Refusing {
            async fn upsert(&self, artifact_id: &str, text: &str, hash: &str) -> eyre::Result<()> {
                if self.refused.contains(&artifact_id) {
                    return Err(eyre::eyre!("the node went away"));
                }
                self.vectors.upsert(artifact_id, text, hash).await
            }
            async fn remove(&self, artifact_id: &str) -> eyre::Result<()> {
                self.vectors.remove(artifact_id).await
            }
            async fn search(&self, query: &str, limit: usize) -> eyre::Result<Vec<(String, f32)>> {
                self.vectors.search(query, limit).await
            }
            async fn complete(&self) -> eyre::Result<bool> {
                self.vectors.complete().await
            }
            async fn mark_incomplete(&self) -> eyre::Result<()> {
                self.vectors.mark_incomplete().await
            }
            async fn mark_complete(&self) -> eyre::Result<()> {
                self.vectors.mark_complete().await
            }
            async fn differing(&self, page: &[(String, String)]) -> eyre::Result<Vec<String>> {
                self.vectors.differing(page).await
            }
            async fn count(&self) -> eyre::Result<usize> {
                self.vectors.count().await
            }
            async fn ids_after(
                &self,
                after: Option<String>,
                limit: usize,
            ) -> eyre::Result<(Vec<String>, Option<String>)> {
                self.vectors.ids_after(after, limit).await
            }
        }

        fn vectors(node: &Node) -> VellevArtifacts {
            VellevArtifacts::new(&node.url, &node.key, &node.tenant).unwrap()
        }

        async fn store_over<V: ArtifactVectors + 'static>(
            pool: &workforce_common::postgres::PostgresPool,
            vectors: V,
        ) -> Arc<KnowledgeStore> {
            let index = ArtifactIndex::new(vectors);
            KnowledgeStore::new(pool.clone(), Some(Arc::new(index)))
                .await
                .unwrap()
        }

        const NOTES: [(&str, &str, &str); 3] = [
            (
                "deploy",
                "Deploy runbook",
                "How to roll back a release that went wrong",
            ),
            ("lunch", "Lunch menu", "Soup and bread on Fridays"),
            ("oncall", "On-call rota", "Who carries the pager each week"),
        ];

        async fn write_notes(store: &KnowledgeStore) {
            for (id, title, content) in NOTES {
                let note = Artifact::new(
                    id,
                    ArtifactType::Spec,
                    title,
                    content,
                    "owner",
                    "owner",
                    Visibility::Public,
                    Vec::new(),
                );
                store.create(&note).await.unwrap();
            }
        }

        /// What a search for "rollback" finds, a word no note has, so only
        /// by meaning: ids and scores.
        async fn found(store: &KnowledgeStore) -> Vec<(String, f32)> {
            searched(store, "rollback").await
        }

        async fn searched(store: &KnowledgeStore, query: &str) -> Vec<(String, f32)> {
            let found = store
                .search(
                    query,
                    &Caller::user("reader"),
                    &ArtifactFilter::default(),
                    5,
                )
                .await
                .unwrap();
            found
                .into_iter()
                .map(|found| (found.artifact.id, found.score))
                .collect()
        }

        async fn until(what: &str, met: impl AsyncFn() -> bool) {
            let begun = std::time::Instant::now();
            while !met().await {
                assert!(
                    begun.elapsed() < Duration::from_secs(60),
                    "not in time: {what}"
                );
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        }

        async fn drop_collection(node: &Node) {
            let name = vellev_core::CollectionName::new(COLLECTION).unwrap();
            node.tenant().collection(&name).drop().await.unwrap();
        }

        /// The index is filled in the background and then relied on. Its
        /// collection dropped behind the hub's back, as one a restored
        /// backup left out is, is found out by the next search, which goes
        /// by keyword; the index is unmarked, and filled again by itself.
        #[tokio::test(flavor = "multi_thread")]
        async fn a_collection_lost_behind_the_hub_is_found_out_and_filled_again() {
            let Some(node) = Node::embedding().await else {
                return;
            };
            workforce_postgres::test_support::with_test_pool(
                KnowledgeStore::build_pool,
                |pool| async move {
                    let index = vectors(&node);
                    let store = store_over(&pool, vectors(&node)).await;
                    write_notes(&store).await;
                    assert_eq!(found(&store).await.len(), 0, "no keyword says rollback");
                    store.keep_index_complete(QUICK);
                    until("filled", async || {
                        store.index_state().complete == Some(true)
                    })
                    .await;
                    assert_eq!(index.count().await.unwrap(), 3);
                    assert_eq!(found(&store).await[0].0, "deploy", "by meaning");

                    drop_collection(&node).await;
                    let by_keyword: Vec<String> = searched(&store, "menu")
                        .await
                        .into_iter()
                        .map(|(id, _)| id)
                        .collect();
                    assert_eq!(
                        by_keyword,
                        ["lunch"],
                        "by keyword, not by an index made again, empty"
                    );
                    assert_ne!(store.index_state().complete, Some(true));
                    until("filled again", async || {
                        store.index_state().complete == Some(true)
                            && index.count().await.unwrap() == 3
                    })
                    .await;
                    assert_eq!(found(&store).await[0].0, "deploy");
                    let state = store.index_state();
                    assert_eq!(state.last_backfill.map(|last| last.indexed), Some(3));
                },
            )
            .await;
        }

        /// The collection dropped while the hub is down; a reindex that
        /// fails part of the way; the hub boots. The index is not trusted,
        /// and the hub fills it.
        #[tokio::test(flavor = "multi_thread")]
        async fn an_index_lost_while_the_hub_was_down_is_not_trusted_after_a_reindex_fails() {
            let Some(node) = Node::embedding().await else {
                return;
            };
            workforce_postgres::test_support::with_test_pool(
                KnowledgeStore::build_pool,
                |pool| async move {
                    // The hub, before: a complete index.
                    let before = store_over(&pool, vectors(&node)).await;
                    write_notes(&before).await;
                    // Each write is indexed in the background as well: by
                    // which of the two each note is indexed is not said,
                    // only that all are, and that nothing is still on its
                    // way to the node when the collection is dropped.
                    let done = before.backfill(true, |_, _| {}).await.unwrap();
                    assert_eq!(done.failed, 0);
                    until("every write settled", async || {
                        let conn = pool.get().await.unwrap();
                        let pending = "SELECT count(*) FROM artifact_index_pending";
                        let left: i64 = conn.query_one(pending, &[]).await.unwrap().get(0);
                        left == 0
                    })
                    .await;
                    assert!(vectors(&node).complete().await.unwrap());
                    assert_eq!(vectors(&node).count().await.unwrap(), 3);
                    drop(before);

                    // Down. The collection is dropped, and a reindex fails
                    // part of the way.
                    drop_collection(&node).await;
                    let refusing = Refusing {
                        vectors: vectors(&node),
                        refused: &["lunch", "oncall"],
                    };
                    let reindex = store_over(&pool, refusing).await;
                    let stopped = reindex.backfill(true, |_, _| {}).await.unwrap();
                    assert_eq!((stopped.indexed, stopped.failed), (1, 2));
                    let named: Vec<&str> = stopped.failures.iter().map(|f| f.id.as_str()).collect();
                    assert_eq!(named, ["lunch", "oncall"]);
                    assert!(stopped.failures[0].error.contains("went away"));
                    drop(reindex);

                    // The hub boots. The collection made again was compared
                    // with Postgres and is marked so: it holds one of the
                    // three, and the other two are noted as still to index.
                    let booted = vectors(&node);
                    assert!(booted.complete().await.unwrap(), "compared");
                    assert_eq!(booted.count().await.unwrap(), 1);
                    let conn = pool.get().await.unwrap();
                    let pending = "SELECT count(*) FROM artifact_index_pending";
                    let left: i64 = conn.query_one(pending, &[]).await.unwrap().get(0);
                    assert_eq!(left, 2, "what it lacks is noted");
                    drop(conn);
                    let store = store_over(&pool, booted).await;
                    store.keep_index_complete(QUICK);
                    until("filled", async || {
                        vectors(&node).count().await.unwrap() == 3
                    })
                    .await;
                    assert_eq!(found(&store).await[0].0, "deploy");
                },
            )
            .await;
        }

        /// The index restored to an earlier state under the hub, its
        /// marker and identity with it: an earlier text of one artifact,
        /// another missing, one since deleted. Each vector carries the hash
        /// of its text as Postgres computes it, so the keeper finds all
        /// three within its reconcile time.
        #[tokio::test(flavor = "multi_thread")]
        async fn an_index_restored_to_an_earlier_state_is_put_right() {
            let Some(node) = Node::embedding().await else {
                return;
            };
            workforce_postgres::test_support::with_test_pool(
                KnowledgeStore::build_pool,
                |pool| async move {
                    let index = vectors(&node);
                    let store = store_over(&pool, vectors(&node)).await;
                    write_notes(&store).await;
                    let pace = IndexPace {
                        reconcile: Duration::from_millis(500),
                        ..QUICK
                    };
                    store.keep_index_complete(pace);
                    let agrees = async || {
                        let conn = pool.get().await.unwrap();
                        let rows = conn
                            .query(
                                "SELECT id, md5(title || E'\\x01' || tags || E'\\x01' || content) \
                                 FROM artifacts ORDER BY id",
                                &[],
                            )
                            .await
                            .unwrap();
                        let stored: Vec<(String, String)> =
                            rows.iter().map(|row| (row.get(0), row.get(1))).collect();
                        index.differing(&stored).await.unwrap().is_empty()
                            && index.count().await.unwrap() == stored.len()
                    };
                    until("filled, each with its hash", async || {
                        store.index_state().complete == Some(true) && agrees().await
                    })
                    .await;

                    index
                        .upsert(
                            "deploy",
                            "Deploy runbook\n\ntags: \n\nAs it was last week",
                            "stale",
                        )
                        .await
                        .unwrap();
                    index.remove("lunch").await.unwrap();
                    index.upsert("gone", "Deleted since", "old").await.unwrap();
                    until("put right", async || {
                        agrees().await && store.index_state().complete == Some(true)
                    })
                    .await;
                    assert_eq!(found(&store).await[0].0, "deploy");
                },
            )
            .await;
        }

        /// Fails on one artifact as the node's own trouble would, and
        /// otherwise is the node.
        struct FailingOn {
            vectors: VellevArtifacts,
            id: &'static str,
        }
        #[async_trait]
        impl ArtifactVectors for FailingOn {
            async fn upsert(&self, id: &str, text: &str, hash: &str) -> eyre::Result<()> {
                if id == self.id {
                    eyre::bail!("vellev answered 500: internal error embedding this object");
                }
                self.vectors.upsert(id, text, hash).await
            }
            async fn remove(&self, id: &str) -> eyre::Result<()> {
                self.vectors.remove(id).await
            }
            async fn search(&self, q: &str, limit: usize) -> eyre::Result<Vec<(String, f32)>> {
                self.vectors.search(q, limit).await
            }
            async fn complete(&self) -> eyre::Result<bool> {
                self.vectors.complete().await
            }
            async fn mark_incomplete(&self) -> eyre::Result<()> {
                self.vectors.mark_incomplete().await
            }
            async fn mark_complete(&self) -> eyre::Result<()> {
                self.vectors.mark_complete().await
            }
            async fn differing(&self, page: &[(String, String)]) -> eyre::Result<Vec<String>> {
                self.vectors.differing(page).await
            }
            async fn count(&self) -> eyre::Result<usize> {
                self.vectors.count().await
            }
            async fn ids_after(
                &self,
                after: Option<String>,
                limit: usize,
            ) -> eyre::Result<(Vec<String>, Option<String>)> {
                self.vectors.ids_after(after, limit).await
            }
        }

        /// A real node that fails on one artifact every time, as its
        /// embedder might on one text, while it takes every other: the
        /// others are indexed and found by meaning, the index is marked
        /// complete, and the one is named as failing with what the node
        /// said, and goes on being tried, for ever. It does not stop the
        /// rest.
        #[tokio::test(flavor = "multi_thread")]
        async fn an_artifact_the_node_fails_on_goes_on_being_tried_and_the_rest_are_indexed() {
            let Some(node) = Node::embedding().await else {
                return;
            };
            workforce_postgres::test_support::with_test_pool(
                KnowledgeStore::build_pool,
                |pool| async move {
                    let failing = FailingOn {
                        vectors: vectors(&node),
                        id: "lunch",
                    };
                    let store = store_over(&pool, failing).await;
                    write_notes(&store).await;
                    store.keep_index_complete(QUICK);
                    until("the rest indexed, and the index complete", async || {
                        store.index_state().complete == Some(true)
                            && vectors(&node).count().await.unwrap() == 2
                    })
                    .await;
                    assert_eq!(found(&store).await[0].0, "deploy", "found by meaning");
                    // The one is named, with what the node said, and goes on
                    // being tried: nothing is ever set aside.
                    until("the one tried again and again", async || {
                        let state = store.index_state();
                        state.failing.len() == 1 && state.failing[0].tries >= 6
                    })
                    .await;
                    let state = store.index_state();
                    assert_eq!(state.failing[0].id, "lunch");
                    assert!(state.failing[0].error.contains("answered 500"), "{state:?}");
                    assert_eq!((state.pending, state.complete), (1, Some(true)));
                    assert!(vectors(&node).complete().await.unwrap());
                    assert_eq!(found(&store).await[0].0, "deploy", "still found by meaning");
                },
            )
            .await;
        }

        /// A vellev node that accepts connections and never answers: an
        /// artifact search waits its deadline, not the client's tries of
        /// half a minute each, and goes by keyword; after three such in a
        /// row the searches do not wait at all.
        #[tokio::test(flavor = "multi_thread")]
        async fn a_search_does_not_wait_out_a_node_that_never_answers() {
            let silent = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}", silent.local_addr().unwrap());
            workforce_postgres::test_support::with_test_pool(
                KnowledgeStore::build_pool,
                |pool| async move {
                    let hung = VellevArtifacts::new(&url, "key", "workforce").unwrap();
                    let deadline = Duration::from_millis(400);
                    let store = KnowledgeStore::waiting(
                        pool.clone(),
                        Some(Arc::new(ArtifactIndex::new(hung))),
                        deadline,
                        Duration::from_secs(30),
                    )
                    .await
                    .unwrap();
                    write_notes(&store).await;
                    // Each of three in a row waits its deadline and goes
                    // by keyword; then the node is left alone.
                    for _ in 0..3 {
                        let began = std::time::Instant::now();
                        let by_keyword = searched(&store, "menu").await;
                        let took = began.elapsed();
                        assert_eq!(by_keyword.len(), 1, "by keyword");
                        assert!(took >= deadline && took < deadline * 4, "{took:?}");
                    }
                    assert_eq!(store.index_state().search_breaker, Some("open"));
                    let began = std::time::Instant::now();
                    assert_eq!(searched(&store, "menu").await.len(), 1);
                    assert!(began.elapsed() < deadline / 2, "{:?}", began.elapsed());
                },
            )
            .await;
            drop(silent);
        }

        /// A collection that cannot be used when the hub boots is used
        /// once an operator has put it right, without a restart: what was
        /// written meanwhile is indexed, and the index filled.
        #[tokio::test(flavor = "multi_thread")]
        async fn a_collection_refused_at_boot_is_used_once_put_right() {
            let Some(node) = Node::embedding().await else {
                return;
            };
            workforce_postgres::test_support::with_test_pool(
                KnowledgeStore::build_pool,
                |pool| async move {
                    // As vellev's importer leaves a class copied from
                    // Weaviate with its vectors.
                    let mut imported = super::super::vellev::artifacts_collection().unwrap();
                    imported.embedding = vellev_core::Embedding {
                        provider: vellev_core::EmbeddingProvider::None,
                        model: "weaviate:text2vec-transformers".into(),
                        dimensions: 384,
                        query_prefix: None,
                        document_prefix: None,
                        properties: Vec::new(),
                    };
                    let tenant = node.tenant();
                    let records = tenant.collection(&imported.name);
                    records.create(&imported).await.unwrap();

                    let pace = Pace {
                        recheck_after: Duration::from_millis(100),
                        ..Pace::default()
                    };
                    let refused =
                        VellevArtifacts::paced(&node.url, &node.key, &node.tenant, pace).unwrap();
                    assert!(refused.usable().await.is_err());
                    let store = store_over(&pool, refused).await;
                    store.keep_index_complete(QUICK);
                    write_notes(&store).await;
                    until("said", async || store.index_state().last_error.is_some()).await;
                    let state = store.index_state();
                    assert_eq!(state.complete, None);
                    assert!(
                        state.last_error.unwrap().contains("copied from Weaviate"),
                        "why, for an operator"
                    );
                    assert!(found(&store).await.is_empty(), "by keyword meanwhile");

                    // The operator drops it, as the refusal says.
                    records.drop().await.unwrap();
                    until("used", async || {
                        let state = store.index_state();
                        state.complete == Some(true) && state.pending == 0
                    })
                    .await;
                    assert_eq!(vectors(&node).count().await.unwrap(), 3);
                    assert_eq!(found(&store).await[0].0, "deploy");
                    // The round that filled it may yet say that its work
                    // took none, before the drop was seen; the next has
                    // nothing to say.
                    until("nothing to say", async || {
                        store.index_state().last_error.is_none()
                    })
                    .await;
                },
            )
            .await;
        }
    }
}
